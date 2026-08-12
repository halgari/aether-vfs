//! Director daemon library: discovery, gRPC service, session registry, and
//! helpers shared by the `vfs` CLI and integration tests.

pub mod discovery;
pub mod registry;
pub mod service;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use tonic::transport::Channel;
use tonic::transport::Server;

use vfs_control::pb::director_client::DirectorClient;

pub use discovery::{default_discovery_path, read_discovery, write_discovery, Discovery};
pub use registry::SessionRegistry;
pub use service::DirectorService;

/// Bind address used when the caller does not pin one (ephemeral port).
pub const DEFAULT_BIND: &str = "127.0.0.1:0";

/// Connect to an already-running daemon at `endpoint` (`host:port`).
pub async fn connect(endpoint: &str) -> Result<DirectorClient<Channel>, String> {
    let uri = if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        endpoint.to_string()
    } else {
        format!("http://{endpoint}")
    };
    DirectorClient::connect(uri)
        .await
        .map_err(|e| format!("connect {endpoint}: {e}"))
}

/// Try discovery file → Health; on failure auto-spawn `vfs daemon` and retry.
///
/// `endpoint_override` skips discovery (and auto-spawn) and connects directly.
pub async fn connect_or_spawn(
    endpoint_override: Option<&str>,
    discovery_path: Option<PathBuf>,
    daemon_exe: PathBuf,
) -> Result<DirectorClient<Channel>, String> {
    if let Some(ep) = endpoint_override {
        return connect(ep).await;
    }

    let path = discovery_path.unwrap_or_else(default_discovery_path);
    if let Ok(d) = read_discovery(&path) {
        if process_alive(d.pid) {
            if let Ok(mut c) = connect(&d.endpoint).await {
                if health_ok(&mut c).await {
                    return Ok(c);
                }
            }
        }
    }

    spawn_daemon(&daemon_exe, &path)?;
    wait_for_daemon(&path, Duration::from_secs(15)).await
}

async fn health_ok(client: &mut DirectorClient<Channel>) -> bool {
    client
        .health(vfs_control::pb::HealthReq {})
        .await
        .is_ok()
}

fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        // Prefer OpenProcess over shelling out to tasklist (slow, locale-dependent).
        // SAFETY: OpenProcess is well-defined for any pid; we only check nullity.
        unsafe {
            type HANDLE = *mut core::ffi::c_void;
            extern "system" {
                fn OpenProcess(access: u32, inherit: i32, pid: u32) -> HANDLE;
                fn CloseHandle(h: HANDLE) -> i32;
            }
            const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return false;
            }
            CloseHandle(h);
            true
        }
    }
    #[cfg(not(windows))]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
}

fn spawn_daemon(exe: &PathBuf, discovery_path: &std::path::Path) -> Result<(), String> {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon");
    cmd.env("VFS_DISCOVERY_PATH", discovery_path);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        const DETACHED_PROCESS: u32 = 0x00000008;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }
    cmd.spawn()
        .map_err(|e| format!("spawn daemon {}: {e}", exe.display()))?;
    Ok(())
}

async fn wait_for_daemon(
    discovery_path: &std::path::Path,
    timeout: Duration,
) -> Result<DirectorClient<Channel>, String> {
    let deadline = std::time::Instant::now() + timeout;
    let mut last_err = "daemon did not become ready".to_string();
    while std::time::Instant::now() < deadline {
        if let Ok(d) = read_discovery(discovery_path) {
            match connect(&d.endpoint).await {
                Ok(mut c) => {
                    if health_ok(&mut c).await {
                        return Ok(c);
                    }
                    last_err = format!("health failed at {}", d.endpoint);
                }
                Err(e) => last_err = e,
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(last_err)
}

/// Run the tonic director server until shutdown (or forever).
///
/// Binds `bind` (use `127.0.0.1:0` for ephemeral), writes the discovery file,
/// then serves. Removes the discovery file on clean exit when it still names
/// this process.
pub async fn serve_daemon(
    bind: SocketAddr,
    discovery_path: PathBuf,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let local = listener.local_addr()?;
    let endpoint = format!("{}:{}", local.ip(), local.port());
    let pid = std::process::id();
    write_discovery(
        &discovery_path,
        &Discovery {
            endpoint: endpoint.clone(),
            pid,
        },
    )?;
    eprintln!("vfs daemon listening on {endpoint} (pid {pid})");
    eprintln!("discovery file: {}", discovery_path.display());

    let registry = SessionRegistry::new();
    let svc = DirectorService::new(registry);
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    let result = Server::builder()
        .add_service(vfs_control::pb::director_server::DirectorServer::new(svc))
        .serve_with_incoming(incoming)
        .await;

    // Best-effort cleanup if we still own the discovery file.
    if let Ok(d) = read_discovery(&discovery_path) {
        if d.pid == pid {
            let _ = std::fs::remove_file(&discovery_path);
        }
    }
    result.map_err(|e| e.into())
}

/// Parse `TYPE:PATH@MOUNT#LAYER` CLI source flags.
pub fn parse_source_flag(s: &str) -> Result<vfs_control::SourceEntry, String> {
    let (ty, rest) = s
        .split_once(':')
        .ok_or_else(|| format!("source flag needs TYPE:PATH…, got {s:?}"))?;
    let ty = ty.to_ascii_lowercase();

    let (path_and_mount, layer) = if let Some((left, right)) = rest.rsplit_once('#') {
        let layer: i32 = right
            .parse()
            .map_err(|_| format!("bad layer in source flag: {right:?}"))?;
        (left, layer)
    } else {
        (rest, 0)
    };

    let (path, mount) = if let Some((p, m)) = path_and_mount.rsplit_once('@') {
        (p.to_string(), m.to_string())
    } else {
        (path_and_mount.to_string(), "/".to_string())
    };

    if path.is_empty() {
        return Err(format!("empty path in source flag: {s:?}"));
    }

    let spec = match ty.as_str() {
        "disk" => vfs_control::SourceSpec::Disk { path },
        "zip" => vfs_control::SourceSpec::Zip { path },
        "http" => vfs_control::SourceSpec::Http { url: path },
        "remote" => vfs_control::SourceSpec::Remote { endpoint: path },
        other => return Err(format!("unknown source type {other:?}")),
    };

    Ok(vfs_control::SourceEntry {
        spec,
        mount,
        layer,
    })
}

/// Drive CreateSession → AddSource* → optional Launch from a [`SessionConfig`].
pub async fn apply_session_config(
    client: &mut DirectorClient<Channel>,
    cfg: &vfs_control::SessionConfig,
) -> Result<(String, Option<i32>), String> {
    use vfs_control::pb::{
        launch_event, source_spec, AddSourceReq, CreateSessionReq, DiskSource, HttpSource,
        LaunchReq, RemoteSource, SourceSpec as PbSource, ZipSource,
    };

    let name = cfg.session.name.clone().unwrap_or_default();
    let session = client
        .create_session(CreateSessionReq { name })
        .await
        .map_err(|e| format!("CreateSession: {e}"))?
        .into_inner();
    let session_id = session.id.clone();

    let mut sources = cfg.sources.clone();
    sources.sort_by_key(|s| s.layer);

    for entry in &sources {
        let kind = match &entry.spec {
            vfs_control::SourceSpec::Disk { path } => {
                source_spec::Kind::Disk(DiskSource { path: path.clone() })
            }
            vfs_control::SourceSpec::Zip { path } => {
                source_spec::Kind::Zip(ZipSource { path: path.clone() })
            }
            vfs_control::SourceSpec::Http { url } => {
                source_spec::Kind::Http(HttpSource { url: url.clone() })
            }
            vfs_control::SourceSpec::Remote { endpoint } => {
                source_spec::Kind::Remote(RemoteSource {
                    endpoint: endpoint.clone(),
                })
            }
        };
        client
            .add_source(AddSourceReq {
                session_id: session_id.clone(),
                source: Some(PbSource { kind: Some(kind) }),
                mount: entry.mount.clone(),
                layer: entry.layer,
            })
            .await
            .map_err(|e| format!("AddSource: {e}"))?;
    }

    let mut exit_code = None;
    if let Some(launch) = &cfg.launch {
        let mut stream = client
            .launch(LaunchReq {
                session_id: session_id.clone(),
                exec: launch.exec.clone(),
                args: launch.args.clone(),
                wait: launch.wait,
                hollow_pe: launch.hollow_pe,
                env: launch.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            })
            .await
            .map_err(|e| format!("Launch: {e}"))?
            .into_inner();

        while let Some(ev) = stream
            .message()
            .await
            .map_err(|e| format!("Launch stream: {e}"))?
        {
            match ev.event {
                Some(launch_event::Event::Started(s)) => {
                    eprintln!("started pid={}", s.pid);
                }
                Some(launch_event::Event::Exited(x)) => {
                    eprintln!("exited code={}", x.code);
                    exit_code = Some(x.code);
                }
                Some(launch_event::Event::Log(l)) => {
                    eprintln!("log: {}", l.line);
                }
                None => {}
            }
        }
    }

    Ok((session_id, exit_code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_source_flag_disk_windows_path() {
        let e = parse_source_flag(r#"disk:C:\mods\SkyUI@/#20"#).unwrap();
        assert_eq!(
            e.spec,
            vfs_control::SourceSpec::Disk {
                path: r#"C:\mods\SkyUI"#.into()
            }
        );
        assert_eq!(e.mount, "/");
        assert_eq!(e.layer, 20);
    }

    #[test]
    fn parse_source_flag_defaults() {
        let e = parse_source_flag("zip:C:/base.zip").unwrap();
        assert_eq!(
            e.spec,
            vfs_control::SourceSpec::Zip {
                path: "C:/base.zip".into()
            }
        );
        assert_eq!(e.mount, "/");
        assert_eq!(e.layer, 0);
    }

    #[test]
    fn parse_source_flag_mount_without_layer() {
        let e = parse_source_flag("disk:C:/mods@/Data").unwrap();
        assert_eq!(e.mount, "/Data");
        assert_eq!(e.layer, 0);
    }

    #[test]
    fn parse_source_flag_rejects_unknown_type() {
        assert!(parse_source_flag("blob:C:/x").is_err());
    }
}

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
pub use registry::{SessionRegistry, StageLaunchOpts};
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
            #[allow(clippy::upper_case_acronyms)] // mirrors the Win32 name
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

/// Parse `TYPE:PATH@MOUNT` CLI source flags.
///
/// Precedence among several `--source` flags is declaration order (later
/// flag wins on a shared path) — the same flat-list sugar
/// [`vfs_control::config`] documents for `[[source]]`, not a per-flag numeric
/// layer. Every entry this builds targets root `0`; the CLI has no syntax
/// yet for naming a non-default root (config files do, via `[[root]]` +
/// `root =`).
pub fn parse_source_flag(s: &str) -> Result<vfs_control::SourceEntry, String> {
    let (ty, rest) = s
        .split_once(':')
        .ok_or_else(|| format!("source flag needs TYPE:PATH…, got {s:?}"))?;
    let ty = ty.to_ascii_lowercase();

    let (path, mount) = if let Some((p, m)) = rest.rsplit_once('@') {
        (p.to_string(), m.to_string())
    } else {
        (rest.to_string(), "/".to_string())
    };

    if path.is_empty() {
        return Err(format!("empty path in source flag: {s:?}"));
    }
    // The old syntax was `TYPE:PATH@MOUNT#LAYER`; `#LAYER` was removed when
    // `layer` left the config (precedence is now flag order). `rsplit_once('@')`
    // has no idea that suffix is gone, so a leftover `#20` from a command
    // line nobody updated silently becomes part of `mount` instead of being
    // stripped — the source then mounts at a mangled, unreachable prefix
    // (`registry.rs`'s `is_root` check sees `"/#20"`, not `"/"`) and the
    // session starts cleanly while serving nothing where the caller expected
    // root content. Reject it loudly instead.
    if let Some((_, suffix)) = mount.split_once('#') {
        return Err(format!(
            "source flag {s:?}: the '#{suffix}' layer suffix no longer exists \
             (precedence is now --source flag order) — use TYPE:PATH@MOUNT"
        ));
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
        root: 0,
        // `--source` declares content. A write layer is a different fact
        // about a session (where its writes land), so it gets its own flag
        // rather than a magic suffix on this one — see `--write-layer`.
        write_layer: false,
    })
}

/// The `--write-layer DIR` flag as a config entry: root 0's writable upper.
///
/// A separate flag rather than a `--source` spelling because it is a
/// different fact — `--source` says what the session *serves*, this says
/// where its writes *land*, seeded from whatever the sources hold. Always a
/// disk directory (nothing else in this workspace is writable), always root
/// 0 (the CLI has no syntax for naming another root), always mounted at the
/// root (the upper covers the whole root by construction).
pub fn write_layer_flag_entry(path: &str) -> vfs_control::SourceEntry {
    vfs_control::SourceEntry {
        spec: vfs_control::SourceSpec::Disk {
            path: path.to_string(),
        },
        mount: "/".to_string(),
        root: 0,
        write_layer: true,
    }
}

/// Drive CreateSession → AddSource* → optional Launch from a [`SessionConfig`].
///
/// Every source is sent, not only root 0's: `AddSourceReq` carries a `root`
/// field (stage 2b), and `Director` now holds one provider per root, so
/// there is no longer a reason to drop anything here. A config declaring
/// roots or sources inconsistently (an undeclared root, a duplicate
/// `[[root]]` id) is rejected up front by
/// [`vfs_control::SessionConfig::validate_roots`] rather than silently
/// serving whatever subset of itself happens to be addressable — the same
/// failure shape the old root-0-only filter had.
pub async fn apply_session_config(
    client: &mut DirectorClient<Channel>,
    cfg: &vfs_control::SessionConfig,
) -> Result<(String, Option<i32>), String> {
    use vfs_control::pb::{
        launch_event, source_spec, AddSourceReq, CreateSessionReq, DeclareRootReq, DiskSource,
        HttpSource, LaunchReq, RemoteSource, SourceSpec as PbSource, ZipSource,
    };

    cfg.validate_roots()?;

    let name = cfg.session.name.clone().unwrap_or_default();
    let session = client
        .create_session(CreateSessionReq { name })
        .await
        .map_err(|e| format!("CreateSession: {e}"))?
        .into_inner();
    let session_id = session.id.clone();

    // Declare each root's host directory before any source is added, so the
    // shim is told about every root the config names — not only about the
    // providers behind them. Mounting a provider on root 1 while never
    // declaring where root 1 *is* produces a session that looks configured
    // and serves nothing under that root, which is the silent-partial shape
    // this project keeps rediscovering.
    //
    // Root 0 is skipped deliberately: its host directory is the daemon's own
    // `Session.root`, chosen at `CreateSession` and already published. A
    // config's `[[root]] path` for id 0 is descriptive (which tree the author
    // means) and cannot repoint the directory the daemon created — declaring
    // it would be rejected by the daemon anyway.
    for root in cfg.roots.iter().filter(|r| r.id != 0) {
        client
            .declare_root(DeclareRootReq {
                session_id: session_id.clone(),
                root: root.id,
                path: root.path.clone(),
            })
            .await
            .map_err(|e| format!("DeclareRoot {} ({}): {e}", root.id, root.name))?;
    }

    // `AddSourceReq.layer` is the RPC's own precedence field, unrelated to
    // config's (now-removed) `SourceEntry.layer` — it orders sources *within
    // their own root* (declaration order is the flat-list sugar's rule), so
    // the position in `cfg.sources` becomes the numeric layer directly, with
    // no re-sort. Layer numbers are not compared across roots, so two
    // sources targeting different roots sharing a `layer` value is not a
    // conflict.
    for (layer, entry) in cfg.sources.iter().enumerate() {
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
                layer: layer as i32,
                root: entry.root,
                write_layer: entry.write_layer,
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

    /// `--source` and `--write-layer` must not be confusable: a source is
    /// content, a write layer is where writes land. The flag that reaches the
    /// daemon has to carry `write_layer: true`, or the CLI silently declares
    /// one more mod directory instead of a copy-up target.
    #[test]
    fn write_layer_flag_declares_a_write_layer_not_a_source() {
        let e = write_layer_flag_entry(r#"C:\mods\overwrite"#);
        assert!(e.write_layer, "the --write-layer flag must set the flag");
        assert_eq!(
            e.spec,
            vfs_control::SourceSpec::Disk {
                path: r#"C:\mods\overwrite"#.into()
            }
        );
        assert_eq!(e.mount, "/", "a write layer covers the whole root");
        assert_eq!(e.root, 0);
        // The contrast that makes the assertion above mean something.
        assert!(!parse_source_flag(r#"disk:C:\mods\overwrite"#).unwrap().write_layer);
        // …and the config it produces is one the daemon will accept.
        vfs_control::SessionConfig {
            sources: vec![e],
            ..Default::default()
        }
        .validate_roots()
        .expect("the flag must produce a config that validates");
    }

    #[test]
    fn parse_source_flag_disk_windows_path() {
        let e = parse_source_flag(r#"disk:C:\mods\SkyUI@/"#).unwrap();
        assert_eq!(
            e.spec,
            vfs_control::SourceSpec::Disk {
                path: r#"C:\mods\SkyUI"#.into()
            }
        );
        assert_eq!(e.mount, "/");
        assert_eq!(e.root, 0);
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
        assert_eq!(e.root, 0);
    }

    #[test]
    fn parse_source_flag_mount_without_at() {
        let e = parse_source_flag("disk:C:/mods@/Data").unwrap();
        assert_eq!(e.mount, "/Data");
        assert_eq!(e.root, 0);
    }

    #[test]
    fn parse_source_flag_rejects_unknown_type() {
        assert!(parse_source_flag("blob:C:/x").is_err());
    }

    /// The pre-2b syntax was `TYPE:PATH@MOUNT#LAYER`. Task 2 dropped `layer`
    /// from config but `parse_source_flag`'s `rsplit_once('@')` has no idea
    /// the `#LAYER` suffix is gone, so a stale command line's `#20` used to
    /// become part of `mount` silently — `registry::add_source`'s `is_root`
    /// check then sees `"/#20"`, not `"/"`, and the source mounts at an
    /// unreachable prefix instead of the root the caller intended, with the
    /// session starting cleanly and serving nothing where expected. This
    /// must be a loud parse error instead.
    #[test]
    fn parse_source_flag_rejects_the_removed_layer_suffix() {
        let err = parse_source_flag(r#"disk:C:\mods\SkyUI@/#20"#).unwrap_err();
        assert!(
            err.contains('#') && err.contains("layer"),
            "error should name the removed '#LAYER' syntax: {err}"
        );
    }
}

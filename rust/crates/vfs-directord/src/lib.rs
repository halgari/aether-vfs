//! Director daemon library: discovery, gRPC service, session registry, and
//! helpers shared by the `vfs` CLI and integration tests.

pub mod discovery;
pub mod registry;
pub mod service;

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tonic::transport::Channel;
use tonic::transport::Server;

use vfs_control::pb::director_client::DirectorClient;
use vfs_embed::{CloseOutcome, Storage, StorageConfig};

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
    client.health(vfs_control::pb::HealthReq {}).await.is_ok()
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
    // Its own process group, so Ctrl-C in the terminal that ran the CLI does
    // not reach the daemon it spawned — the unix counterpart of
    // CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS above.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
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

/// The daemon's storage directory: `flag` (`--storage-dir`), else
/// `VFS_STORAGE_DIR`, else `<home>/storage` where the aether-vfs home is
/// `VFS_HOME`, else `$XDG_DATA_HOME/aether-vfs`, else
/// `$HOME/.local/share/aether-vfs`, else `%LOCALAPPDATA%\aether-vfs` — the
/// same order the Proton runtimes' home resolves in. `None` when nothing
/// names a home at all.
///
/// `env` is the environment lookup (`std::env::var_os` in the daemon), so the
/// order can be tested without touching the process environment.
pub fn storage_dir_from(
    flag: Option<&Path>,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    if let Some(dir) = flag {
        return Some(dir.to_path_buf());
    }
    let set = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(dir) = set(vfs_env::STORAGE_DIR) {
        return Some(dir);
    }
    let home = set(vfs_env::HOME)
        .or_else(|| set("XDG_DATA_HOME").map(|x| x.join("aether-vfs")))
        .or_else(|| set("HOME").map(|h| h.join(".local/share/aether-vfs")))
        .or_else(|| set("LOCALAPPDATA").map(|l| l.join("aether-vfs")))?;
    Some(home.join("storage"))
}

/// Open the daemon's storage at `dir`, with `cache_max_gib` as the cache
/// budget (the default otherwise), and print what reconciliation repaired.
///
/// A failure — above all another daemon holding the directory — is an error
/// naming the directory and how to choose another.
pub fn open_daemon_storage(dir: &Path, cache_max_gib: Option<u64>) -> Result<Arc<Storage>, String> {
    let mut cfg = StorageConfig::default();
    if let Some(gib) = cache_max_gib {
        cfg.cache_max_bytes = gib.saturating_mul(1 << 30);
    }
    let storage = Storage::open(dir, cfg).map_err(|e| {
        format!(
            "cannot open storage at {}: {e} (is another vfs daemon using it? choose \
             another directory with --storage-dir or {})",
            dir.display(),
            vfs_env::STORAGE_DIR
        )
    })?;
    let r = storage.last_reconcile();
    let repaired = r.emptied_files.len()
        + r.zero_filled_files.len()
        + r.resized_rows.len()
        + r.orphans_deleted as usize
        + r.cache_rows_dropped as usize;
    if repaired > 0 {
        eprintln!(
            "vfs daemon: storage at {} was repaired at open:",
            dir.display()
        );
        for (layer, path) in &r.emptied_files {
            eprintln!("  layer {layer:?}: {path} lost its data and is now empty");
        }
        for (layer, path) in &r.zero_filled_files {
            eprintln!("  layer {layer:?}: {path} had missing blocks, now zeros");
        }
        for (layer, path) in &r.resized_rows {
            eprintln!("  layer {layer:?}: {path} length corrected to the store's");
        }
        if r.orphans_deleted > 0 {
            eprintln!("  {} unreferenced store file(s) deleted", r.orphans_deleted);
        }
        if r.cache_rows_dropped > 0 {
            eprintln!(
                "  {} cache entr(ies) without data dropped",
                r.cache_rows_dropped
            );
        }
    }
    Ok(storage)
}

/// Run the tonic director server until SIGINT/SIGTERM (Ctrl-C on Windows),
/// then drain it — see [`serve_daemon_until`]. `storage` is the daemon's
/// already-opened storage (see [`open_daemon_storage`]).
pub async fn serve_daemon(
    bind: SocketAddr,
    discovery_path: PathBuf,
    storage: Arc<Storage>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    serve_daemon_until(
        bind,
        discovery_path,
        SessionRegistry::with_storage(storage),
        shutdown_signal(),
    )
    .await
}

/// Resolves on the first SIGINT or SIGTERM (unix) / Ctrl-C (Windows). If the
/// handler cannot be installed it never resolves — the daemon then runs until
/// killed, as it always did.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) {
            (Ok(mut term), Ok(mut int)) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = int.recv() => {}
                }
            }
            _ => std::future::pending::<()>().await,
        }
    }
    #[cfg(not(unix))]
    {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Run the tonic director server over `registry` until `shutdown` resolves.
///
/// Binds `bind` (use `127.0.0.1:0` for ephemeral), writes the discovery file,
/// then serves. On shutdown it stops accepting, lets in-flight requests
/// finish (a waited launch included — its session is dropped when it
/// returns), **drains the registry** ([`SessionRegistry::teardown_all`], so
/// every session's `Drop` runs: on Linux that removes root links and deletes
/// anonymous Wine prefixes, which a killed process would leak), then **closes
/// the registry's storage**, and finally removes the discovery file if it
/// still names this process.
///
/// The storage closes after the drain because every session holds it: a
/// layer write layer is a provider holding the `Storage`, and so is a cached
/// source. `registry` is consumed so this function's reference is the one
/// the close can take; a caller that keeps a clone of the registry (or a
/// launch still running in a torn-down session) keeps the store open and the
/// directory locked, which is reported rather than silently left.
pub async fn serve_daemon_until(
    bind: SocketAddr,
    discovery_path: PathBuf,
    registry: SessionRegistry,
    shutdown: impl std::future::Future<Output = ()> + Send,
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

    let svc = DirectorService::new(registry.clone());
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    let result = Server::builder()
        .add_service(vfs_control::pb::director_server::DirectorServer::new(svc))
        .serve_with_incoming_shutdown(incoming, shutdown)
        .await;

    // Dropping a session can block for seconds (stopping a prefix's
    // `wineserver`, deleting a ~600 MB prefix), so off the async executor.
    let storage = registry.storage().cloned();
    let drained = tokio::task::spawn_blocking(move || {
        let n = registry.teardown_all();
        // The registry's own reference goes with it, before the close below.
        drop(registry);
        n
    })
    .await
    .unwrap_or(0);
    if drained > 0 {
        eprintln!("vfs daemon: tore down {drained} session(s) on shutdown");
    }
    if let Some(storage) = storage {
        match tokio::task::spawn_blocking(move || storage.close()).await {
            Ok(Ok(CloseOutcome::Released)) => {}
            Ok(Ok(CloseOutcome::StillShared { refs })) => eprintln!(
                "vfs daemon: storage flushed, but {} other reference(s) are still alive \
                 (a launch still running?); its directory stays locked until they drop",
                refs - 1
            ),
            Ok(Err(e)) => eprintln!("vfs daemon: closing storage failed: {e}"),
            Err(e) => eprintln!("vfs daemon: closing storage panicked: {e}"),
        }
    }

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
/// layer. Every entry this builds targets root `0`: `--root` declares where
/// roots are, but a `--source` cannot yet name a non-default root (config
/// files can, via `[[root]]` + `root =`).
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
        if ty == "layer" {
            return Err(format!("source flag {s:?}: `layer:` needs a layer name"));
        }
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
        "layer" => vfs_control::SourceSpec::Layer { name: path },
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
        cache_key: None,
    })
}

/// The `--write-layer DIR|layer:NAME` flag as a config entry: root 0's
/// writable upper.
///
/// A separate flag rather than a `--source` spelling because it is a
/// different fact — `--source` says what the session *serves*, this says
/// where its writes *land*, seeded from whatever the sources hold. Either a
/// disk directory, or `layer:NAME`, a named persistent layer in the daemon's
/// storage (an empty name is refused). Always root 0 (the CLI has no syntax
/// for naming another root), always mounted at the root (the upper covers
/// the whole root by construction).
pub fn write_layer_flag_entry(path: &str) -> Result<vfs_control::SourceEntry, String> {
    let spec = match path.strip_prefix("layer:") {
        Some("") => {
            return Err(format!(
                "--write-layer {path:?}: `layer:` needs a layer name (layer:NAME)"
            ))
        }
        Some(name) => vfs_control::SourceSpec::Layer {
            name: name.to_string(),
        },
        None => vfs_control::SourceSpec::Disk {
            path: path.to_string(),
        },
    };
    Ok(vfs_control::SourceEntry {
        spec,
        mount: "/".to_string(),
        root: 0,
        write_layer: true,
        cache_key: None,
    })
}

/// Parse one `--root ID=NAME=LOCATION` flag into a `[[root]]` entry.
///
/// Only the first two `=` split, so a location may itself contain one. The
/// name is what a launch path spells as `{NAME}\…`; the location is where
/// the program sees the root (a `C:\…` path inside the prefix on Linux).
pub fn parse_root_flag(s: &str) -> Result<vfs_control::RootEntry, String> {
    let bad = || format!("--root expects ID=NAME=LOCATION, got `{s}`");
    let mut parts = s.splitn(3, '=');
    let (Some(id), Some(name), Some(location)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(bad());
    };
    let id: u32 = id.trim().parse().map_err(|_| bad())?;
    if name.is_empty() || location.is_empty() {
        return Err(bad());
    }
    Ok(vfs_control::RootEntry {
        id,
        name: name.to_string(),
        path: location.to_string(),
    })
}

/// Every `--root` flag as `[[root]]` entries. Once any root is declared the
/// config has a `[[root]]` table, and `--source`/`--write-layer` target root
/// 0 — so root 0 must be among them, or the config names a root it never
/// declares.
pub fn root_flag_entries(flags: &[String]) -> Result<Vec<vfs_control::RootEntry>, String> {
    let roots = flags
        .iter()
        .map(|f| parse_root_flag(f))
        .collect::<Result<Vec<_>, _>>()?;
    if !roots.is_empty() && !roots.iter().any(|r| r.id == 0) {
        return Err(
            "--root: declare root 0 too; --source and --write-layer target root 0".to_string(),
        );
    }
    Ok(roots)
}

/// Drive CreateSession → DeclareRoot* → AddSource* → optional Launch from a [`SessionConfig`].
///
/// Every source is sent, not only root 0's: `AddSourceReq` carries a `root`
/// field (stage 2b), and `Director` now holds one provider per root, so
/// there is no longer a reason to drop anything here. A config declaring
/// roots or sources inconsistently (an undeclared root, a duplicate
/// `[[root]]` id) is rejected up front by
/// [`vfs_control::SessionConfig::validate_roots`] rather than silently
/// serving whatever subset of itself happens to be addressable — the same
/// failure shape the old root-0-only filter had.
///
/// **All or nothing.** Any failure after `CreateSession` — a refused root, a
/// source that cannot be built, a launch that cannot start — tears the new
/// session down before the original error is returned. A half-applied
/// session left live would hold the config's name, so the corrected retry
/// would be refused as a duplicate (or, before that rule, become a second
/// session of the same name).
pub async fn apply_session_config(
    client: &mut DirectorClient<Channel>,
    cfg: &vfs_control::SessionConfig,
) -> Result<(String, Option<i32>), String> {
    use vfs_control::pb::{CreateSessionReq, TeardownReq};

    cfg.validate_roots()?;

    let name = cfg.session.name.clone().unwrap_or_default();
    let session = client
        .create_session(CreateSessionReq { name })
        .await
        .map_err(|e| format!("CreateSession: {e}"))?
        .into_inner();
    let session_id = session.id.clone();

    match configure_session(client, &session_id, cfg).await {
        Ok(exit_code) => Ok((session_id, exit_code)),
        Err(e) => {
            // Best effort: the error being reported is the original one.
            let _ = client
                .teardown_session(TeardownReq {
                    session_id: session_id.clone(),
                })
                .await;
            Err(e)
        }
    }
}

/// [`apply_session_config`] after `CreateSession`: declare the roots, add the
/// sources, run the optional launch.
async fn configure_session(
    client: &mut DirectorClient<Channel>,
    session_id: &str,
    cfg: &vfs_control::SessionConfig,
) -> Result<Option<i32>, String> {
    use vfs_control::pb::{
        source_spec, AddSourceReq, DeclareRootReq, DiskSource, HttpSource, LayerSource,
        RemoteSource, SourceSpec as PbSource, ZipSource,
    };
    let session_id = session_id.to_string();

    // Declare each root's location before any source is added, so the
    // shim is told about every root the config names — not only about the
    // providers behind them. Mounting a provider on root 1 while never
    // declaring where root 1 *is* produces a session that looks configured
    // and serves nothing under that root, which is the silent-partial shape
    // this project keeps rediscovering.
    //
    // Root 0 is declared too: its `[[root]] path` is root 0's location (where
    // the program sees it) and replaces the daemon's default for the session.
    // Every root's `name` travels with it, so a launch can spell a path as
    // `{Name}\…`.
    for root in &cfg.roots {
        client
            .declare_root(DeclareRootReq {
                session_id: session_id.clone(),
                root: root.id,
                path: root.path.clone(),
                name: root.name.clone(),
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
            vfs_control::SourceSpec::Layer { name } => {
                source_spec::Kind::Layer(LayerSource { name: name.clone() })
            }
            // No `source.proto` `Kind::Memory` exists yet — the daemon's
            // gRPC control plane has no wire shape for an inline name→bytes
            // map. `vfs_source::build_provider` and `vfs_embed::MemoryProvider`
            // serve this locally; wiring it over gRPC is a proto change, not
            // this task's scope.
            vfs_control::SourceSpec::Memory { .. } => {
                return Err(format!(
                    "source at root {}: a `memory` source has no gRPC wire representation yet \
                     — the directord control plane cannot carry it. Build it locally instead, \
                     via `vfs_source::build_provider` or `vfs_embed::MemoryProvider`.",
                    entry.root
                ));
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
                cache_key: entry.cache_key.clone().unwrap_or_default(),
            })
            .await
            .map_err(|e| format!("AddSource: {e}"))?;
    }

    match &cfg.launch {
        Some(launch) => run_launch(client, &session_id, launch).await,
        None => Ok(None),
    }
}

/// The one-shot `vfs launch`: apply `cfg` (a fresh session plus its launch)
/// and, when that launch was **waited**, tear the session down again once it
/// has returned — success or failure. Nothing can launch into a one-shot
/// session afterwards, so keeping it would only hold its name and, on Linux,
/// its anonymous Wine prefix (~600 MB) until the daemon exits. A `--no-wait`
/// launch leaves it up: the program may still be running in it.
///
/// A failure before or during the launch already tears the session down
/// ([`apply_session_config`]); a teardown that fails here is reported on
/// stderr, and the launch's own outcome is still returned.
pub async fn launch_one_shot(
    client: &mut DirectorClient<Channel>,
    cfg: &vfs_control::SessionConfig,
) -> Result<(String, Option<i32>), String> {
    let (session_id, exit) = apply_session_config(client, cfg).await?;
    if cfg.launch.as_ref().is_some_and(|l| l.wait) {
        if let Err(e) = client
            .teardown_session(vfs_control::pb::TeardownReq {
                session_id: session_id.clone(),
            })
            .await
        {
            eprintln!(
                "vfs: session {session_id} was not torn down: {}",
                e.message()
            );
        }
    }
    Ok((session_id, exit))
}

/// Launch `launch` in the live session `session_id` (an id or a session
/// name) and follow its event stream to the end, reporting each event on
/// stderr. Returns the child's exit code when the stream carried one.
///
/// Shared by `vfs up`/`vfs launch` (through [`apply_session_config`]) and
/// `vfs exec`, which launches into a session that is already up.
pub async fn run_launch(
    client: &mut DirectorClient<Channel>,
    session_id: &str,
    launch: &vfs_control::LaunchConfig,
) -> Result<Option<i32>, String> {
    use vfs_control::pb::{launch_event, LaunchReq};

    let mut stream = client
        .launch(LaunchReq {
            session_id: session_id.to_string(),
            exec: launch.exec.clone(),
            args: launch.args.clone(),
            wait: launch.wait,
            env: launch
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        })
        .await
        .map_err(|e| format!("Launch: {e}"))?
        .into_inner();

    let mut exit_code = None;
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
    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The daemon's shutdown path (what SIGTERM/SIGINT drive in
    /// `serve_daemon`), driven here by a oneshot: once `shutdown` resolves,
    /// the server stops, every live session is torn down — so its `Drop`
    /// runs, which on Linux is what deletes an anonymous prefix — and the
    /// discovery file naming this process is removed.
    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_drains_the_registry_and_removes_the_discovery_file() {
        let dir = tempfile::tempdir().unwrap();
        let discovery = dir.path().join("discovery.json");
        let registry = SessionRegistry::new();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve_daemon_until(
            DEFAULT_BIND.parse().unwrap(),
            discovery.clone(),
            registry.clone(),
            async {
                let _ = stopped.await;
            },
        ));

        let mut client = wait_for_daemon(&discovery, Duration::from_secs(10))
            .await
            .expect("the daemon comes up");
        for name in ["drain-named", ""] {
            client
                .create_session(vfs_control::pb::CreateSessionReq { name: name.into() })
                .await
                .expect("create session");
        }
        assert_eq!(registry.len(), 2);
        drop(client);

        stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .expect("the daemon stops once shutdown resolves")
            .unwrap()
            .expect("a clean shutdown");
        assert!(registry.is_empty(), "shutdown must tear every session down");
        assert!(
            !discovery.exists(),
            "shutdown must remove its discovery file"
        );
    }

    /// `--source` and `--write-layer` must not be confusable: a source is
    /// content, a write layer is where writes land. The flag that reaches the
    /// daemon has to carry `write_layer: true`, or the CLI silently declares
    /// one more mod directory instead of a copy-up target.
    #[test]
    fn write_layer_flag_declares_a_write_layer_not_a_source() {
        let e = write_layer_flag_entry(r#"C:\mods\overwrite"#).unwrap();
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
        assert!(
            !parse_source_flag(r#"disk:C:\mods\overwrite"#)
                .unwrap()
                .write_layer
        );
        // …and the config it produces is one the daemon will accept.
        vfs_control::SessionConfig {
            sources: vec![e],
            ..Default::default()
        }
        .validate_roots()
        .expect("the flag must produce a config that validates");
    }

    #[test]
    fn parse_root_flag_splits_id_name_location() {
        let r = parse_root_flag(r"0=Games=C:\Games\Fixture").unwrap();
        assert_eq!(r.id, 0);
        assert_eq!(r.name, "Games");
        assert_eq!(r.path, r"C:\Games\Fixture");
        // Only the first two `=` split: a location may contain one.
        assert_eq!(parse_root_flag("1=Docs=C:\\a=b").unwrap().path, "C:\\a=b");
    }

    #[test]
    fn parse_root_flag_rejects_malformed() {
        for bad in [
            "Games=C:\\x",
            "x=Games=C:\\x",
            "0=C:\\x",
            "0==C:\\x",
            "0=Games=",
        ] {
            let e = parse_root_flag(bad).unwrap_err();
            assert!(e.contains("--root") && e.contains(bad), "{bad}: {e}");
        }
    }

    #[test]
    fn root_flags_must_declare_root_zero() {
        assert!(root_flag_entries(&[]).unwrap().is_empty());
        let e = root_flag_entries(&["1=Docs=C:\\docs".to_string()]).unwrap_err();
        assert_eq!(
            e,
            "--root: declare root 0 too; --source and --write-layer target root 0"
        );
        let ok = root_flag_entries(&[
            "0=Games=C:\\games".to_string(),
            "1=Docs=C:\\docs".to_string(),
        ])
        .unwrap();
        assert_eq!(ok.iter().map(|r| r.id).collect::<Vec<_>>(), [0, 1]);
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
    #[test]
    fn parse_source_flag_layer() {
        let e = parse_source_flag("layer:prof").unwrap();
        assert_eq!(
            e.spec,
            vfs_control::SourceSpec::Layer {
                name: "prof".into()
            }
        );
        assert_eq!(e.mount, "/");
        assert!(!e.write_layer);
    }

    #[test]
    fn write_layer_flag_entry_layer_prefix_builds_a_layer() {
        let e = write_layer_flag_entry("layer:prof").unwrap();
        assert_eq!(
            e.spec,
            vfs_control::SourceSpec::Layer {
                name: "prof".into()
            }
        );
        assert!(e.write_layer);
        let d = write_layer_flag_entry("C:/scratch").unwrap();
        assert!(matches!(d.spec, vfs_control::SourceSpec::Disk { .. }));
    }

    /// `layer:` with no name would reach the daemon as a layer called "",
    /// which `Storage::layer` would happily create. Refused at parse time.
    #[test]
    fn an_empty_layer_name_is_refused_by_both_flags() {
        for flag in ["layer:", "layer:@/"] {
            let e = parse_source_flag(flag).unwrap_err();
            assert!(e.contains("layer name"), "{flag}: {e}");
        }
        let e = write_layer_flag_entry("layer:").unwrap_err();
        assert!(
            e.contains("layer name") && e.contains("--write-layer"),
            "{e}"
        );
    }

    /// `--storage-dir`, then `VFS_STORAGE_DIR`, then `<VFS_HOME>/storage`
    /// with VFS_HOME resolved as the Proton runtimes' home is.
    #[test]
    fn storage_dir_resolution_order() {
        use std::collections::HashMap;
        use std::ffi::OsString;
        let env = |pairs: &[(&str, &str)]| {
            let m: HashMap<String, OsString> = pairs
                .iter()
                .map(|(k, v)| (k.to_string(), OsString::from(v)))
                .collect();
            move |k: &str| m.get(k).cloned()
        };
        let all = env(&[
            (vfs_env::STORAGE_DIR, "/env/storage"),
            (vfs_env::HOME, "/vfs-home"),
            ("XDG_DATA_HOME", "/xdg"),
            ("HOME", "/home/u"),
            ("LOCALAPPDATA", "/lad"),
        ]);
        let flag = PathBuf::from("/flag/storage");
        assert_eq!(storage_dir_from(Some(&flag), &all), Some(flag.clone()));
        assert_eq!(
            storage_dir_from(None, &all),
            Some(PathBuf::from("/env/storage"))
        );
        let p = |s: &str| Some(PathBuf::from(s));
        assert_eq!(
            storage_dir_from(
                None,
                &env(&[(vfs_env::HOME, "/vfs-home"), ("HOME", "/home/u")])
            ),
            p("/vfs-home").map(|h| h.join("storage"))
        );
        assert_eq!(
            storage_dir_from(
                None,
                &env(&[("XDG_DATA_HOME", "/xdg"), ("HOME", "/home/u")])
            ),
            p("/xdg").map(|h| h.join("aether-vfs").join("storage"))
        );
        assert_eq!(
            storage_dir_from(None, &env(&[("HOME", "/home/u"), ("LOCALAPPDATA", "/lad")])),
            p("/home/u").map(|h| h.join(".local/share/aether-vfs").join("storage"))
        );
        assert_eq!(
            storage_dir_from(None, &env(&[("LOCALAPPDATA", "/lad")])),
            p("/lad").map(|h| h.join("aether-vfs").join("storage"))
        );
        assert_eq!(storage_dir_from(None, &env(&[])), None);
    }

    /// A storage directory another daemon holds is refused with an error that
    /// names the directory and the way to pick another.
    #[test]
    fn a_locked_storage_dir_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let held = open_daemon_storage(dir.path(), None).expect("first open");
        let e = open_daemon_storage(dir.path(), None)
            .err()
            .expect("second open refused");
        assert!(
            e.contains(&dir.path().display().to_string()) && e.contains("--storage-dir"),
            "{e}"
        );
        drop(held);
    }

    /// The shutdown drain closes the storage after the sessions (which hold
    /// its layer providers) are gone, so the directory is free again.
    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_closes_the_storage_after_the_drain() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("storage");
        let discovery = dir.path().join("discovery.json");
        let registry = SessionRegistry::with_storage(
            open_daemon_storage(&store_dir, None).expect("open storage"),
        );
        let s = registry.create("drain-layer".into()).unwrap();
        registry.set_layer_write_layer(&s.id, 0, "drained").unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve_daemon_until(
            DEFAULT_BIND.parse().unwrap(),
            discovery.clone(),
            registry,
            async {
                let _ = stopped.await;
            },
        ));
        wait_for_daemon(&discovery, Duration::from_secs(10))
            .await
            .expect("the daemon comes up");
        stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(20), server)
            .await
            .expect("the daemon stops")
            .unwrap()
            .expect("a clean shutdown");
        let reopened = open_daemon_storage(&store_dir, None)
            .expect("the drain must have released the storage directory");
        assert_eq!(reopened.layers().unwrap()[0].name, "drained");
    }
}

//! Serving the gRPC director: the daemon loop and its shutdown path.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use tonic::transport::Server;

use vfs_embed::{CloseOutcome, Storage};

use crate::discovery::{Discovery, read_discovery, write_discovery};
use crate::service::DirectorService;
use crate::sessions::SessionRegistry;

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
        use tokio::signal::unix::{SignalKind, signal};
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::wait_for_daemon;
    use crate::open_daemon_storage;
    use crate::{DEFAULT_BIND, SessionRegistry};
    use std::time::Duration;

    /// The daemon's shutdown path (what SIGTERM/SIGINT drive in
    /// `serve_daemon`), driven here by a oneshot: once `shutdown` resolves,
    /// the server stops, every live session is torn down — so its `Drop`
    /// runs, which on Linux is what deletes an anonymous prefix — and the
    /// discovery file naming this process is removed.
    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_drains_the_registry_and_removes_the_discovery_file() {
        let dir = vfs_testkit::tempdir().unwrap();
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

    /// The shutdown drain closes the storage after the sessions (which hold
    /// its layer providers) are gone, so the directory is free again.
    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_closes_the_storage_after_the_drain() {
        let dir = vfs_testkit::tempdir().unwrap();
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

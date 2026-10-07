//! Applying a session config to a running daemon, and launching into it.

use tonic::transport::Channel;

use vfs_control::pb::director_client::DirectorClient;

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
        AddSourceReq, DeclareRootReq, DiskSource, HttpSource, LayerSource, RemoteSource,
        SourceSpec as PbSource, ZipSource, source_spec,
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
    use vfs_control::pb::{LaunchReq, launch_event};

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

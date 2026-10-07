//! Daemon -> CreateSession -> AddSource -> Launch, driven through a
//! `scenario.toml`, asserting that a fixture reads and writes virtual bytes
//! through the ring; plus the rooted launch of a graph-only image.

// Every test here injects a real Windows process, so on other hosts the helpers are unused.
#![cfg_attr(not(windows), allow(dead_code, unused_imports))]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::net::TcpListener;
use tonic::transport::Server;
use vfs_control::pb::director_server::DirectorServer;
use vfs_control::SessionConfig;
use vfs_directord::{apply_session_config, connect, DirectorService, SessionRegistry};

mod support;
use support::{artifacts::*, launch::*};

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn scenario_toml_disk_source_fixture_read() {
    let _guard = LAUNCH_LOCK.lock().await;
    ensure_inject_artifacts();

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr: SocketAddr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    let registry = SessionRegistry::new();
    let svc = DirectorService::new(registry);
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(DirectorServer::new(svc))
            .serve_with_incoming(incoming)
            .await
    });

    // Give the server a moment to accept.
    tokio::time::sleep(Duration::from_millis(20)).await;

    let content_dir = vfs_testkit::tempdir().expect("tempdir");
    std::fs::write(content_dir.path().join("hello.txt"), b"hello").unwrap();

    let fixture = locate_artifact("vfs-fixture-read.exe");
    // Root is chosen by the daemon per session; we learn it after CreateSession
    // and rewrite the fixture path. apply_session_config needs the env path
    // up front — so we do the RPC steps manually after create to inject the root.

    let mut client = connect(&format!("{addr}")).await.expect("connect");

    // Health
    let h = client
        .health(vfs_control::pb::HealthReq {})
        .await
        .expect("health")
        .into_inner();
    assert_eq!(h.sessions, 0);

    // Prefer the shared config path for sources + launch meta; fill fixture env
    // after we know the session root.
    let toml = format!(
        r#"
[session]
name = "m0-e2e"

[[source]]
type  = "disk"
path  = {}
mount = "/"

[launch]
exec      = {}
wait      = true
"#,
        toml_string(&content_dir.path().to_string_lossy()),
        toml_string(&fixture.to_string_lossy()),
    );

    let mut cfg: SessionConfig = toml::from_str(&toml).expect("parse scenario");
    // Create session first to learn root, then set env and continue via helper-equivalent.

    let session = client
        .create_session(vfs_control::pb::CreateSessionReq {
            name: "m0-e2e".into(),
        })
        .await
        .expect("CreateSession")
        .into_inner();
    assert!(!session.id.is_empty());
    assert!(!session.root.is_empty());

    let fixture_path = PathBuf::from(&session.root).join("hello.txt");
    if let Some(launch) = cfg.launch.as_mut() {
        launch.env.insert(
            "VFS_FIXTURE_PATH".into(),
            fixture_path.to_string_lossy().into_owned(),
        );
        launch.env.insert("VFS_FIXTURE_EXPECT".into(), "5".into());
    }

    // Add sources + launch using the same helper by rebuilding a config that
    // already has the session's sources/launch, but CreateSession was already
    // called — call apply pieces manually.
    use vfs_control::pb::{
        source_spec, AddSourceReq, DiskSource, LaunchReq, SourceSpec as PbSource,
    };

    // Precedence is declaration order (the flat-list sugar), so position in
    // `cfg.sources` becomes the RPC's numeric layer directly — see the
    // comment in `apply_session_config` for why this is no longer a config
    // field.
    for (layer, entry) in cfg.sources.iter().enumerate() {
        let path = match &entry.spec {
            vfs_control::SourceSpec::Disk { path } => path.clone(),
            other => panic!("expected disk source, got {other:?}"),
        };
        client
            .add_source(AddSourceReq {
                session_id: session.id.clone(),
                source: Some(PbSource {
                    kind: Some(source_spec::Kind::Disk(DiskSource { path })),
                }),
                mount: entry.mount.clone(),
                layer: layer as i32,
                root: entry.root,
                write_layer: entry.write_layer,
                cache_key: String::new(),
            })
            .await
            .expect("AddSource");
    }

    let launch = cfg.launch.unwrap();
    let mut stream = client
        .launch(LaunchReq {
            session_id: session.id.clone(),
            exec: launch.exec,
            args: launch.args,
            wait: launch.wait,
            env: launch.env.into_iter().collect(),
        })
        .await
        .expect("Launch")
        .into_inner();

    let mut exit_code = None;
    drain_launch_events(&mut stream, "log", &mut exit_code).await;

    assert_eq!(
        exit_code,
        Some(0),
        "fixture should exit 0 after reading 5 bytes via injected shim"
    );

    client
        .teardown_session(vfs_control::pb::TeardownReq {
            session_id: session.id,
        })
        .await
        .expect("teardown");

    server.abort();
}

/// The decisive end-to-end assertion for the whole write-path phase: a
/// launched, injected process's writes/rename/delete land through the real
/// director + `DiskProvider`, not the shim-local overlay bypass.
///
/// Task 6 found that `try_fuse_create`/`open_write` never forwarded the NT
/// create-disposition into the ring's `OP_OPEN`, so a brand-new file always
/// got `ST_NOT_FOUND` from the director and silently fell through to
/// `<session-base>/overlay/` — a shim-local directory the director never
/// reads from. A test that only checks the bytes exist somewhere would pass
/// with that bypass fully intact; the decisive check is that overlay/ stays
/// EMPTY, proving the write actually crossed the ring instead.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn scenario_toml_disk_source_fixture_writepath() {
    let _guard = LAUNCH_LOCK.lock().await;
    ensure_inject_artifacts();

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr: SocketAddr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    let registry = SessionRegistry::new();
    let svc = DirectorService::new(registry);
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(DirectorServer::new(svc))
            .serve_with_incoming(incoming)
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Empty scratch directory: the DiskProvider's backing store. Nothing
    // pre-exists, so every byte the assertions find had to be written by the
    // launched fixture through the real provider graph.
    let content_dir = vfs_testkit::tempdir().expect("tempdir");

    // A separate directory (not the DiskProvider's backing store) for the
    // shim's own stats report, so `VFS_SHIM_STATS_LOG`'s temp/rename dance
    // never shows up as a stray entry when the write-path assertions below
    // list content_dir.
    let stats_dir = vfs_testkit::tempdir().expect("stats tempdir");
    let stats_log = stats_dir.path().join("shim-stats.log");

    let fixture = locate_artifact("vfs-fixture-writepath.exe");
    let mut client = connect(&format!("{addr}")).await.expect("connect");

    let session = client
        .create_session(vfs_control::pb::CreateSessionReq {
            name: "m0-e2e-writepath".into(),
        })
        .await
        .expect("CreateSession")
        .into_inner();
    assert!(!session.id.is_empty());
    assert!(!session.root.is_empty());

    use vfs_control::pb::{source_spec, AddSourceReq, DiskSource, LaunchReq, SourceSpec as PbSource};

    client
        .add_source(AddSourceReq {
            session_id: session.id.clone(),
            source: Some(PbSource {
                kind: Some(source_spec::Kind::Disk(DiskSource {
                    path: content_dir.path().to_string_lossy().into_owned(),
                })),
            }),
            mount: "/".into(),
            layer: 0,
            root: 0,
            write_layer: false,
            cache_key: String::new(),
        })
        .await
        .expect("AddSource");

    let mut env = std::collections::HashMap::new();
    env.insert(
        "VFS_SHIM_STATS_LOG".to_string(),
        stats_log.to_string_lossy().into_owned(),
    );
    // This fixture's whole create/write/append/rename/delete sequence
    // completes in well under 250ms (measured ~70-90ms wall clock for the
    // entire launch), faster than the reporter's default tick — confirmed by
    // running with the default: the report file never appeared at all, not
    // even a partial one, because the process exits (and takes its reporter
    // thread down with it — nothing flushes on exit) before the first tick.
    // A short override makes the snapshot land reliably without changing the
    // default cadence any real, longer-lived launch gets.
    env.insert("VFS_SHIM_STATS_INTERVAL_MS".to_string(), "5".to_string());

    // Baseline for the director's open count, taken right before the launch
    // that will drive real opens through `OP_OPEN`/`record_open`. `io_stats`
    // is a process-wide static (not per-`DirectorService`), and this test
    // binary runs other tests concurrently, so a delta — not an absolute
    // reading — is what isolates this launch's own opens (same convention
    // `io_stats::tests::open_totals_counts_ok_and_err_separately` uses).
    let (opens_ok_before, opens_err_before) = vfs_director::io_stats::open_totals();

    let mut stream = client
        .launch(LaunchReq {
            session_id: session.id.clone(),
            exec: fixture.to_string_lossy().into_owned(),
            args: Vec::new(),
            wait: true,
            env,
        })
        .await
        .expect("Launch")
        .into_inner();

    let mut exit_code = None;
    drain_launch_events(&mut stream, "log", &mut exit_code).await;

    assert_eq!(
        exit_code,
        Some(0),
        "fixture should exit 0 after create/write/append/rename/delete all round-trip \
         through the injected shim"
    );

    // The process (and its reporter thread) has exited by now, `wait: true`
    // having blocked until it did, so the director's open count for this
    // launch is stable to read.
    let (opens_ok_after, opens_err_after) = vfs_director::io_stats::open_totals();
    // The reconciliation target is the director's *total* arrived-open
    // count, not `opens_ok` alone: this fixture's own error probes (a
    // failing re-open of a renamed-away name, a failing re-open of a
    // deleted file, a failing second `CREATE_NEW`) are real opens that
    // reach the director and get a legitimate negative answer back — the
    // shim correctly records each as `Routed` regardless of that answer
    // (see `support`'s module doc for the verification behind this).
    let opens_ok_delta =
        (opens_ok_after - opens_ok_before) + (opens_err_after - opens_err_before);

    // The decisive assertions, on the filesystem, not on a director query:
    // bytes must be in the DiskProvider's backing directory, and NOTHING may
    // have landed in the shim-local overlay fallback.
    let renamed = content_dir.path().join("renamed-probe.txt");
    assert!(
        renamed.is_file(),
        "renamed file must be in the DiskProvider backing dir at {renamed:?}"
    );
    assert_eq!(
        std::fs::read(&renamed).expect("read renamed-probe.txt"),
        b"helloworld",
        "renamed file must carry the create+append bytes through to the backing dir"
    );
    // The original name must be gone from the backing dir too (real rename,
    // not a copy left behind).
    assert!(
        !content_dir.path().join("write-probe.txt").exists(),
        "write-probe.txt must not remain in the backing dir after rename"
    );
    // delete-probe.txt was created then deleted by the fixture; it must never
    // have been left behind in the backing dir.
    assert!(
        !content_dir.path().join("delete-probe.txt").exists(),
        "delete-probe.txt must not remain in the backing dir after delete"
    );

    // session.root is "<session-base>/root"; overlay is its sibling.
    let overlay = PathBuf::from(&session.root)
        .parent()
        .expect("session.root has a parent")
        .join("overlay");
    assert_overlay_empty(&overlay);

    // The reconciliation: gate 1's whole point. Every open the shim believed
    // it routed to the director must actually have arrived there — checked
    // by comparing the shim's own `routed` count (from its report) against
    // the director's total arrived-open delta captured above. A mismatch is
    // a live bypass (see `support::assert_reconciled`'s doc for why, and for
    // what this comparison deliberately leaves out — directory creates).
    let recon = support::assert_reconciled(&stats_log, opens_ok_delta);
    assert!(
        recon.routed > 0,
        "expected at least one `routed` under-root open outcome in the shim \
         report at {stats_log:?}, got 0 (report contents: {:?})",
        std::fs::read_to_string(&stats_log)
    );
    // Gate 4, Task 5: for the *write-fallback* class this now IS a claim that
    // fall-through is zero. Every write this fixture makes is answered by the
    // director, and a write it would not answer is a hard NT failure rather
    // than a diversion — so any count here is a bypass that came back. (The
    // other fall-through classes are still nonzero by design; gates 5 and up
    // own those.)
    assert_eq!(
        recon.write_fallback(),
        0,
        "under-root writes fell through to the shim-local overlay {} time(s) — the bypass \
         this gate closes. Report: {:?}",
        recon.write_fallback(),
        std::fs::read_to_string(&stats_log)
    );
    // Only that the section this test depends on genuinely exists and parsed,
    // rather than the reconciliation above having passed vacuously on an
    // empty/missing report.
    assert!(
        recon.outcomes_section_found,
        "expected the shim report at {stats_log:?} to contain an \
         \"under-root open outcomes:\" section once the launch completed; \
         got: {:?}",
        std::fs::read_to_string(&stats_log)
    );

    client
        .teardown_session(vfs_control::pb::TeardownReq {
            session_id: session.id,
        })
        .await
        .expect("teardown");

    server.abort();
}

/// `read_dir(...).unwrap_or_default()` turns a wrong or missing overlay path
/// into a silent empty-Vec pass — the exact failure mode this assertion
/// exists to catch would then go undetected. Assert the directory actually
/// exists first, so a path mistake surfaces as a panic instead of a false
/// green.
fn assert_overlay_empty(overlay: &std::path::Path) {
    assert!(
        overlay.is_dir(),
        "expected the shim-local overlay directory to exist at {overlay:?} \
         (Session::launch creates it unconditionally) — a missing/wrong path \
         here would make the emptiness check below pass vacuously"
    );
    let overlay_entries: Vec<_> = std::fs::read_dir(overlay)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(
        overlay_entries.is_empty(),
        "nothing should land in the shim-local overlay fallback \
         ({overlay:?} contains {overlay_entries:?}) — this is the bypass the phase closes. \
         Full tree: {:#?}",
        // The top-level listing alone cannot distinguish the cases that
        // matter: an empty root-scoped directory the shim created and did not
        // use (`Overlay::ensure_parent` runs before a decision that may not
        // need it), real diverted bytes underneath it, or — the one actually
        // observed — a previous process's litter inherited at the same path
        // (see `SessionRegistry::create`, which now clears the base
        // directory). All three fail this assertion, deliberately; a failure
        // that does not say which one costs an investigation.
        overlay_tree(overlay)
    );
}

/// Every path under `dir`, files and directories alike, for a failure
/// message that has to explain *what* landed in the overlay.
fn overlay_tree(dir: &std::path::Path) -> Vec<PathBuf> {
    fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            let is_dir = p.is_dir();
            out.push(p.clone());
            if is_dir {
                walk(&p, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out
}

/// Two root-mounted sources — the case Fix 1 exists for. A single mounted
/// source never constructs a `LayeredProvider` at all (`stack_layers`
/// returns a lone layer as-is), so the headline "writes cross the ring, not
/// the overlay bypass" assertion above cannot see LayeredProvider's `open()`
/// hard-rejecting `OPEN_WRITE` while its `capabilities()` advertised
/// `ReadWrite` — exactly the shape `SessionRegistry::add_source` builds for
/// any session with two or more root-mounted sources, the ordinary modded-
/// game case. `layer = 1` mounts on top of `layer = 0`, and a layered stack
/// routes every write to the topmost child that declares `ReadWrite` — both
/// `DiskProvider`s here do — so the written bytes must land in the top
/// content directory, not the bottom one and not the overlay fallback.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn scenario_toml_two_disk_sources_fixture_writepath() {
    let _guard = LAUNCH_LOCK.lock().await;
    ensure_inject_artifacts();

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr: SocketAddr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    let registry = SessionRegistry::new();
    let svc = DirectorService::new(registry);
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(DirectorServer::new(svc))
            .serve_with_incoming(incoming)
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Two empty scratch directories, mounted as two separate root sources.
    let bottom_dir = vfs_testkit::tempdir().expect("tempdir bottom");
    let top_dir = vfs_testkit::tempdir().expect("tempdir top");

    // Separate from both source directories, same reasoning as the
    // single-source test above: keeps the shim's `VFS_SHIM_STATS_LOG`
    // temp/rename dance out of the bottom/top directory listings the
    // assertions below rely on being exactly the fixture's own writes.
    let stats_dir = vfs_testkit::tempdir().expect("stats tempdir");
    let stats_log = stats_dir.path().join("shim-stats.log");

    let fixture = locate_artifact("vfs-fixture-writepath.exe");
    let mut client = connect(&format!("{addr}")).await.expect("connect");

    let session = client
        .create_session(vfs_control::pb::CreateSessionReq {
            name: "m0-e2e-writepath-two-sources".into(),
        })
        .await
        .expect("CreateSession")
        .into_inner();
    assert!(!session.id.is_empty());
    assert!(!session.root.is_empty());

    use vfs_control::pb::{source_spec, AddSourceReq, DiskSource, LaunchReq, SourceSpec as PbSource};

    client
        .add_source(AddSourceReq {
            session_id: session.id.clone(),
            source: Some(PbSource {
                kind: Some(source_spec::Kind::Disk(DiskSource {
                    path: bottom_dir.path().to_string_lossy().into_owned(),
                })),
            }),
            mount: "/".into(),
            layer: 0,
            root: 0,
            write_layer: false,
            cache_key: String::new(),
        })
        .await
        .expect("AddSource bottom");

    client
        .add_source(AddSourceReq {
            session_id: session.id.clone(),
            source: Some(PbSource {
                kind: Some(source_spec::Kind::Disk(DiskSource {
                    path: top_dir.path().to_string_lossy().into_owned(),
                })),
            }),
            mount: "/".into(),
            layer: 1,
            root: 0,
            write_layer: false,
            cache_key: String::new(),
        })
        .await
        .expect("AddSource top");

    let mut env = std::collections::HashMap::new();
    env.insert(
        "VFS_SHIM_STATS_LOG".to_string(),
        stats_log.to_string_lossy().into_owned(),
    );
    // Same rationale as the single-source test: this fixture's full
    // create/write/append/rename/delete sequence finishes well under the
    // reporter's default 250ms tick, so a short-lived process here would
    // otherwise exit before the reporter thread ever writes a report at all.
    env.insert("VFS_SHIM_STATS_INTERVAL_MS".to_string(), "5".to_string());

    // See the single-source test above for why this is a delta rather than
    // an absolute reading: `io_stats` is a process-wide static shared by
    // every test in this binary.
    let (opens_ok_before, opens_err_before) = vfs_director::io_stats::open_totals();

    let mut stream = client
        .launch(LaunchReq {
            session_id: session.id.clone(),
            exec: fixture.to_string_lossy().into_owned(),
            args: Vec::new(),
            wait: true,
            env,
        })
        .await
        .expect("Launch")
        .into_inner();

    let mut exit_code = None;
    drain_launch_events(&mut stream, "log", &mut exit_code).await;

    assert_eq!(
        exit_code,
        Some(0),
        "fixture should exit 0 after create/write/append/rename/delete all round-trip \
         through the injected shim over a two-source (LayeredProvider) stack"
    );

    let (opens_ok_after, opens_err_after) = vfs_director::io_stats::open_totals();
    // See the single-source test above: the target is the director's total
    // arrived-open count, `opens_ok + opens_err`, not `opens_ok` alone —
    // this fixture's own error probes are real, correctly-`Routed` opens
    // that the director legitimately answered with an error.
    let opens_ok_delta =
        (opens_ok_after - opens_ok_before) + (opens_err_after - opens_err_before);

    // The decisive assertions: bytes in the TOP source's backing directory
    // (the layer writes route to), nothing in the bottom source, and nothing
    // in the shim-local overlay fallback.
    let renamed = top_dir.path().join("renamed-probe.txt");
    assert!(
        renamed.is_file(),
        "renamed file must be in the topmost DiskProvider's backing dir at {renamed:?}"
    );
    assert_eq!(
        std::fs::read(&renamed).expect("read renamed-probe.txt"),
        b"helloworld",
        "renamed file must carry the create+append bytes through to the top backing dir"
    );
    assert!(
        !top_dir.path().join("write-probe.txt").exists(),
        "write-probe.txt must not remain in the top backing dir after rename"
    );
    assert!(
        !top_dir.path().join("delete-probe.txt").exists(),
        "delete-probe.txt must not remain in the top backing dir after delete"
    );

    // Nothing should have landed in the bottom (non-target) layer at all.
    let bottom_entries: Vec<_> = std::fs::read_dir(bottom_dir.path())
        .expect("read bottom dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    assert!(
        bottom_entries.is_empty(),
        "the bottom layer must stay untouched when the top layer is writable \
         (found {bottom_entries:?})"
    );

    let overlay = PathBuf::from(&session.root)
        .parent()
        .expect("session.root has a parent")
        .join("overlay");
    assert_overlay_empty(&overlay);

    // The decisive reconciliation for the case this test exists to cover:
    // `LayeredProvider` is exactly where an earlier phase found the bypass
    // reintroduced, so this is the case where shim-`routed` vs.
    // director-`opens_ok` drifting apart would matter most.
    let recon = support::assert_reconciled(&stats_log, opens_ok_delta);
    assert!(
        recon.routed > 0,
        "expected at least one `routed` under-root open outcome in the shim \
         report at {stats_log:?}, got 0 (report contents: {:?})",
        std::fs::read_to_string(&stats_log)
    );
    // Same claim as the single-source write-path test, over a
    // `LayeredProvider`: no under-root write left the director's answer
    // behind (gate 4, Task 5).
    assert_eq!(
        recon.write_fallback(),
        0,
        "under-root writes fell through to the shim-local overlay {} time(s) — the bypass \
         this gate closes. Report: {:?}",
        recon.write_fallback(),
        std::fs::read_to_string(&stats_log)
    );
    assert!(
        recon.outcomes_section_found,
        "expected the shim report at {stats_log:?} to contain an \
         \"under-root open outcomes:\" section once the launch completed; \
         got: {:?}",
        std::fs::read_to_string(&stats_log)
    );

    client
        .teardown_session(vfs_control::pb::TeardownReq {
            session_id: session.id,
        })
        .await
        .expect("teardown");

    server.abort();
}

/// **Copy-on-write over a layered base, live, on the daemon surface**
/// (gate 4, Task 6b).
///
/// The two scenarios above prove writes cross the ring; neither proves a
/// write can be *seeded* from content nothing writable holds. Everything that
/// does is unit-level, and in a shape the daemon never builds — so this test
/// exists for what had never run live:
///
/// - **A layered base under the overlay.** `skyrim-live` hands `compose_root`
///   four sibling `""` mounts; `SessionRegistry` collapses a root's sources
///   with `stack_layers` and hands it *one* `""` mount. That distinction only
///   exists with **more than one** root-mounted source: `stack_layers` returns
///   a lone layer unwrapped, so a single-source session builds no
///   `LayeredProvider` at all. Hence three sources here — archive, then two
///   mod directories — which is also what an ordinary modded game looks like.
/// - **The registry's own source wrapping under the overlay.** This bullet
///   was written when every registry source went through the old `vfs-cache` crate's
///   `CachingProvider`, so a copy-up seeded through the block cache had never
///   happened live. That crate is gone: `vfs-storage` caches only slow,
///   immutable sources, so the archive and directories here are mounted as
///   they are, exactly as the daemon mounts them.
/// - **The whole declaration path**, from `AddSourceReq.write_layer` to a real
///   `fopen(…, "r+b")` in an injected process.
///
/// Two paths are edited in place, and the pair is the point:
///
/// - `Data/x.esp` lives **only in the archive**, at the bottom of the stack,
///   so copy-up has to read down through every layer to seed it.
/// - `Data/mod.esp` lives in **both** mod directories with different bytes, so
///   copy-up has to seed from the layer that *wins* precedence.
///
/// The two halves of this test catch different things, and both are needed.
/// The fixture catches a refused open, a blank destination and a truncating
/// copy-up, inside the process, with distinct exit codes. It cannot catch a
/// seed from the *wrong layer* — it reads back through the same handle it
/// wrote, so wrong-but-consistent bytes look fine to it (verified: reversing
/// the layer order leaves the fixture exiting 0). That is what the host-side
/// byte assertion below is for.
///
/// Neither source directory may be written to — the concern that makes the
/// write layer a separate declaration in the first place is a game's writes
/// scattering into whichever mod folder was declared last, and this asserts
/// on disk that they did not.
///
/// The rest of the fixture (create, append, rename, delete) runs too, so this
/// is also the first live exercise of those through an `OverlayProvider`
/// upper rather than a bare writable mount.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn scenario_layered_sources_with_write_layer_copy_up_in_place() {
    let _guard = LAUNCH_LOCK.lock().await;
    ensure_inject_artifacts();

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr: SocketAddr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    let registry = SessionRegistry::new();
    let svc = DirectorService::new(registry);
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(DirectorServer::new(svc))
            .serve_with_incoming(incoming)
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Layer 0, the read-only archive: one Stored zip entry, spelled as an
    // archive spells it. Its bytes are known exactly, so "the archive is
    // untouched" is a byte comparison rather than a timestamp check.
    const ZIP_ENTRY: &str = "Data/x.esp";
    const ORIGINAL: &[u8] = b"ORIGINAL-ESP-BYTES";
    let content_dir = vfs_testkit::tempdir().expect("tempdir");
    let zip = content_dir.path().join("content.zip");
    support::write_stored_zip(&zip, ZIP_ENTRY, ORIGINAL);
    let zip_before = std::fs::read(&zip).expect("read zip");

    // Layers 10 and 20: two mod directories holding the *same* path with
    // different bytes, which is what makes the stack's precedence observable.
    // Equal lengths so a copy-up that seeded from the loser cannot pass by
    // accident of size, and both long enough for the fixture's offset-9 edit.
    const MOD_ENTRY: &str = "Data/mod.esp";
    const MOD_BOTTOM: &[u8] = b"BOTTOM-MOD-BYTES!!";
    const MOD_TOP: &[u8] = b"TOP-MOD-BYTES-WIN!";
    let mods_bottom = vfs_testkit::tempdir().expect("mods-bottom tempdir");
    let mods_top = vfs_testkit::tempdir().expect("mods-top tempdir");
    for (dir, bytes) in [(&mods_bottom, MOD_BOTTOM), (&mods_top, MOD_TOP)] {
        std::fs::create_dir_all(dir.path().join("Data")).expect("mkdir Data");
        std::fs::write(dir.path().join(MOD_ENTRY), bytes).expect("write mod entry");
    }

    // The declared write layer: a directory of the user's choosing, not the
    // session's own overlay. Left uncreated on purpose — an overwrite folder
    // need not exist before the first write.
    let overwrite_parent = vfs_testkit::tempdir().expect("overwrite tempdir");
    let overwrite = overwrite_parent.path().join("overwrite");

    let stats_dir = vfs_testkit::tempdir().expect("stats tempdir");
    let stats_log = stats_dir.path().join("shim-stats.log");

    let fixture = locate_artifact("vfs-fixture-writepath.exe");
    let mut client = connect(&format!("{addr}")).await.expect("connect");

    let session = client
        .create_session(vfs_control::pb::CreateSessionReq {
            name: "m0-e2e-cow-write-layer".into(),
        })
        .await
        .expect("CreateSession")
        .into_inner();

    use vfs_control::pb::{
        source_spec, AddSourceReq, DiskSource, LaunchReq, SourceSpec as PbSource,
        ZipSource,
    };

    client
        .add_source(AddSourceReq {
            session_id: session.id.clone(),
            source: Some(PbSource {
                kind: Some(source_spec::Kind::Zip(ZipSource {
                    path: zip.to_string_lossy().into_owned(),
                })),
            }),
            mount: "/".into(),
            layer: 0,
            root: 0,
            write_layer: false,
            cache_key: String::new(),
        })
        .await
        .expect("AddSource (archive)");

    // The two mod directories, as ordinary sources. These are what turn the
    // root's base into a real `LayeredProvider` — with the archive alone,
    // `stack_layers` would hand back the archive unwrapped and the layered
    // path this test exists for would never execute.
    for (layer, dir) in [(10, &mods_bottom), (20, &mods_top)] {
        client
            .add_source(AddSourceReq {
                session_id: session.id.clone(),
                source: Some(PbSource {
                    kind: Some(source_spec::Kind::Disk(DiskSource {
                        path: dir.path().to_string_lossy().into_owned(),
                    })),
                }),
                mount: "/".into(),
                layer,
                root: 0,
                write_layer: false,
                cache_key: String::new(),
            })
            .await
            .unwrap_or_else(|e| panic!("AddSource (mods layer {layer}): {e}"));
    }

    client
        .add_source(AddSourceReq {
            session_id: session.id.clone(),
            source: Some(PbSource {
                kind: Some(source_spec::Kind::Disk(DiskSource {
                    path: overwrite.to_string_lossy().into_owned(),
                })),
            }),
            mount: "/".into(),
            layer: 0,
            root: 0,
            write_layer: true,
            cache_key: String::new(),
        })
        .await
        .expect("AddSource (write layer)");

    let mut env = std::collections::HashMap::new();
    env.insert(
        "VFS_SHIM_STATS_LOG".to_string(),
        stats_log.to_string_lossy().into_owned(),
    );
    env.insert("VFS_SHIM_STATS_INTERVAL_MS".to_string(), "5".to_string());
    // The steps that need a write layer: the archive-only path (seeded from
    // the bottom of the stack) and the shadowed path (seeded from whichever
    // layer wins). Spelled exactly as the sources spell them; the shim folds
    // them on the way to the director.
    env.insert(
        "VFS_FIXTURE_COW_PATH".to_string(),
        format!("{ZIP_ENTRY};{MOD_ENTRY}"),
    );

    let (opens_ok_before, opens_err_before) = vfs_director::io_stats::open_totals();

    let mut stream = client
        .launch(LaunchReq {
            session_id: session.id.clone(),
            exec: fixture.to_string_lossy().into_owned(),
            args: Vec::new(),
            wait: true,
            env,
        })
        .await
        .expect("Launch")
        .into_inner();

    let mut exit_code = None;
    drain_launch_events(&mut stream, "log", &mut exit_code).await;
    assert_eq!(
        exit_code,
        Some(0),
        "the fixture exits 17 if the in-place open of archive content was refused, 18 if the \
         write layer produced a blank file instead of a seeded copy-up, 19 on the write and \
         20 if the readback lost the untouched bytes"
    );

    let (opens_ok_after, opens_err_after) = vfs_director::io_stats::open_totals();
    let opens_ok_delta =
        (opens_ok_after - opens_ok_before) + (opens_err_after - opens_err_before);

    // The copied-up file, on disk, in the directory the wire named — with the
    // edit applied and every other byte of the archive's content preserved.
    let mut expected = ORIGINAL.to_vec();
    expected[9..15].copy_from_slice(b"EDITED");
    let copied = overwrite.join("Data").join("x.esp");
    assert!(
        copied.is_file(),
        "copy-up must have materialised the archive entry in the declared write layer at \
         {copied:?} (write layer contains: {:?})",
        std::fs::read_dir(&overwrite).map(|rd| rd.flatten().map(|e| e.path()).collect::<Vec<_>>())
    );
    assert_eq!(
        std::fs::read(&copied).expect("read copied-up file"),
        expected,
        "the copied-up file must carry the in-place edit over seeded content"
    );

    // The shadowed path: copy-up had to seed from the layer that *wins*, not
    // merely from some layer that holds the path. Only the top mod
    // directory's bytes can produce this, and only through a real
    // `LayeredProvider` — which is what a second and third source build.
    let mut expected_mod = MOD_TOP.to_vec();
    expected_mod[9..15].copy_from_slice(b"EDITED");
    let copied_mod = overwrite.join("Data").join("mod.esp");
    assert_eq!(
        std::fs::read(&copied_mod).ok(),
        Some(expected_mod),
        "copy-up of a path two layers hold must seed from the winning layer ({}), not the \
         one beneath it ({})",
        String::from_utf8_lossy(MOD_TOP),
        String::from_utf8_lossy(MOD_BOTTOM)
    );

    // The archive is untouched, byte for byte. This is the assertion the
    // whole feature rests on: copy-on-write, not write-through.
    assert_eq!(
        std::fs::read(&zip).expect("read zip after"),
        zip_before,
        "the read-only archive was modified — copy-up wrote through instead of copying"
    );

    // Neither mod directory may be written to. Both are writable on disk, so
    // nothing but the overlay composition stops a write landing in one — and
    // "the game's saves ended up inside a mod folder" is the failure that
    // makes the write layer a separate declaration rather than an inference.
    for (label, dir, bytes) in [
        ("bottom", &mods_bottom, MOD_BOTTOM),
        ("top", &mods_top, MOD_TOP),
    ] {
        assert_eq!(
            std::fs::read(dir.path().join(MOD_ENTRY)).expect("read mod entry after"),
            bytes,
            "the {label} mod directory was edited in place instead of copied up"
        );
        assert!(
            !dir.path().join(ZIP_ENTRY).exists(),
            "the archive-only file was copied into the {label} mod directory"
        );
        assert!(
            !dir.path().join("renamed-probe.txt").exists(),
            "the fixture's own writes leaked into the {label} mod directory"
        );
    }

    // The fixture's ordinary writes land in the write layer too, since it is
    // the only writable member of this graph.
    let renamed = overwrite.join("renamed-probe.txt");
    assert!(
        renamed.is_file(),
        "the fixture's renamed file must be in the write layer at {renamed:?}"
    );
    assert_eq!(
        std::fs::read(&renamed).expect("read renamed-probe.txt"),
        b"helloworld"
    );
    assert!(
        !overwrite.join("write-probe.txt").exists(),
        "write-probe.txt must not remain after the rename"
    );

    // The bypass detector, unchanged: nothing may have landed in the
    // shim-local overlay, and every open the shim believed it routed must
    // have arrived at the director.
    let overlay = PathBuf::from(&session.root)
        .parent()
        .expect("session.root has a parent")
        .join("overlay");
    assert_overlay_empty(&overlay);

    let recon = support::assert_reconciled(&stats_log, opens_ok_delta);
    assert!(recon.routed > 0, "expected routed opens: {recon:?}");
    assert_eq!(
        recon.write_fallback(),
        0,
        "under-root writes fell through to the shim-local overlay {} time(s) — including, \
         possibly, the in-place edit this scenario exists for. Report: {:?}",
        recon.write_fallback(),
        std::fs::read_to_string(&stats_log)
    );
    assert!(recon.outcomes_section_found, "no outcomes section: {recon:?}");

    client
        .teardown_session(vfs_control::pb::TeardownReq {
            session_id: session.id,
        })
        .await
        .expect("teardown");

    server.abort();
}

/// A rooted launch on Windows: `{Game}\fixture.exe` resolves to the graph-only
/// `fixture.exe` (a copy living only in the disk source, absent from `loc`),
/// which the session stages, and the launched process reads `hello.txt` through
/// the injected shim at `<loc>\hello.txt`. Then the same image spelled as an
/// absolute path inside root 0's location launches too — but by then the first
/// launch's staged copy is a real file at that path, so this second launch takes
/// the **real-file** branch, not staging: it proves the absolute form resolves
/// to the same root-0 vpath, not that it stages.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn rooted_launch_by_name_and_absolute_path_stages_a_graph_only_image() {
    let _guard = LAUNCH_LOCK.lock().await;
    ensure_inject_artifacts();

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr: SocketAddr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let svc = DirectorService::new(SessionRegistry::new());
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(DirectorServer::new(svc))
            .serve_with_incoming(incoming)
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    // The disk source holds a COPY of the fixture plus hello.txt.
    let content = vfs_testkit::tempdir().expect("content tempdir");
    std::fs::copy(
        locate_artifact("vfs-fixture-read.exe"),
        content.path().join("fixture.exe"),
    )
    .expect("copy fixture");
    std::fs::write(content.path().join("hello.txt"), b"hello").unwrap();

    // Root 0's location: a fresh path that does not exist yet (the first
    // launch creates it), so fixture.exe is graph-only relative to it.
    let base = vfs_testkit::tempdir().expect("base tempdir");
    let loc = base.path().join("Game").to_string_lossy().replace('/', "\\");
    assert!(!Path::new(&loc).exists());

    let cfg = SessionConfig {
        session: vfs_control::SessionMeta {
            name: Some("rooted-launch".into()),
        },
        roots: vec![vfs_control::RootEntry {
            id: 0,
            name: "Game".into(),
            path: loc.clone(),
        }],
        sources: vec![vfs_control::SourceEntry {
            spec: vfs_control::SourceSpec::Disk {
                path: content.path().to_string_lossy().into_owned(),
            },
            mount: "/".into(),
            root: 0,
            write_layer: false,
            cache_key: None,
        }],
        launch: None,
        ..Default::default()
    };

    let mut client = connect(&format!("{addr}")).await.expect("connect");
    let (id, _) = apply_session_config(&mut client, &cfg)
        .await
        .expect("apply_session_config");

    let launch_with = |exec: String| vfs_control::LaunchConfig {
        exec,
        args: vec![],
        wait: true,
        env: [
            ("VFS_FIXTURE_PATH".to_string(), format!(r"{loc}\hello.txt")),
            ("VFS_FIXTURE_EXPECT".to_string(), "5".to_string()),
        ]
        .into_iter()
        .collect(),
    };

    // Bounded like `drain_launch_events`: `run_launch` itself waits forever, and
    // this test holds LAUNCH_LOCK.
    let stall = |which: &str| -> String {
        format!(
            "launch {which} stalled after {:?}; raise VFS_TEST_LAUNCH_TIMEOUT_SECS \
             if this machine is merely slow",
            launch_timeout()
        )
    };

    let by_name = tokio::time::timeout(
        launch_timeout(),
        vfs_directord::run_launch(&mut client, &id, &launch_with(r"{Game}\fixture.exe".into())),
    )
    .await
    .unwrap_or_else(|_| panic!("{}", stall(r"{Game}\fixture.exe")))
    .expect("launch by root name");
    assert_eq!(by_name, Some(0), "{{Game}}\\fixture.exe should stage and exit 0");

    let abs = format!(r"{loc}\fixture.exe");
    let by_path = tokio::time::timeout(
        launch_timeout(),
        vfs_directord::run_launch(&mut client, &id, &launch_with(abs.clone())),
    )
    .await
    .unwrap_or_else(|_| panic!("{}", stall(&abs)))
    .expect("launch by absolute path");
    assert_eq!(
        by_path,
        Some(0),
        "absolute path inside root 0 (now the staged real file) should launch and exit 0"
    );

    client
        .teardown_session(vfs_control::pb::TeardownReq { session_id: id })
        .await
        .expect("teardown");
    server.abort();
}

fn toml_string(s: &str) -> String {
    // Quote a path for TOML (escape backslashes).
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Sessions default to a directory under the system temp dir, and the daemons
/// these tests spawn inherit this process's environment. Point both at `target/`.
#[ctor::ctor]
fn scratch_tmpdir() {
    vfs_testkit::use_scratch_as_tmpdir();
}

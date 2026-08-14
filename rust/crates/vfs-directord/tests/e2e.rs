//! M0 acceptance: daemon → CreateSession → AddSource(disk) → Launch(fixture-read)
//! via a scenario.toml, asserting the fixture reads virtual bytes through the ring.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tokio::net::TcpListener;
use tonic::transport::Server;
use vfs_control::pb::director_server::DirectorServer;
use vfs_control::SessionConfig;
use vfs_directord::{apply_session_config, connect, DirectorService, SessionRegistry};

mod support;

fn profile_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let dir = exe.parent().unwrap();
    if dir.file_name().and_then(|s| s.to_str()) == Some("deps") {
        dir.parent().unwrap().to_path_buf()
    } else {
        dir.to_path_buf()
    }
}

fn locate_artifact(name: &str) -> PathBuf {
    let profile = profile_dir();
    for cand in [profile.join(name), profile.join("deps").join(name)] {
        if cand.is_file() {
            return cand;
        }
    }
    // Workspace target fallback (when test runs from unexpected layout).
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target");
    for profile in ["debug", "release"] {
        for cand in [
            root.join(profile).join(name),
            root.join(profile).join("deps").join(name),
        ] {
            if cand.is_file() {
                return cand;
            }
        }
    }
    panic!(
        "{name} not found near {:?}; build -p vfs-fixture-read -p vfs-shim-dll and vfs-payload (--manifest-path crates/vfs-payload/Cargo.toml) first",
        profile_dir()
    );
}

/// Every crate directory reachable from `crate_dir` by following `path =
/// "..."` dependencies — normal, dev, build, and per-target — transitively,
/// including `crate_dir` itself. Parsed straight from each crate's
/// `Cargo.toml` rather than hand-maintained: a fixed list of "the crates that
/// feed vfs-shim-dll" silently stops covering the graph the moment a
/// dependency is added or changed, which is exactly how this function's
/// caller earned its history of testing against a stale DLL.
///
/// Some of these crates depend on each other in both directions across the
/// normal/dev split (`vfs-shim` depends on `vfs-inject`; `vfs-inject`
/// dev-depends on `vfs-shim`), so this canonicalizes each directory before
/// checking whether it has already been queued — without that, a `path =
/// "../x"` hop back into an already-visited crate would never match its
/// earlier, differently-`..`-laden spelling, and the walk would not
/// terminate.
fn transitive_crate_dirs(crate_dir: &Path) -> Vec<PathBuf> {
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    queue.push_back(crate_dir.to_path_buf());
    while let Some(raw_dir) = queue.pop_front() {
        let dir = raw_dir.canonicalize().unwrap_or(raw_dir);
        if seen.contains(&dir) {
            continue;
        }
        seen.push(dir.clone());
        let Ok(text) = std::fs::read_to_string(dir.join("Cargo.toml")) else {
            continue;
        };
        let Ok(manifest) = text.parse::<toml::Value>() else {
            continue;
        };
        let mut dep_tables: Vec<&toml::Value> = Vec::new();
        for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(t) = manifest.get(key) {
                dep_tables.push(t);
            }
        }
        if let Some(targets) = manifest.get("target").and_then(|t| t.as_table()) {
            for platform in targets.values() {
                for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
                    if let Some(t) = platform.get(key) {
                        dep_tables.push(t);
                    }
                }
            }
        }
        for table in dep_tables {
            let Some(table) = table.as_table() else { continue };
            for spec in table.values() {
                if let Some(rel) = spec.get("path").and_then(|p| p.as_str()) {
                    queue.push_back(dir.join(rel));
                }
            }
        }
    }
    seen
}

/// Latest modification time of any file under `dir`, recursively, skipping
/// `target` and `.git`. `None` if `dir` has no files (or does not exist).
fn newest_mtime(dir: &Path) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if matches!(entry.file_name().to_str(), Some("target") | Some(".git")) {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) {
                newest = Some(newest.map_or(mtime, |n| n.max(mtime)));
            }
        }
    }
    newest
}

/// Whether `artifact` is missing, or older than any file feeding the crate at
/// `crate_dir` through its transitive local dependency graph.
///
/// This is the check the old `ensure_inject_artifacts` skipped: it rebuilt
/// only when an artifact file did not exist, so `cargo test -p vfs-directord`
/// would silently validate a change to `vfs-redirect` or `vfs-shim` against
/// whatever DLL a previous, unrelated build had left behind — no error, just
/// a passing test that measured the wrong binary. A needless rebuild costs
/// seconds; a stale one costs a false pass, so staleness (not mere absence)
/// is the bar, and every direction of that comparison is biased toward
/// rebuilding: `artifact_is_stale` treats an unreadable artifact as stale
/// (not "assume fresh"), and treats an unreadable dependency directory as
/// contributing no mtime (so it can never *suppress* a rebuild it should not).
fn artifact_is_stale(artifact: &Path, crate_dir: &Path) -> bool {
    let Ok(artifact_mtime) = std::fs::metadata(artifact).and_then(|m| m.modified()) else {
        return true;
    };
    transitive_crate_dirs(crate_dir)
        .iter()
        .filter_map(|dir| newest_mtime(dir))
        .any(|source_mtime| source_mtime > artifact_mtime)
}

fn ensure_inject_artifacts() {
    // Session::launch locates shim/payload near the current exe (the test
    // binary). Co-locate them into the profile dir if cargo left them only in
    // deps/ or they were never built for this package.
    let profile = profile_dir();
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let needed = [
        "vfs_shim_dll.dll",
        "vfs_payload.dll",
        "vfs-fixture-read.exe",
        "vfs-fixture-writepath.exe",
        "vfs-fixture-escape.exe",
    ];
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());

    // vfs_payload.dll is its own separate workspace (see below); the rest
    // build together as part of this one. Each is checked for staleness
    // against its own crate's transitive source, not merely for presence.
    let main_artifact_crates: [(&str, &str); 4] = [
        ("vfs_shim_dll.dll", "vfs-shim-dll"),
        ("vfs-fixture-read.exe", "vfs-fixture-read"),
        ("vfs-fixture-writepath.exe", "vfs-fixture-writepath"),
        ("vfs-fixture-escape.exe", "vfs-fixture-escape"),
    ];
    let main_stale = main_artifact_crates.iter().any(|(artifact, crate_name)| {
        artifact_is_stale(&profile.join(artifact), &workspace.join("crates").join(crate_name))
    });
    if main_stale {
        let status = std::process::Command::new(&cargo)
            .current_dir(&workspace)
            .args([
                "build",
                "-p",
                "vfs-shim-dll",
                "-p",
                "vfs-fixture-read",
                "-p",
                "vfs-fixture-writepath",
                "-p",
                "vfs-fixture-escape",
                "--quiet",
            ])
            .status()
            .expect("spawn cargo");
        assert!(status.success(), "fixture/artifact build failed: {status}");
    }

    // vfs-payload lives in its own workspace (panic = "abort"). Build it
    // into the same target dir so the co-location below finds it unchanged.
    let target_dir = workspace.join("target");
    let payload_stale = artifact_is_stale(
        &profile.join("vfs_payload.dll"),
        &workspace.join("crates").join("vfs-payload"),
    );
    if payload_stale {
        let status = std::process::Command::new(&cargo)
            .current_dir(&workspace)
            .env("CARGO_TARGET_DIR", &target_dir)
            .args([
                "build",
                "--manifest-path",
                "crates/vfs-payload/Cargo.toml",
                "--quiet",
            ])
            .status()
            .expect("spawn cargo to build vfs-payload");
        assert!(status.success(), "vfs-payload cargo build failed: {status}");
    }

    // Copy into profile root so Session::launch's find_near works from the
    // test exe. Always overwrite: a rebuilt artifact must replace whatever
    // was co-located here before, not sit next to a stale copy that a
    // skip-if-exists check would otherwise leave in place untouched.
    for name in needed {
        let dest = profile.join(name);
        let src = locate_artifact(name);
        let same_file = match (src.canonicalize(), dest.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
        if same_file {
            continue;
        }
        let _ = std::fs::copy(&src, &dest);
    }
}

/// `Session::launch` configures the injected child through **process-global**
/// environment variables (`IpcServe::apply_env`'s own doc comment: "for the
/// injected child (and single-session hosts)"). Any two tests in this binary
/// that each create a session and launch a real child process race on that
/// global env under the default (parallel) test harness — whichever
/// session's `apply_env` fires last wins for the whole process, so a child
/// can silently connect to the *other* test's ring/session instead of its
/// own. Flip-tested: without this lock, running this file's launching tests
/// together is intermittently flaky (a write lands nowhere the assertions
/// expect) even though each passes reliably alone.
///
/// This is the project's stated convention for a test touching process-global
/// state (see `VA_LOCK` in `vfs-shim::lazy_section`) rather than moving every
/// launching test into its own binary. An async `tokio::sync::Mutex`, not
/// `std::sync::Mutex`: the guard is held across `.await` points for this
/// test's whole session lifecycle, which clippy's `await_holding_lock` rightly
/// refuses for a std lock.
static LAUNCH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

    let content_dir = tempfile::tempdir().expect("tempdir");
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
layer = 0

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
        launch_event, source_spec, AddSourceReq, DiskSource, LaunchReq, SourceSpec as PbSource,
    };

    for entry in &cfg.sources {
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
                layer: entry.layer,
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
    while let Some(ev) = stream.message().await.expect("stream") {
        match ev.event {
            Some(launch_event::Event::Exited(x)) => exit_code = Some(x.code),
            Some(launch_event::Event::Started(_)) => {}
            Some(launch_event::Event::Log(l)) => eprintln!("log: {}", l.line),
            None => {}
        }
    }

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
    let content_dir = tempfile::tempdir().expect("tempdir");

    // A separate directory (not the DiskProvider's backing store) for the
    // shim's own stats report, so `VFS_SHIM_STATS_LOG`'s temp/rename dance
    // never shows up as a stray entry when the write-path assertions below
    // list content_dir.
    let stats_dir = tempfile::tempdir().expect("stats tempdir");
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

    use vfs_control::pb::{launch_event, source_spec, AddSourceReq, DiskSource, LaunchReq, SourceSpec as PbSource};

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
    while let Some(ev) = stream.message().await.expect("stream") {
        match ev.event {
            Some(launch_event::Event::Exited(x)) => exit_code = Some(x.code),
            Some(launch_event::Event::Started(_)) => {}
            Some(launch_event::Event::Log(l)) => eprintln!("log: {}", l.line),
            None => {}
        }
    }

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
    // Not a claim that fall-through is zero — it isn't yet, that's the
    // entire point of measuring before removing it (gates 2-5 do the
    // removing). Only that the section this test depends on genuinely
    // exists and parsed, rather than the reconciliation above having passed
    // vacuously on an empty/missing report.
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
         ({overlay:?} contains {overlay_entries:?}) — this is the bypass the phase closes"
    );
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
    let bottom_dir = tempfile::tempdir().expect("tempdir bottom");
    let top_dir = tempfile::tempdir().expect("tempdir top");

    // Separate from both source directories, same reasoning as the
    // single-source test above: keeps the shim's `VFS_SHIM_STATS_LOG`
    // temp/rename dance out of the bottom/top directory listings the
    // assertions below rely on being exactly the fixture's own writes.
    let stats_dir = tempfile::tempdir().expect("stats tempdir");
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

    use vfs_control::pb::{launch_event, source_spec, AddSourceReq, DiskSource, LaunchReq, SourceSpec as PbSource};

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
    while let Some(ev) = stream.message().await.expect("stream") {
        match ev.event {
            Some(launch_event::Event::Exited(x)) => exit_code = Some(x.code),
            Some(launch_event::Event::Started(_)) => {}
            Some(launch_event::Event::Log(l)) => eprintln!("log: {}", l.line),
            None => {}
        }
    }

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

#[tokio::test(flavor = "multi_thread")]
async fn apply_session_config_health_and_list() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
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

    let mut client = connect(&format!("{addr}")).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), b"x").unwrap();

    let cfg = SessionConfig {
        session: vfs_control::SessionMeta {
            name: Some("list-me".into()),
        },
        sources: vec![vfs_control::SourceEntry {
            spec: vfs_control::SourceSpec::Disk {
                path: dir.path().to_string_lossy().into_owned(),
            },
            mount: "/".into(),
            layer: 0,
        }],
        launch: None,
        cache: None,
    };
    let (id, exit) = apply_session_config(&mut client, &cfg).await.unwrap();
    assert!(exit.is_none());
    let list = client
        .list_sessions(vfs_control::pb::Empty {})
        .await
        .unwrap()
        .into_inner();
    assert!(list.sessions.iter().any(|s| s.id == id && s.name == "list-me"));

    client
        .teardown_session(vfs_control::pb::TeardownReq { session_id: id })
        .await
        .unwrap();
    server.abort();
}

fn toml_string(s: &str) -> String {
    // Quote a path for TOML (escape backslashes).
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

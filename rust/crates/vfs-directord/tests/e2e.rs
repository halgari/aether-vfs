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

/// Name the processes holding the shim DLL, for the one build failure that is
/// never about the code.
///
/// `cargo build` cannot replace `vfs_shim_dll.dll` while any process has it
/// mapped, and it reports only `Access is denied. (os error 5)` — which reads
/// like a permissions problem and sends you looking in the wrong place. Two
/// things routinely hold it: a leftover game launched by this project's own
/// injector, and orphaned fixtures from a previously killed test.
///
/// The orphan case is self-reinforcing and worth naming loudly: a wedged fixture
/// outlives its test, keeps the DLL mapped, and every later build on that
/// machine fails until someone notices. Measured 2026-09-02: one wedge left
/// three orphans, and with a stale `SkyrimSE` also holding it, 19 tests failed
/// across two runs for a reason none of them had anything to do with.
fn lock_holders_hint() -> String {
    let dll = profile_dir().join("vfs_shim_dll.dll");
    // Only bother if the DLL is genuinely unwritable; a build can fail for
    // ordinary reasons too and this hint would then be noise.
    if std::fs::OpenOptions::new().write(true).open(&dll).is_ok() {
        return String::new();
    }
    let out = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-Command",
            "Get-Process | ForEach-Object { $p=$_; try { if ($p.Modules |              Where-Object { $_.ModuleName -eq 'vfs_shim_dll.dll' }) {              \"$($p.ProcessName) (pid $($p.Id))\" } } catch {} }",
        ])
        .output();
    let holders = match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(", "),
        Err(_) => String::new(),
    };
    if holders.is_empty() {
        format!(
            "

NOTE: {} is not writable, so cargo could not replace it. This is not a              code failure. Could not enumerate the holders.",
            dll.display()
        )
    } else {
        format!(
            "

NOTE: this is NOT a code failure — cargo could not replace {} because these              processes have it mapped: {holders}. Close them (an orphaned fixture from a killed              test, or a game left running by the injector) and re-run.",
            dll.display()
        )
    }
}

/// Move a build artifact aside when a live process has it mapped.
///
/// Windows refuses to *delete or overwrite* a mapped file, which is what makes
/// `cargo build` fail with `Access is denied. (os error 5)` — but it permits
/// **renaming** one. Moving the old file out of the way frees the path so cargo
/// can write a fresh copy, while the process holding the old bytes keeps them.
///
/// This exists because the alternative is a wedged machine. A hung fixture
/// outlives its test and keeps `vfs_shim_dll.dll` mapped; if that fixture is
/// stuck in a non-alertable kernel wait it cannot even be terminated, so every
/// later build on that machine fails until someone reboots. Measured 2026-09-02:
/// three such fixtures survived repeated `Stop-Process` and `taskkill /F`, and
/// with them holding the DLL, 19 tests failed across two runs for a reason none
/// of them had anything to do with. Renaming recovered it without a reboot.
///
/// Best-effort by design: if the rename fails too, the build is attempted
/// anyway and [`lock_holders_hint`] explains what happened.
fn move_locked_artifacts_aside(names: &[&str]) {
    for name in names {
        let path = profile_dir().join(name);
        if !path.exists() {
            continue;
        }
        // Only disturb a file we cannot write; renaming a healthy artifact would
        // churn the build directory for nothing.
        if std::fs::OpenOptions::new().write(true).open(&path).is_ok() {
            continue;
        }
        let aside = path.with_extension(format!(
            "orphanlocked-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ));
        match std::fs::rename(&path, &aside) {
            Ok(()) => eprintln!(
                "note: {} was mapped by a live process; moved aside to {} so cargo can                  replace it (see move_locked_artifacts_aside)",
                path.display(),
                aside.display()
            ),
            Err(e) => eprintln!(
                "note: {} is mapped and could not be moved aside ({e}); the build below                  will probably fail",
                path.display()
            ),
        }
    }
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
        "vfs-fixture-prefs.exe",
    ];
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());

    // vfs_payload.dll is its own separate workspace (see below); the rest
    // build together as part of this one. Each is checked for staleness
    // against its own crate's transitive source, not merely for presence.
    let main_artifact_crates: [(&str, &str); 5] = [
        ("vfs_shim_dll.dll", "vfs-shim-dll"),
        ("vfs-fixture-read.exe", "vfs-fixture-read"),
        ("vfs-fixture-writepath.exe", "vfs-fixture-writepath"),
        ("vfs-fixture-escape.exe", "vfs-fixture-escape"),
        ("vfs-fixture-prefs.exe", "vfs-fixture-prefs"),
    ];
    // Free any artifact path a dead-but-unkillable process still holds, or the
    // build below fails with a bare `Access is denied`.
    move_locked_artifacts_aside(&needed);

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
                "-p",
                "vfs-fixture-prefs",
                "--quiet",
            ])
            .status()
            .expect("spawn cargo");
        assert!(
            status.success(),
            "fixture/artifact build failed: {status}{}",
            lock_holders_hint()
        );
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

/// How long a launched fixture may take before this file calls it wedged.
///
/// Override with `VFS_TEST_LAUNCH_TIMEOUT_SECS` (the `VFS_TEST_` prefix is
/// exempt from `vfs-env`'s registry lint by design, for exactly this kind of
/// harness-only knob).
fn launch_timeout() -> Duration {
    std::env::var("VFS_TEST_LAUNCH_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(Duration::from_secs(240), Duration::from_secs)
}

/// Drain a launch event stream to completion, **bounded**.
///
/// `stream.message().await` carries no timeout of its own, so a fixture that
/// wedges — or an injected shim that never lets it exit — hangs the awaiting
/// test forever. That is not a private failure. Every launching test in this
/// file serialises on [`LAUNCH_LOCK`], so one wedge queues all the others behind
/// it and the binary stops with nothing to diagnose from.
///
/// Measured on CI 2026-09-02: four tests reported "running for over 60 seconds"
/// at the same instant, and the job was still stuck 43 minutes later having
/// produced no further output. Three of those four were victims of the lock, not
/// independent failures.
///
/// A bound turns that into one legible failure, releases the lock so the
/// remaining tests still report honestly, and — the reason this matters — makes
/// the underlying wedge diagnosable at all, because the panic says what had been
/// seen before the stall.
async fn drain_launch_events(
    stream: &mut tonic::Streaming<vfs_control::pb::LaunchEvent>,
    label: &str,
    exit_code: &mut Option<i32>,
) {
    let mut events = 0usize;
    // `Started` is the discriminator that makes a future stall conclusive:
    // seen-but-no-Exited means the child launched and then wedged, while never
    // seeing it means the launch itself never got off the ground. Without this
    // the panic can only say "no Exited", which fits both causes.
    let mut saw_started = false;
    let drain = async {
        while let Some(ev) = stream.message().await.expect("stream") {
            events += 1;
            match ev.event {
                Some(vfs_control::pb::launch_event::Event::Exited(x)) => {
                    *exit_code = Some(x.code)
                }
                Some(vfs_control::pb::launch_event::Event::Started(_)) => saw_started = true,
                Some(vfs_control::pb::launch_event::Event::Log(l)) => {
                    eprintln!("{label}: {}", l.line)
                }
                None => {}
            }
        }
    };
    if tokio::time::timeout(launch_timeout(), drain).await.is_err() {
        panic!(
            "launch event stream stalled after {:?} ({label}): {events} event(s) seen,              saw_started={saw_started}, exit_code={exit_code:?}. {}. Raise              VFS_TEST_LAUNCH_TIMEOUT_SECS if this machine is merely slow.",
            launch_timeout(),
            if saw_started {
                "The child STARTED and then never reported Exited, so it is wedged or died                  without the daemon noticing"
            } else {
                "The child never even reported Started, so the launch itself did not get                  going — suspect artifact staging or the daemon, not the fixture"
            }
        );
    }
}


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
/// - **`CachingProvider` under the overlay.** Every registry source is cache-
///   wrapped; `skyrim-live` mounts raw. So a copy-up seeded *through the block
///   cache* — a cached read feeding a write — had never happened live.
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
    let content_dir = tempfile::tempdir().expect("tempdir");
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
    let mods_bottom = tempfile::tempdir().expect("mods-bottom tempdir");
    let mods_top = tempfile::tempdir().expect("mods-top tempdir");
    for (dir, bytes) in [(&mods_bottom, MOD_BOTTOM), (&mods_top, MOD_TOP)] {
        std::fs::create_dir_all(dir.path().join("Data")).expect("mkdir Data");
        std::fs::write(dir.path().join(MOD_ENTRY), bytes).expect("write mod entry");
    }

    // The declared write layer: a directory of the user's choosing, not the
    // session's own overlay. Left uncreated on purpose — an overwrite folder
    // need not exist before the first write.
    let overwrite_parent = tempfile::tempdir().expect("overwrite tempdir");
    let overwrite = overwrite_parent.path().join("overwrite");

    let stats_dir = tempfile::tempdir().expect("stats tempdir");
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

/// One parsed line of `vfs-fixture-escape`'s TSV output — see that crate's
/// module doc for the exact format this mirrors.
#[derive(Debug, Clone)]
struct EscapeLine {
    vector: String,
    spelling: String,
    outcome: String,
    note: String,
}

fn parse_escape_lines(text: &str) -> Vec<EscapeLine> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            let mut parts = l.splitn(4, '\t');
            Some(EscapeLine {
                vector: parts.next()?.to_string(),
                spelling: parts.next()?.to_string(),
                outcome: parts.next()?.to_string(),
                note: parts.next().unwrap_or("").to_string(),
            })
        })
        .collect()
}

/// The full, fixed vector-id order `vfs-fixture-escape` emits — used to
/// assert every expected line actually showed up (a vector silently
/// missing from the output would otherwise read as "nothing to check"
/// rather than the fixture-contract violation it would be).
/// `3b`/`3c`/`3d` are the object-manager spellings that reach the target
/// through its drive letter (`\??\GLOBALROOT\GLOBAL??\C:\...` and two
/// siblings) rather than through a device name, which is all vector `3` ever
/// built. Every assertion in this file applies to them through the same
/// catch-all expectation tables as the rest — they are ordinary vectors, not
/// caveats — and each was verified to fail here when the canonicaliser fix
/// they cover is reverted.
const ALL_VECTOR_IDS: &[&str] = &[
    "1", "2", "3", "3b", "3c", "3d", "4", "5", "5b", "6", "7", "8", "9", "10a", "10b", "10c", "11",
    "12a", "12b", "12c", "13", "14",
];

/// This gate's own scope note (`docs/superpowers/plans/...`): vectors 13
/// and 14 are reported, not closed, here — 13 needs gate 3's timing fix, 14
/// may not be a shim fix at all. Neither gets a strict outcome assertion in
/// either canary; both still have their line printed and preserved in the
/// matrix, with the fixture's own "reported, not closed" note carried
/// through, so a reader can never mistake a blank for a pass.
fn is_reported_not_closed(vector: &str) -> bool {
    matches!(vector, "13" | "14")
}

/// The positive canary's expected outcome per vector, or `None` for a
/// vector this test does not assert an exact outcome for (the two
/// reported-not-closed vectors, and `5b`'s documented caveat — see its own
/// doc comment in `vfs-fixture-escape`).
///
/// Every other buildable vector must open the real bytes: this is the half
/// of the matrix the brief calls "fully assertable now... the cheap way to
/// pass a containment test is to break all access, and the positive canary
/// is what forbids that."
fn positive_expectation(vector: &str) -> Option<&'static str> {
    if vector == "5b" || is_reported_not_closed(vector) {
        return None;
    }
    // Stage 2b task 5 flip, covering vectors 1, 3, 4, 7 and 9 together: all
    // five are back to the catch-all `Some("opened")` below, which is where
    // they started before Gate 3 Task 5 moved them out.
    //
    // Why they were `not-found` in between: those five are recognised as
    // under-root *only* by `RootMap::compute_under_root`'s canonicalisation
    // (`vfs-redirect`'s device/volume-GUID/GLOBALROOT/UNC-admin-share/
    // junction-alias tables), and `fuse_client::vpath_under_root` — the
    // shim-side router deciding whether an open reaches the director at all —
    // used to be a *second*, plain string-prefix predicate with none of those
    // tables. So `try_fuse_create` gave up for all five spellings and fell
    // through to `decision_for`/`RootMap`, which in a live session resolves
    // against the shim's embedded empty-tree snapshot and answered
    // `SnapResolution::NotFound` no matter what the director actually had.
    // Gate 3 Task 5 sealed that `NotFound` (correctly), which is what turned
    // these five from `opened`-via-real-disk into `not-found`.
    //
    // What changed: task 5 deleted the second predicate.
    // `fuse_client::vpath_under_root` *is* a `RootMap` now, so these five
    // spellings route to the director like any ordinary path, and the
    // director genuinely has the positive canary's content — so they open
    // through the director rather than by reading the byte-identical real
    // file on `session.root`, which is the outcome gate 3 was reaching for.
    //
    // The `negative_expectation` side is unchanged and still `not-found` for
    // all five: routing them to the director does not make a file no provider
    // serves appear. Together those two are the containment claim — reachable
    // when a provider has it, sealed when none does — for every spelling this
    // fixture can build, not merely for the ordinary one.
    match vector {
        // A hardlink names the SAME bytes under a brand-new file name the
        // content-addressed provider has never heard of. FUSE-routing (the
        // shim's pre-existing, gate-2-independent `vpath_under_root`
        // matcher, not this gate's canonicaliser) recognises the ordinary,
        // unmangled path and asks the director for that name first; the
        // director correctly answers "no such name" (`ST_NOT_FOUND`), and
        // with disk-fallthrough at its secure default (off,
        // `VFS_ALLOW_DISK_FALLTHROUGH` unset), that answer is sealed rather
        // than falling through to the real, hardlinked bytes still sitting
        // on disk. Verified by reproduction, not assumed: this is an
        // inherent property of naming the same bytes under a name the
        // content model has never seen — orthogonal to gate 2's
        // canonicaliser, which is never even consulted for this vector
        // whenever FUSE-routing claims the path first.
        "8" => Some("not-found"),
        // Read-only, `OPEN_EXISTING`, against a stream this fixture never
        // pre-creates (see the fixture's own vector-11 doc comment) —
        // legitimately absent, standalone or under a session.
        "11" => Some("not-found"),
        _ => Some("opened"),
    }
}

/// The negative canary's expected outcome for a **read** open, or `None` for
/// a vector this check does not apply to strictly — the same two documented
/// exceptions `positive_expectation` already carries:
///
/// - `"5b"`: not an alternate classification of the negative canary at all.
///   `OBJECT_ATTRIBUTES.RootDirectory` pointing at an anonymous pipe fails
///   the construction itself at the NT level (`error:ntstatus:...`,
///   independent of which target is named), so there is no "reachable vs.
///   not-found" question to assert here regardless of target.
/// - `"13"`/`"14"` (`is_reported_not_closed`): per this gate's own scope
///   note, neither vector gets a strict outcome assertion in either canary.
///   `"14"` spawns a child process, on the long-standing assumption that the
///   child runs with **no shim injected** and so reads the real, physical
///   negative-canary bytes directly. **Measured otherwise in gate 4 task 8**:
///   the shim hooks `CreateProcessInternalW` and injects into children
///   (`vfs-shim/src/hook.rs`), so the child is injected and this line actually
///   reports `error:cmd-exit:1` — the real bytes were *not* reachable. Still
///   unasserted, because that injection is explicitly best-effort (force
///   suspend, inject, give up on timeout), so asserting it would be asserting
///   a scheduling outcome. See `rust/docs/escape-matrix.md`, "Gate 4, Task 8".
///
/// Every other buildable vector must now come back `not-found`: Gate 3 Task
/// 5 stopped `RootMap::decide` passing `NotFound`/`Dir` through, and the
/// director itself answers "no such name" for any spelling that reaches it
/// with disk-fallthrough off — so a real, on-disk file under root that no
/// provider serves is unreachable by a read, for every spelling this fixture
/// can build, not merely classified into a counted bucket while still
/// secretly readable. This is a stronger claim than `classification_marker`
/// below checks, and the two are asserted separately in the test body — see
/// this function's own call site for why classified-but-reachable is exactly
/// the failure mode that made "classification, not containment" the matrix's
/// standing caveat before this task.
fn negative_expectation(vector: &str) -> Option<&'static str> {
    if vector == "5b" || is_reported_not_closed(vector) {
        return None;
    }
    Some("not-found")
}

/// The substring this test searches for in the shim's classified-paths set
/// (see `support::classified_paths`) to decide whether a given vector's
/// attempt was classified under-root, or `None` for a vector this check
/// does not apply to.
///
/// `"14"` is excluded unconditionally, though **not for the reason this
/// comment used to give**. It said the child runs with no shim injected at
/// all, so its open happens in a process whose hook stats this test can never
/// see. The shim in fact detours `CreateProcessInternalW` and injects into
/// children (`vfs-shim/src/hook.rs`; measured in gate 4 task 8 — see
/// `rust/docs/escape-matrix.md`), so the child is hooked and, inheriting
/// `VFS_SHIM_STATS_LOG`, may even report into this same file.
///
/// The exclusion stands anyway, and is now the stronger claim rather than the
/// weaker one: whatever appears is a *different process's* classification of
/// its own open, not this vector's, and whether it appears at all depends on
/// a best-effort inject that is allowed to time out. Presence and absence are
/// both scheduling outcomes here, so neither is evidence about gate 2.
///
/// `"5b"` is excluded too, but for the opposite reason: it *is* an
/// in-process, hooked open, but one whose `OBJECT_ATTRIBUTES.RootDirectory`
/// (an anonymous pipe) `GetFinalPathNameByHandleW` cannot resolve, so
/// `path_of_tracked` never decodes a path for it at all — the open is real
/// but genuinely un-decodable, landing in the shim's separate "undecodable"
/// counter, never in "under-root open outcomes". That is Task 4's
/// documented, accepted edge (falls back to the pre-existing passthrough),
/// not a gate-2 classification miss — see this vector's own note in the
/// matrix.
///
/// `"7"` (junction) and `"9"` (UNC admin share) **used to be excluded here
/// too**, for a third, more serious reason: verified by isolated
/// reproduction, they genuinely did not classify — both resolve to the real
/// bytes via a syntactically unrelated path (a different directory tree for
/// the junction; a `UNC\localhost\C$\...` form for the admin share) that
/// contains no `~`, so `RootMap::compute_under_root` never reached its
/// OS-consult branch (`expand_short_name`), the only place a syntactically
/// unrelated path like these could ever be recognised.
///
/// Both are now closed by resolving each into a `VolumeMap` alias **once at
/// session start**, the same pattern the device/volume-GUID table already
/// uses, rather than widening the per-open OS-consult gate: `vfs-redirect`'s
/// `resolve_volume_map` now also (a) registers `\??\UNC\localhost\<drive>$`
/// as an alias for `<drive>:` for every mounted drive, and (b) walks the
/// managed root's own ancestor chain, one non-recursive directory listing
/// per level, registering any *sibling* reparse point whose resolved target
/// lands inside the root. See `vfs-redirect/src/volumes.rs`'s
/// `junction_aliases` and `admin_share_nt_key` doc comments for the full
/// mechanism, the scope this task deliberately chose (and rejected), and
/// `rust/docs/escape-matrix.md` for the verified before/after.
fn classification_marker(vector: &str, basename: &str) -> Option<String> {
    match vector {
        "14" | "5b" => None,
        // The hardlink itself, not the original canary name.
        "8" => Some("vfs-escape-hardlink".to_string()),
        _ => Some(basename.to_ascii_lowercase()),
    }
}

/// Launch `vfs-fixture-escape.exe` against `target` under `client`'s
/// `session_id`, with hook-stats logging enabled, and return its own parsed
/// TSV lines plus the shim's classified-paths set (see
/// `support::classified_paths`) built from the same run.
/// The parts of an escape-matrix fixture launch that stay constant across
/// every call in one test run — bundled so `run_escape_fixture` itself
/// stays under clippy's argument-count lint rather than growing a ninth
/// positional parameter for every future per-vector wrinkle.
#[derive(Clone, Copy)]
struct EscapeFixtureCtx<'a> {
    session_id: &'a str,
    fixture: &'a Path,
    stats_log: &'a Path,
    /// See `VFS_ESCAPE_VECTOR7_LINK_DIR`'s doc comment in `vfs-env`: a
    /// junction created by this test harness itself, before any fixture
    /// process is launched, so vector 7 never has to construct one from
    /// inside an already-injected process.
    vector7_link_dir: Option<&'a str>,
    /// `true` sets `VFS_ESCAPE_ACCESS=write`, so every vector writes through
    /// its spelling instead of reading through it. Unset (the default) the
    /// fixture runs the read matrix exactly as it always has.
    write_access: bool,
}

async fn run_escape_fixture(
    client: &mut vfs_control::pb::director_client::DirectorClient<tonic::transport::Channel>,
    ctx: &EscapeFixtureCtx<'_>,
    target: &Path,
    out_file: &Path,
    only_vector: Option<&str>,
) -> (i32, Vec<EscapeLine>, std::collections::BTreeSet<String>, bool) {
    use vfs_control::pb::LaunchReq;
    let EscapeFixtureCtx { session_id, fixture, stats_log, vector7_link_dir, write_access } = *ctx;

    let _ = std::fs::remove_file(stats_log);
    let _ = std::fs::remove_file(out_file);

    let mut env = std::collections::HashMap::new();
    env.insert("VFS_SHIM_STATS_LOG".to_string(), stats_log.to_string_lossy().into_owned());
    if let Some(dir) = vector7_link_dir {
        env.insert("VFS_ESCAPE_VECTOR7_LINK_DIR".to_string(), dir.to_string());
    }
    if write_access {
        env.insert("VFS_ESCAPE_ACCESS".to_string(), "write".to_string());
    }
    // Fast tick: this whole run (twenty-two lines plus a couple of helper
    // process spawns) finishes in well under the reporter's 250ms default,
    // so a short override is what makes the classification snapshot land at
    // all — same reasoning as the write-path e2e tests' identical override.
    //
    // The vectors-7/9 closeout found that this alone is not quite enough
    // margin for an *isolated* single-vector run specifically: with
    // `VFS_ESCAPE_ONLY_VECTOR` set, the selected vector's own decision is
    // the *only* real file activity in the whole process, so the process's
    // total lifetime can be short enough that `vfs-fixture-escape`'s own
    // end-of-run wait (`interval_ms * 2` = 10ms here) lands under Windows'
    // default ~15.6ms system timer resolution — a `Sleep(10)` on Windows is
    // not reliably "wakes at 10ms", only "wakes no earlier than 10ms, next
    // tick or later" — occasionally letting the process exit before the
    // reporter's first tick ever fires, an intermittent classification miss
    // unrelated to canonicalisation itself. Fixed at the source
    // (`vfs-fixture-escape::main`'s end-of-run wait is now floored at 20ms,
    // comfortably clearing that granularity) rather than by tuning this
    // interval further, since shrinking it below Windows' own timer
    // resolution floor would not have helped either.
    env.insert("VFS_SHIM_STATS_INTERVAL_MS".to_string(), "5".to_string());
    if let Some(v) = only_vector {
        env.insert("VFS_ESCAPE_ONLY_VECTOR".to_string(), v.to_string());
    }

    let mut stream = client
        .launch(LaunchReq {
            session_id: session_id.to_string(),
            exec: fixture.to_string_lossy().into_owned(),
            args: vec![
                target.to_string_lossy().into_owned(),
                out_file.to_string_lossy().into_owned(),
            ],
            wait: true,
            env,
        })
        .await
        .expect("Launch")
        .into_inner();

    let mut exit_code = None;
    drain_launch_events(&mut stream, "escape fixture log", &mut exit_code).await;

    let text = std::fs::read_to_string(out_file).unwrap_or_default();
    let lines = parse_escape_lines(&text);
    let (classified, truncated) = support::classified_paths(stats_log);
    (exit_code.unwrap_or(-1), lines, classified, truncated)
}

/// Task 6: the canary matrix. Runs `vfs-fixture-escape` under a real,
/// composed session — daemon, director, injected shim, the works — against
/// two targets, and checks the two halves the gate's scope note draws:
///
/// - **Positive canary** (`escape-positive-canary.esp`, mirrored
///   byte-for-byte into both the `DiskProvider`'s backing directory and the
///   physical, on-disk managed root): every buildable spelling must open
///   it, with the *exact same bytes* — checked by the fixture itself
///   (`vfs-fixture-escape` now reads every successful open back and
///   compares against a baseline read of the literal path, failing closed
///   to `error:content-mismatch:...` on any difference; see that crate's
///   module doc). This is the half that forbids "pass by breaking
///   everything".
/// - **Negative canary** (`escape-negative-canary.bin`, a real file
///   physically on the managed root that the `DiskProvider` never serves):
///   two properties are now asserted, not one.
///   - **Classified** — every buildable spelling still appears in a counted
///     outcome bucket in the shim's own hook-stats report, checked in
///     isolation (`VFS_ESCAPE_ONLY_VECTOR`) to rule out riding on another
///     vector's entry — this is the gate-2-era property, unchanged.
///   - **Unreachable, Gate 3 Task 6's own addition**: every buildable
///     spelling's **read** open must come back `not-found`. Before this
///     gate, "classified" and "reachable" could both be true for the same
///     vector at once (see `rust/docs/escape-matrix.md`'s "second, structural
///     finding") — classification alone was never proof of containment.
///     This is the assertion that closes that gap: a vector that is merely
///     classified while still opening the real bytes now fails this test.
///     Scoped to reads only, because when this was written a **write** open
///     still reached the negative canary through `Engine::cow_seed`'s
///     last-resort branch. Gate 4's Task 5 deleted that branch, and
///     `escape_matrix_write_access_positive_and_negative_canary` below now
///     asserts the write half — so the scope note describes this test's
///     coverage, not a remaining hole. `5b` (undecodable handle-relative
///     open) and the two reported-not-closed vectors (`13`, `14`) are exempt
///     from this assertion for the same documented reasons the positive
///     canary's own `positive_expectation` already exempts them — see
///     `negative_expectation`'s doc comment.
///
/// A stack-overflow crash was found and fixed while building this test (see
/// `vfs_redirect`'s `OS_CONSULT_DEPTH` guard) — vector 1 (8.3 short name)
/// recursed without bound the first time this matrix was run against a
/// *served* target, because `RootMap::compute_under_root`'s OS-consult
/// branch made its own hooked `CreateFileW` call with no re-entrancy guard.
/// See `task-6-report.md` for the full account.
#[tokio::test(flavor = "multi_thread")]
async fn escape_matrix_positive_and_negative_canary() {
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

    // The DiskProvider's backing store — deliberately NOT session.root, so
    // the negative canary (written only to session.root below) is a real
    // file under the managed root that this provider genuinely does not
    // have, rather than something this test would have to fake.
    let content_dir = tempfile::tempdir().expect("tempdir");
    let stats_dir = tempfile::tempdir().expect("stats tempdir");
    let stats_log = stats_dir.path().join("shim-stats.log");
    let out_dir = tempfile::tempdir().expect("out tempdir");
    let out_file = out_dir.path().join("escape-out.tsv");

    let fixture = locate_artifact("vfs-fixture-escape.exe");
    let mut client = connect(&format!("{addr}")).await.expect("connect");

    let session = client
        .create_session(vfs_control::pb::CreateSessionReq {
            name: "escape-matrix".into(),
        })
        .await
        .expect("CreateSession")
        .into_inner();
    assert!(!session.id.is_empty());
    assert!(!session.root.is_empty());

    use vfs_control::pb::{source_spec, AddSourceReq, DiskSource, SourceSpec as PbSource};

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
        })
        .await
        .expect("AddSource");

    let root = PathBuf::from(&session.root);
    let sub = PathBuf::from("Games").join("Skyrim").join("Data");
    std::fs::create_dir_all(root.join(&sub)).expect("mkdir under session root");
    std::fs::create_dir_all(content_dir.path().join(&sub)).expect("mkdir under content dir");

    // Positive canary: identical bytes physically on session.root AND in
    // the DiskProvider's backing dir. Whichever mechanism actually serves a
    // given spelling — FUSE-routed (the director, reading content_dir) or
    // real-disk passthrough (reading session.root directly) — the bytes are
    // the same either way, so "opened" is a meaningful byte-identity
    // signal regardless of which path served it.
    const POSITIVE_BASENAME: &str = "escape-positive-canary.esp";
    const POSITIVE_BYTES: &[u8] = b"the-positive-canary-bytes";
    let pos_rel = sub.join(POSITIVE_BASENAME);
    std::fs::write(root.join(&pos_rel), POSITIVE_BYTES).expect("write positive canary (root)");
    std::fs::write(content_dir.path().join(&pos_rel), POSITIVE_BYTES)
        .expect("write positive canary (content_dir)");

    // Negative canary: real bytes ONLY on session.root — a file under the
    // managed root that the DiskProvider's backing dir never has.
    const NEGATIVE_BASENAME: &str = "escape-negative-canary.bin";
    let neg_rel = sub.join(NEGATIVE_BASENAME);
    std::fs::write(root.join(&neg_rel), b"the-negative-canary-bytes")
        .expect("write negative canary");

    // Vector 7's junction, created here — by this test harness's own,
    // never-injected process — rather than by the fixture at runtime. See
    // `VFS_ESCAPE_VECTOR7_LINK_DIR`'s doc comment in `vfs-env` for why: the
    // fixture spawning `mklink /J` itself would be real, hooked file
    // activity inside the injected process, racing `vfs-redirect`'s
    // once-per-session volume/junction resolution (found by reproduction —
    // an isolated vector-7 run consistently failed to classify until this
    // moved out of the fixture). Points at the shared `Data` directory both
    // canaries live in, so one junction covers both.
    let vector7_link = std::env::temp_dir().join(format!("vfs-escape-junction-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir(&vector7_link);
    let vector7_link_ready = std::process::Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            &vector7_link.to_string_lossy(),
            &root.join(&sub).to_string_lossy(),
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let vector7_link_dir = vector7_link_ready.then(|| vector7_link.to_string_lossy().into_owned());
    let ctx = EscapeFixtureCtx {
        session_id: &session.id,
        fixture: &fixture,
        stats_log: &stats_log,
        vector7_link_dir: vector7_link_dir.as_deref(),
        write_access: false,
    };

    // ---------------------------------------------------------------
    // Positive canary: every buildable spelling opens it, byte-identical.
    // ---------------------------------------------------------------
    let (pos_exit, pos_lines, _pos_classified, _pos_truncated) =
        run_escape_fixture(&mut client, &ctx, &root.join(&pos_rel), &out_file, None).await;
    if std::env::var("VFS_TEST_MATRIX_DUMP").is_ok() {
        eprintln!("=== POSITIVE lines ===");
        for l in &pos_lines {
            eprintln!("{}\t{}\t{}\t{}", l.vector, l.spelling, l.outcome, l.note);
        }
    }
    assert_eq!(
        pos_exit, 0,
        "vfs-fixture-escape must exit 0 against the positive canary — a nonzero/crash exit \
         (e.g. STATUS_STACK_OVERFLOW, -1073741571) means a vector took the process down before \
         the rest of the matrix could even be attempted, which is worse than any single \
         vector's own outcome. Lines captured before the crash: {pos_lines:?}"
    );
    for id in ALL_VECTOR_IDS {
        assert!(
            pos_lines.iter().any(|l| &l.vector == id),
            "positive canary: vector {id} produced no line at all in {out_file:?} — a missing \
             line must never be readable as a pass"
        );
    }
    for line in &pos_lines {
        let Some(want) = positive_expectation(&line.vector) else { continue };
        if line.outcome.starts_with("unbuildable:") {
            // A first-class, environment-dependent outcome — recorded in
            // the matrix, not a failure of this assertion.
            continue;
        }
        assert_eq!(
            line.outcome, want,
            "positive canary vector {}: expected `{want}`, got `{}` (spelling: {:?}, note: {:?})",
            line.vector, line.outcome, line.spelling, line.note
        );
    }

    // ---------------------------------------------------------------
    // Negative canary: every buildable spelling is classified under-root
    // (appears in the shim's own counted outcome buckets), never merely
    // "reachable" and never invisible as outside-root. See
    // `rust/docs/escape-matrix.md` for what this half does and does not
    // establish.
    // ---------------------------------------------------------------
    let (neg_exit, neg_lines, neg_classified, neg_truncated) =
        run_escape_fixture(&mut client, &ctx, &root.join(&neg_rel), &out_file, None).await;
    if std::env::var("VFS_TEST_MATRIX_DUMP").is_ok() {
        eprintln!("=== NEGATIVE lines ===");
        for l in &neg_lines {
            eprintln!("{}\t{}\t{}\t{}", l.vector, l.spelling, l.outcome, l.note);
        }
        eprintln!("=== NEGATIVE classified set (truncated={neg_truncated}) ===");
        for p in &neg_classified {
            eprintln!("{p}");
        }
    }
    assert_eq!(
        neg_exit, 0,
        "vfs-fixture-escape must exit 0 against the negative canary too. Lines captured: {neg_lines:?}"
    );
    for id in ALL_VECTOR_IDS {
        assert!(
            neg_lines.iter().any(|l| &l.vector == id),
            "negative canary: vector {id} produced no line at all in {out_file:?}"
        );
    }
    assert!(
        !neg_truncated,
        "the shim report's per-outcome path list was truncated (more distinct paths in one \
         outcome bucket than `hookstats::OUTCOME_PATHS_SHOWN`) — this test's per-vector \
         classification search below cannot be \
         trusted against a truncated list, so this must never happen for a run this small. \
         Report: {stats_log:?}"
    );
    // ---------------------------------------------------------------
    // Gate 3, Task 6: the negative canary is now unreachable, not merely
    // classified. Each `EscapeLine` is already tagged with its own vector,
    // so — unlike the classification check below — this needs no isolated
    // re-run to avoid riding on another vector's effect: `line.outcome` is
    // this vector's own attempt's own result, from this combined run.
    //
    // This is the assertion this task adds, and it is strictly stronger than
    // "classified": before Gate 3 Task 5, a spelling could be classified
    // (land in a counted bucket) while still opening the real bytes on
    // `session.root` (see "A second, structural finding" in
    // `rust/docs/escape-matrix.md` — vectors 1/3/4/7/9 were exactly this).
    // Scoped to reads only, per the brief. When that scope was set, a write
    // open still reached this same file through `Engine::cow_seed`'s
    // last-resort branch; gate 4's Task 5 deleted it, and the write half is
    // asserted by `escape_matrix_write_access_positive_and_negative_canary`.
    for line in &neg_lines {
        let Some(want) = negative_expectation(&line.vector) else { continue };
        if line.outcome.starts_with("unbuildable:") {
            continue; // Never attempted at the OS level; nothing to seal.
        }
        assert_eq!(
            line.outcome, want,
            "negative canary vector {}: expected `{want}` — a real file on session.root that no \
             provider serves must be unreachable by a read, for every buildable spelling, not \
             merely classified while still readable — got `{}` (spelling: {:?}, note: {:?})",
            line.vector, line.outcome, line.spelling, line.note
        );
    }
    // The combined run above shares one shim-stats report across all
    // twenty-two attempts, and the report's classified-paths set is not keyed
    // by vector — several *different* spellings legitimately canonicalise
    // to the identical recorded path (that collapsing is the whole point of
    // the canonicaliser), so "some entry contains this vector's marker" in
    // the combined set does not prove *this* vector's own attempt was the
    // one that produced it. A vector whose own attempt was silently
    // unclassified (outside-root, invisible) would still pass that check
    // for free, riding on an unrelated vector's classified entry that
    // happens to share the same filename substring — exactly the "silently
    // probed nothing and reported closed" failure this project has hit
    // before. Re-run each buildable vector *alone* (`VFS_ESCAPE_ONLY_VECTOR`
    // — see `vfs-fixture-escape`'s module doc), so its isolated run's
    // classified set can only ever contain its own attempt's effect, plus
    // the handful of incidental opens (parent-directory probes, etc.) every
    // launch makes regardless of which vector is selected.
    for line in &neg_lines {
        if line.outcome.starts_with("unbuildable:") {
            continue; // Never attempted at the OS level; nothing to classify.
        }
        let Some(marker) = classification_marker(&line.vector, NEGATIVE_BASENAME) else {
            continue; // `5b` / `14` — see `classification_marker`'s doc comment.
        };
        let (iso_exit, iso_lines, iso_classified, iso_truncated) = run_escape_fixture(
            &mut client,
            &ctx,
            &root.join(&neg_rel),
            &out_file,
            Some(line.vector.as_str()),
        )
        .await;
        assert_eq!(
            iso_exit, 0,
            "negative canary, isolated run for vector {}: must exit 0. Lines: {iso_lines:?}",
            line.vector
        );
        assert!(!iso_truncated, "isolated run for vector {} truncated its path list", line.vector);
        if std::env::var("VFS_TEST_MATRIX_DUMP").is_ok() {
            eprintln!("--- isolated vector {} classified set (marker={marker:?}) ---", line.vector);
            for p in &iso_classified {
                eprintln!("{p}");
            }
        }
        let found = iso_classified.iter().any(|p| p.contains(&marker));
        assert!(
            found,
            "negative canary vector {}: run in isolation (every other vector skipped), no entry \
             containing {marker:?} appears in the shim's classified-paths set for that run — \
             this spelling was not recognised as under-root at all (outside-root, invisible to \
             every counter), which is exactly the failure mode this test exists to catch. \
             Spelling: {:?}, fixture-observed outcome (combined run): {}, note: {:?}. Isolated \
             classified set: {:?}",
            line.vector, line.spelling, line.outcome, line.note, iso_classified
        );
    }

    client
        .teardown_session(vfs_control::pb::TeardownReq {
            session_id: session.id,
        })
        .await
        .expect("teardown");

    server.abort();
    if vector7_link_ready {
        let _ = std::fs::remove_dir(&vector7_link);
    }
}

/// Gate 4, Task 8b: **a directory listing under a managed root never reveals
/// a real, unserved file.**
///
/// Every other assertion in this file is about an *open* — can this spelling
/// reach these bytes. Enumeration is a different mechanism with a different
/// predicate: it runs on an already-opened handle, through
/// `NtQueryDirectoryFile(Ex)` → `vfs-shim::hook::serve_dir_query`, and asks
/// `FuseClient::vpath_under_root` rather than `RootMap::decide`. For two
/// gates `rust/docs/escape-matrix.md` claimed enumeration containment
/// "follows directly from read-open containment rather than being a separate
/// mechanism". It does not follow, and behind the argument sat a branch that
/// drained the real directory behind the mount and listed whatever was
/// physically there.
///
/// **That branch was latent, not live** — reachable only if a second,
/// unrelated seal (gate 3 task 5's `NotFound` deny) regressed alongside a
/// disagreement between the shim's two under-root predicates. Both are needed;
/// see the mutation note below. This test's value is that enumeration no
/// longer depends on another gate's invariant untested, not that it rescued a
/// session from an active leak.
///
/// Nothing tested either branch: no fixture called `read_dir` under a session
/// at all, the shim-level enumeration tests attach no director, and every
/// director-level `readdir` test asserts presence or de-duplication, never
/// absence.
///
/// This test is the measurement that replaced the argument. It runs the
/// escape fixture's `enum` vector under a real composed session — daemon,
/// director, injected shim — against the same two-canary geometry the read
/// matrix uses, and asserts **both** directions of one listing:
///
/// - the provider-served positive canary **appears** (so the test cannot pass
///   by breaking enumeration outright, which is how a one-sided
///   absence assertion would pass), and
/// - the physically-present, unserved negative canary **does not**.
///
/// It also asserts the instrument, not only the bytes: `hookstats`'s
/// per-enumeration record must name `director` as the source for this
/// directory. That flag existed before this task (as a `served: bool` that
/// read `true` for the real-disk branch too) and nothing anywhere asserted
/// it. A regression that reinstated a real-disk listing while the director
/// happened to serve the same names would still be caught by the source
/// column even if the bytes agreed.
///
/// **Proven able to fail, not merely observed passing.** With
/// `FuseClient::vpath_under_root` mutated to decline this directory and
/// `RootMap::decide`'s `NotFound` arm reverted to the pre-gate-3
/// `PassThrough` — the two states, both historically real in this tree, that
/// together produce a real directory handle the client does not claim — the
/// unserved canary appears in the listing and this test fails on its absence
/// assertion. Neither mutation alone suffices, which is the measurement
/// behind "latent, not live" above. See `task-8b-report.md` for both
/// mutations and their output.
#[tokio::test(flavor = "multi_thread")]
async fn directory_enumeration_under_a_managed_root_hides_an_unserved_real_file() {
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

    // Same geometry as the read matrix: the provider's backing store is a
    // separate directory, so a file written only onto `session.root` is a
    // real file under the managed root that no provider serves.
    let content_dir = tempfile::tempdir().expect("tempdir");
    let stats_dir = tempfile::tempdir().expect("stats tempdir");
    let stats_log = stats_dir.path().join("shim-stats.log");
    let out_dir = tempfile::tempdir().expect("out tempdir");
    let out_file = out_dir.path().join("escape-enum-out.tsv");

    let fixture = locate_artifact("vfs-fixture-escape.exe");
    let mut client = connect(&format!("{addr}")).await.expect("connect");

    let session = client
        .create_session(vfs_control::pb::CreateSessionReq {
            name: "escape-enum".into(),
        })
        .await
        .expect("CreateSession")
        .into_inner();

    use vfs_control::pb::{source_spec, AddSourceReq, DiskSource, SourceSpec as PbSource};
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
        })
        .await
        .expect("AddSource");

    let root = PathBuf::from(&session.root);
    let sub = PathBuf::from("Games").join("Skyrim").join("Data");
    std::fs::create_dir_all(root.join(&sub)).expect("mkdir under session root");
    std::fs::create_dir_all(content_dir.path().join(&sub)).expect("mkdir under content dir");

    const SERVED: &str = "enum-served-canary.esp";
    const UNSERVED: &str = "enum-unserved-canary.bin";
    let served_rel = sub.join(SERVED);
    std::fs::write(root.join(&served_rel), b"served").expect("write served canary (root)");
    std::fs::write(content_dir.path().join(&served_rel), b"served")
        .expect("write served canary (content_dir)");
    std::fs::write(root.join(sub.join(UNSERVED)), b"unserved").expect("write unserved canary");

    // The listing must be a real question of containment, not of existence:
    // both files really are in the physical directory this vector lists, and
    // an uninjected reader of that directory really does see both. Asserted
    // from this (never-injected) harness process before the fixture runs, so
    // "the unserved canary was absent" can never mean "it was never there".
    let physical = real_dir_names(&root.join(&sub));
    assert!(
        physical.iter().any(|n| n.eq_ignore_ascii_case(SERVED))
            && physical.iter().any(|n| n.eq_ignore_ascii_case(UNSERVED)),
        "the physical directory must hold both canaries before the fixture runs, or the \
         absence assertion below proves nothing: {physical:?}"
    );

    let ctx = EscapeFixtureCtx {
        session_id: &session.id,
        fixture: &fixture,
        stats_log: &stats_log,
        vector7_link_dir: None,
        write_access: false,
    };
    let (exit, lines, _classified, _truncated) = run_escape_fixture(
        &mut client,
        &ctx,
        &root.join(&served_rel),
        &out_file,
        Some("enum"),
    )
    .await;

    assert_eq!(exit, 0, "the enumeration fixture must exit 0. Lines: {lines:?}");
    let line = lines
        .iter()
        .find(|l| l.vector == "enum")
        .unwrap_or_else(|| panic!("no `enum` line in {out_file:?}: {lines:?}"));
    assert!(
        line.outcome.starts_with("listed:"),
        "the served directory must be enumerable under a session — a listing that failed \
         outright would make the absence assertion below vacuous. Got `{}` (note: {:?})",
        line.outcome,
        line.note
    );

    let listed: Vec<String> = line.note.split('|').filter(|s| !s.is_empty()).collect::<Vec<_>>()
        .into_iter()
        .map(|s| s.to_ascii_lowercase())
        .collect();
    assert!(
        listed.iter().any(|n| n == &SERVED.to_ascii_lowercase()),
        "the provider-served file is missing from the listing — enumeration is broken, and a \
         broken enumeration would satisfy the absence half of this test for free. Listed: \
         {listed:?}"
    );
    assert!(
        !listed.iter().any(|n| n == &UNSERVED.to_ascii_lowercase()),
        "a real, physically-present file under the managed root that no provider serves \
         appeared in a directory listing. Reads, metadata and writes are all sealed for this \
         file; enumeration handed out its name. Listed: {listed:?}"
    );

    // The instrument, not only the bytes. `serve_dir_query` records which
    // mechanism produced each listing; the served directory's rows must all
    // say `director`. A regression to the real-disk drain shows up here even
    // in a scenario where the names happen to agree.
    //
    // The recorded path is the NT spelling the open carried, which for a
    // directory keeps its trailing separator — matched on the trimmed form so
    // this does not turn into a silently-vacuous filter if that ever changes.
    let want_dir = format!("\\{}", sub.to_string_lossy().to_ascii_lowercase());
    let records = support::readdir_records(&stats_log);
    let ours: Vec<&support::ReadDirRecord> = records
        .iter()
        .filter(|r| r.dir.trim_end_matches('\\').ends_with(&want_dir))
        .collect();
    assert!(
        !ours.is_empty(),
        "the shim recorded no enumeration of {want_dir:?} at all, so the source assertion \
         below would pass vacuously. All records: {records:?}"
    );
    for r in &ours {
        assert_eq!(
            r.source, "director",
            "a listing of a directory under the managed root was produced by `{}`, not the \
             director. `contained` means the shim's two under-root predicates disagree about \
             this path; anything else means real disk answered. Record: {r:?}",
            r.source
        );
    }
    assert!(
        ours.iter().any(|r| r.count > 0),
        "every recorded listing of {want_dir:?} came back with zero entries: {ours:?}"
    );

    client
        .teardown_session(vfs_control::pb::TeardownReq {
            session_id: session.id,
        })
        .await
        .expect("teardown");
    server.abort();
}

/// One tab-separated line of `vfs-fixture-prefs`' output: the operation, the
/// string the profile API produced (or `ok`/`fail` for a write), and the
/// `GetLastError` recorded straight after the call.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PrefsLine {
    op: String,
    value: String,
    lasterror: String,
}

fn parse_prefs_lines(text: &str) -> Vec<PrefsLine> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            let mut f = l.split('\t');
            Some(PrefsLine {
                op: f.next()?.to_string(),
                value: f.next()?.to_string(),
                lasterror: f.next().unwrap_or_default().to_string(),
            })
        })
        .collect()
}

/// The three strings this test has to tell apart. All three are things
/// `GetPrivateProfileStringW` can hand back *successfully*, which is why the
/// fixture's exit code cannot carry the answer and the harness matches on the
/// value.
const PREFS_VIRTUAL: &str = "VIRTUAL";
const PREFS_DISK: &str = "DISK";
const PREFS_DEFAULT: &str = "MISSING";

/// The Windows profile APIs (`GetPrivateProfileStringW` and family) must read
/// a file under a managed root **through the director**.
///
/// This is the investigation of 2026-08-14 promoted from a throwaway probe to
/// a permanent test. Skyrim loads `SkyrimPrefs.ini` this way, and a live
/// session showed 450 opens of it with zero reads. The cause was not the
/// profile APIs bypassing our hooks — they reach them and open through the
/// director correctly — but the call they make *next*: `NtLockFile`, which had
/// no detour, so the real kernel was handed a synthetic handle and answered
/// `STATUS_INVALID_HANDLE`. The API then abandoned the read and returned the
/// caller's own default. The game got no INI data at all.
///
/// **Three outcomes, not two.** A real file with different content sits on
/// disk at the same under-root path, so the value the fixture reports
/// separates:
///
/// - `VIRTUAL` — the director served it. The only pass.
/// - `DISK` — something read the real file behind the mount: a containment
///   escape, and a *different* bug from the one this test was written for.
/// - `MISSING` — the fixture's own default came back: nothing read anything.
///   This is what the unfixed tree produces, verified before the fix landed.
///
/// A test that only asserted "not MISSING" would pass on an escape, and one
/// that only compared against the director's bytes without a decoy on disk
/// could not tell a served read from a passthrough at all.
#[tokio::test(flavor = "multi_thread")]
async fn profile_api_reads_a_managed_root_ini_through_the_director() {
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

    // CRLF: what an INI on Windows actually contains, and what the profile
    // API's own parser is fed in a live session.
    let virtual_ini = format!("[Display]\r\nsTest={PREFS_VIRTUAL}\r\niTest=42\r\n");
    let disk_ini = format!("[Display]\r\nsTest={PREFS_DISK}\r\niTest=7\r\n");

    let content_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(content_dir.path().join("prefs.ini"), virtual_ini.as_bytes()).unwrap();

    let out_dir = tempfile::tempdir().expect("out tempdir");
    let out_file = out_dir.path().join("prefs-out.tsv");
    let stats_dir = tempfile::tempdir().expect("stats tempdir");
    let stats_log = stats_dir.path().join("shim-stats.log");

    let fixture = locate_artifact("vfs-fixture-prefs.exe");
    let mut client = connect(&format!("{addr}")).await.expect("connect");

    let session = client
        .create_session(vfs_control::pb::CreateSessionReq {
            name: "prefs-read".into(),
        })
        .await
        .expect("CreateSession")
        .into_inner();

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
        })
        .await
        .expect("AddSource");

    // The decoy: a real file, physically present under the managed root, with
    // *different* content at the same path the fixture will ask for. Written
    // and read back from this never-injected harness process first, so
    // "the fixture did not see DISK" can never mean "DISK was never there".
    let root = PathBuf::from(&session.root);
    let ini_path = root.join("prefs.ini");
    std::fs::write(&ini_path, disk_ini.as_bytes()).expect("write on-disk decoy");
    let decoy_readback = std::fs::read(&ini_path).expect("read decoy back");
    assert_eq!(
        decoy_readback,
        disk_ini.as_bytes(),
        "the on-disk decoy must really hold DISK content before the fixture runs, or the \
         VIRTUAL-vs-DISK discrimination below proves nothing"
    );

    let mut env = std::collections::HashMap::new();
    env.insert(
        "VFS_FIXTURE_INI_PATH".to_string(),
        ini_path.to_string_lossy().into_owned(),
    );
    env.insert(
        "VFS_FIXTURE_INI_OUT".to_string(),
        out_file.to_string_lossy().into_owned(),
    );
    env.insert(
        "VFS_SHIM_STATS_LOG".to_string(),
        stats_log.to_string_lossy().into_owned(),
    );
    // Fast tick, same reason as the escape matrix's identical override: this
    // fixture's whole run is far under the reporter's 250ms default, and the
    // periodic reporter is the *only* writer of the report — nothing flushes
    // one at process exit (`vfs_shim::hookstats::banner` records why an exit
    // flush was built, measured, and removed). Paired with the fixture's own
    // end-of-run wait, which is what guarantees a tick lands after its last
    // call rather than merely somewhere during the run.
    env.insert("VFS_SHIM_STATS_INTERVAL_MS".to_string(), "5".to_string());

    let mut stream = client
        .launch(LaunchReq {
            session_id: session.id.clone(),
            exec: fixture.to_string_lossy().into_owned(),
            args: vec![],
            wait: true,
            env,
        })
        .await
        .expect("Launch")
        .into_inner();

    let mut exit_code = None;
    drain_launch_events(&mut stream, "prefs fixture log", &mut exit_code).await;
    assert_eq!(
        exit_code,
        Some(0),
        "the prefs fixture must exit 0 (it reports what it saw in its output file rather than \
         through its exit code; a nonzero exit means it could not run at all)"
    );

    let text = std::fs::read_to_string(&out_file).unwrap_or_default();
    let lines = parse_prefs_lines(&text);
    let reads: Vec<&PrefsLine> = lines.iter().filter(|l| l.op == "read").collect();
    assert_eq!(
        reads.len(),
        2,
        "expected two read lines from the fixture, got {lines:?} (raw: {text:?})"
    );

    for r in &reads {
        assert_ne!(
            r.value, PREFS_DEFAULT,
            "GetPrivateProfileStringW returned the caller's own default \
             ({PREFS_DEFAULT}, lasterror={}) for a file under a managed root: nothing served \
             the read at all. This is the NtLockFile signature — the open reaches the director, \
             the very next NT call on the synthetic handle is rejected by the real kernel, and \
             the profile API abandons the read. Lines: {lines:?}",
            r.lasterror
        );
        assert_ne!(
            r.value, PREFS_DISK,
            "the profile API read the *real* file behind the mount instead of the director's \
             content — a containment escape, not the lock bug. Lines: {lines:?}"
        );
        assert_eq!(
            r.value, PREFS_VIRTUAL,
            "the profile API returned neither the director's content, the real file's content, \
             nor its own default. Lines: {lines:?}"
        );
        assert_eq!(
            r.lasterror, "0",
            "a read that produced the right value must not also be reporting an error. \
             Lines: {lines:?}"
        );
    }

    // The primitive ladder: one assertion per NT call a synthetic handle has
    // to survive. The profile read above stops at the first failure, so
    // without these `NtUnlockFile` and `NtFlushBuffersFile` would have no
    // coverage at all — the read never gets far enough to reach them.
    for op in [
        "open",
        "getfiletype",
        "setfilepointer",
        "lockfile",
        "unlockfile",
        "flushbuffers",
        // `LockFileEx` with an OVERLAPPED event — the asynchronous-shaped
        // lock. A plain `LockFile` passes a null event, so only this one
        // exercises the completion the shim has to signal.
        "lockfileex",
    ] {
        let line = lines
            .iter()
            .find(|l| l.op == op)
            .unwrap_or_else(|| panic!("no `{op}` line from the fixture: {lines:?}"));
        assert_eq!(
            line.value, "ok",
            "`{op}` failed (lasterror={}) on a handle to a file under a managed root. A \
             synthetic handle is not a kernel object, so every NT call without a detour is \
             rejected outright — and the caller's next move is usually to abandon the file, \
             not to retry. Lines: {lines:?}",
            line.lasterror
        );
    }

    // The other half of the same question: which handles must **not** be
    // answered. A synthetic handle is marked by a single tag bit, and
    // `INVALID_HANDLE_VALUE` (-1) has every bit set — so a hook that decides on
    // the tag alone reports a lock taken on a handle nobody owns, and the same
    // for one closed a moment earlier. Both are worse than an error: they
    // convert a caller's failed open into a lock it believes it holds.
    for op in [
        "lockfile-closed",
        "unlockfile-closed",
        "flushbuffers-closed",
        "lockfile-invalid",
        "unlockfile-invalid",
        "flushbuffers-invalid",
    ] {
        let line = lines
            .iter()
            .find(|l| l.op == op)
            .unwrap_or_else(|| panic!("no `{op}` line from the fixture: {lines:?}"));
        assert_eq!(
            line.value, "fail",
            "`{op}` succeeded. The handle is either closed or was never valid, so the only \
             correct answer is failure — a synthetic-handle branch that keys off the tag bit \
             without resolving the handle grants locks on handles that do not exist. \
             Lines: {lines:?}"
        );
    }

    // The read must not have disturbed the real file either: the profile API
    // opens INIs for read/write in places, and a "successful" read that
    // rewrote the decoy would be a different failure wearing this test's pass.
    assert_eq!(
        std::fs::read(&ini_path).expect("re-read decoy"),
        disk_ini.as_bytes(),
        "the real on-disk file under the managed root was modified by a read-only profile-API call"
    );

    // ── the instrument, not only the bytes ──
    let report = std::fs::read_to_string(&stats_log).unwrap_or_default();
    assert!(
        !report.is_empty(),
        "the shim wrote no stats report at {stats_log:?}, so every assertion below would pass \
         or fail for the wrong reason"
    );

    // Asserted **before** anything is read out of the report: every report
    // this shim writes is a point-in-time snapshot (there is no exit flush —
    // `vfs_shim::hookstats::banner` records why one was tried and removed), so
    // a reader has to know which point. Measured during this task: a report
    // written before the fixture's own end-of-run wait genuinely lacks the
    // `NtLockFile` row, and every row assertion below would then fail for a
    // reason that has nothing to do with the hooks. Checking the banner first
    // is what keeps those failures readable.
    assert!(
        report.starts_with("SNAPSHOT at t+"),
        "the report does not open with its snapshot banner, so nothing below can tell \"this \
         hook never ran\" from \"this hook ran after the last tick\". First line: {:?}",
        report.lines().next().unwrap_or_default()
    );

    // Each hook's own row. `render` omits any hook with zero calls, so a
    // present row means the hook ran *and* was counted — which is the half
    // that was missing for NtSetInformationFile and NtQueryVolumeInformationFile
    // (neither had a `hookstats::Timed` guard, unlike every other hook, so
    // calls to them were invisible in this table).
    for hook in [
        "NtLockFile",
        "NtUnlockFile",
        "NtFlushBuffersFile",
        "NtSetInformationFile",
        "NtQueryVolumeInformationFile",
    ] {
        assert!(
            report.contains(hook),
            "the shim's hook table has no `{hook}` row, though the fixture demonstrably made \
             that call (its own ladder line above says it succeeded). An uncounted hook reads \
             as a hook that never ran. Report:\n{report}"
        );
    }

    // The deliberate semantic gap, made visible: locks granted on synthetic
    // handles are locks nobody holds, and the report says so by path.
    //
    // Sliced to the section rather than searched across the whole report:
    // `prefs.ini` appears in the trace and outcome sections of the same file
    // for entirely unrelated reasons, so a bare `report.contains` here would
    // assert nothing about the lock section at all.
    const SYNTH_LOCK_HEADER: &str = "synthetic byte-range locks";
    let idx = report.find(SYNTH_LOCK_HEADER).unwrap_or_else(|| {
        panic!(
            "no synthetic-lock section in the report, though the fixture locked a file under a \
             managed root. That section is the only record that a lock was granted without \
             being taken. Report:\n{report}"
        )
    });
    let section = report[idx..]
        .lines()
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    assert!(
        section.contains("lock-exclusive") || section.contains("lock-shared"),
        "the synthetic-lock section names no lock operation. Section:\n{section}"
    );
    assert!(
        section.contains("prefs.ini"),
        "the synthetic-lock section does not name the file that was locked, so it cannot answer \
         \"is anyone else locking this\" — the one question it exists for. Section:\n{section}"
    );

    // Completion shape. The shim signals a caller's event but never runs a
    // caller's APC, so a caller that waits on one hangs — and the async
    // section is the only place that shape is visible. `lock_hook` has to
    // classify its calls the way `read_hook`/`write_hook` classify theirs or
    // an APC-supplied lock is neither delivered nor counted.
    //
    // Attributable: every other synthetic-handle operation this fixture makes
    // is fully synchronous (null event), so the `with event` count can only
    // have come from the `LockFileEx` above.
    let with_event = report
        .lines()
        .find_map(|l| {
            let rest = l.trim().strip_prefix("reads:")?;
            let (_, after_apc) = rest.split_once('/')?;
            after_apc.trim().strip_suffix(" with event").or_else(|| {
                after_apc.split_once(" with event").map(|(n, _)| n)
            })
        })
        .and_then(|n| n.trim().parse::<u64>().ok())
        .unwrap_or_else(|| {
            panic!("could not read the async section's completion counts. Report:\n{report}")
        });
    assert!(
        with_event >= 1,
        "the async section counts no event-completed operations, though the fixture's \
         `LockFileEx` supplied one and reported success. An unclassified lock is one the \
         report cannot show waiting on a completion we never deliver. Report:\n{report}"
    );

    client
        .teardown_session(vfs_control::pb::TeardownReq {
            session_id: session.id,
        })
        .await
        .expect("teardown");
    server.abort();
}

/// The write half of the same mechanism: `WritePrivateProfileStringW` must
/// land its bytes **at the director**, not nowhere.
///
/// The live session that started the investigation showed 300 successful write
/// opens of `SkyrimPrefs.ini` and zero write operations. That is
/// `WritePrivateProfileStringW` hitting the same unhooked `NtLockFile` one
/// operation earlier than the read path does: the create succeeds, the lock is
/// rejected, and the API reports failure to its caller having written nothing.
///
/// The decisive assertion is not that the value reads back — a shim-local
/// overlay or the real file behind the mount would satisfy that too — but that
/// the **provider's own backing file** on disk holds the new value while the
/// decoy under the session root is untouched.
#[tokio::test(flavor = "multi_thread")]
async fn profile_api_writes_a_managed_root_ini_through_the_director() {
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

    const PREFS_WRITTEN: &str = "WRITTEN";
    let virtual_ini = format!("[Display]\r\nsTest={PREFS_VIRTUAL}\r\niTest=42\r\n");
    let disk_ini = format!("[Display]\r\nsTest={PREFS_DISK}\r\niTest=7\r\n");

    let content_dir = tempfile::tempdir().expect("tempdir");
    let backing = content_dir.path().join("prefs.ini");
    std::fs::write(&backing, virtual_ini.as_bytes()).unwrap();

    let out_dir = tempfile::tempdir().expect("out tempdir");
    let out_file = out_dir.path().join("prefs-write-out.tsv");

    let fixture = locate_artifact("vfs-fixture-prefs.exe");
    let mut client = connect(&format!("{addr}")).await.expect("connect");

    let session = client
        .create_session(vfs_control::pb::CreateSessionReq {
            name: "prefs-write".into(),
        })
        .await
        .expect("CreateSession")
        .into_inner();

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
            write_layer: true,
        })
        .await
        .expect("AddSource");

    let root = PathBuf::from(&session.root);
    let ini_path = root.join("prefs.ini");
    std::fs::write(&ini_path, disk_ini.as_bytes()).expect("write on-disk decoy");

    let mut env = std::collections::HashMap::new();
    env.insert(
        "VFS_FIXTURE_INI_PATH".to_string(),
        ini_path.to_string_lossy().into_owned(),
    );
    env.insert(
        "VFS_FIXTURE_INI_OUT".to_string(),
        out_file.to_string_lossy().into_owned(),
    );
    env.insert("VFS_FIXTURE_INI_WRITE".to_string(), PREFS_WRITTEN.to_string());

    let mut stream = client
        .launch(LaunchReq {
            session_id: session.id.clone(),
            exec: fixture.to_string_lossy().into_owned(),
            args: vec![],
            wait: true,
            env,
        })
        .await
        .expect("Launch")
        .into_inner();

    let mut exit_code = None;
    drain_launch_events(&mut stream, "prefs fixture log", &mut exit_code).await;
    assert_eq!(exit_code, Some(0), "the prefs fixture must exit 0");

    let text = std::fs::read_to_string(&out_file).unwrap_or_default();
    let lines = parse_prefs_lines(&text);
    let write = lines
        .iter()
        .find(|l| l.op == "write")
        .unwrap_or_else(|| panic!("no `write` line from the fixture: {lines:?} (raw: {text:?})"));
    assert_eq!(
        write.value, "ok",
        "WritePrivateProfileStringW reported failure (lasterror={}) for a file under a managed \
         root. lasterror 6 (ERROR_INVALID_HANDLE) is the unhooked-NT-call signature: the create \
         reached the director and the very next call on the synthetic handle did not. \
         Lines: {lines:?}",
        write.lasterror
    );

    for r in lines.iter().filter(|l| l.op == "read") {
        assert_eq!(
            r.value, PREFS_WRITTEN,
            "the value written through the profile API did not read back through it. \
             Lines: {lines:?}"
        );
    }

    // The decisive pair. The provider's own file must hold the new value —
    // that is what "the write crossed the ring" means, and neither a
    // shim-local overlay nor the real file behind the mount can produce it.
    let backing_after = std::fs::read_to_string(&backing).expect("read provider backing file");
    assert!(
        backing_after.contains(PREFS_WRITTEN),
        "the director's backing file does not contain the written value, so the write landed \
         somewhere else (or nowhere). Backing file now: {backing_after:?}"
    );
    // …and the real file physically under the session root must be untouched.
    assert_eq!(
        std::fs::read(&ini_path).expect("re-read decoy"),
        disk_ini.as_bytes(),
        "the write escaped to the real file under the managed root instead of going to the \
         director"
    );

    client
        .teardown_session(vfs_control::pb::TeardownReq {
            session_id: session.id,
        })
        .await
        .expect("teardown");
    server.abort();
}

/// The suffix `vfs-fixture-escape`'s **write-mode** vector 14 appends to the
/// canary's own path — mirrored from that crate's `V14_WRITE_SUFFIX`, which
/// documents why that one vector moves off the target in write mode.
///
/// This harness needs the name in order to *tolerate* it in the real-disk
/// listing, and only it. Vector 14's containment rests on the shim's
/// `CreateProcessInternalW` hook injecting the child (`vfs-shim/src/hook.rs`),
/// which is explicitly best-effort — it force-suspends, injects, and gives up
/// on a timeout. Observed here the child *is* injected and its write is
/// answered by the director like any other (see this file's
/// `negative_write_expectation` and the finding recorded in
/// `rust/docs/escape-matrix.md`), so this file does not normally appear on
/// disk at all. But a timed-out inject would leave it there through no fault
/// of the canonicaliser, and turning that into a flaky containment failure
/// would teach the next person to weaken the assertion. It is the one name
/// this harness accepts; anything else in the directory is an escape.
const V14_WRITE_SUFFIX: &str = ".v14-child-write.txt";

/// The alternate-stream name `vfs-fixture-escape`'s vector 11 builds. In
/// write mode that vector uses a creating disposition, so a spelling that
/// escapes leaves a real named stream on the canary — a create on real disk
/// that no directory listing would ever show.
const ADS_PROBE_STREAM: &str = "vfs-escape-fixture-probe";

/// The positive canary's expected outcome for a **write**, or `None` for a
/// vector this test does not assert an exact outcome for.
///
/// Same three exemptions the read matrix carries, for the same documented
/// reasons — `5b`'s construction fails at the NT level whatever the target,
/// and `13`/`14` are reported-not-closed in this gate. Every other buildable
/// spelling must come back `written`: opened for write, this vector's own
/// payload written, and that exact payload read back through the same
/// spelling (see `vfs-fixture-escape`'s module doc for why `written` is a
/// read-back claim and not merely "the call succeeded").
///
/// This half is not decoration. Gate 4 has already produced the failure it
/// guards against once — closing the write fall-through silently removed
/// copy-on-write while 526 tests stayed green. A containment matrix that only
/// asserted refusals would pass with every write broken.
fn positive_write_expectation(vector: &str) -> Option<&'static str> {
    if vector == "5b" || is_reported_not_closed(vector) {
        return None;
    }
    Some("written")
}

/// The negative canary's expected outcome for a **write**: `not-found`, for
/// every buildable spelling, with the same three exemptions as above.
///
/// Spec §8 criterion 1's load-bearing clause — "a write to the negative
/// canary must be **blocked**, and must not create a file on the real
/// filesystem under the root". The status half is this table; the filesystem
/// half is asserted separately, from this (uninjected) harness process, after
/// the run.
///
/// **Why the negative canary sits outside every mount here, rather than
/// inside a root mount whose backing directory merely lacks it** — the
/// construction the read matrix uses. A create is not a read: the read matrix
/// can put a writable `DiskProvider` at `/` and still call a file it does not
/// hold "unserved", because a read of an absent name is a refusal. A *create*
/// of that same name under a writable mount is something the provider graph
/// legitimately accepts and stores — contained, but not blocked. "A path no
/// provider serves" therefore means something stricter once writes are in
/// scope: no writable mount covers it at all. So the source here mounts at
/// `/Games/Skyrim/Data` and the negative canary lives in a sibling directory
/// under the same managed root, physically on disk, reachable by every one of
/// the fourteen spellings and owned by nothing.
fn negative_write_expectation(vector: &str) -> Option<&'static str> {
    if vector == "5b" || is_reported_not_closed(vector) {
        return None;
    }
    Some("not-found")
}

/// Every name directly inside `dir` on the **real** filesystem. Called only
/// from this test harness process, which is never injected, so `read_dir`
/// here answers about physical disk rather than about the provider graph.
fn real_dir_names(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {dir:?}: {e}"))
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

/// The only two names this harness accepts in a canary directory's real-disk
/// listing after a write run: the canary it put there itself, and vector 14's
/// best-effort-injected child (see [`V14_WRITE_SUFFIX`]).
///
/// Vector 8's hardlink is deliberately **not** on this list. It is created by
/// `CreateHardLinkW`, which the shim does not hook by name — but the NT opens
/// underneath it are hooked, and observed behaviour is that the link either
/// never comes into existence on real disk (the negative canary, where
/// `hard_link` fails outright and the vector reports `unbuildable:`) or is
/// gone again by the end of the run. Accepting the name "just in case" would
/// mean a real-disk create could appear here forever without anyone noticing,
/// which is the opposite of what this function is for. If it ever does show
/// up, this test should fail and someone should find out why.
fn accounted_for_on_real_disk(name: &str, canary: &str) -> bool {
    name == canary || name == format!("{canary}{V14_WRITE_SUFFIX}")
}

/// Create a junction at a fresh temp path pointing at `target`, from this
/// (never-injected) harness process. See `VFS_ESCAPE_VECTOR7_LINK_DIR` in
/// `vfs-env` for why vector 7's junction must pre-date the fixture launch.
fn make_escape_junction(tag: &str, target: &Path) -> (PathBuf, Option<String>) {
    let link = std::env::temp_dir().join(format!("vfs-escape-junction-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir(&link);
    let ready = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J", &link.to_string_lossy(), &target.to_string_lossy()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let dir = ready.then(|| link.to_string_lossy().into_owned());
    (link, dir)
}

/// **The write half of the canary matrix** (gate 4, Task 8).
///
/// Spec §8 criterion 1 asks for the matrix green for *write* access as well
/// as read: 14 spellings × 2 canaries, unbuildable vectors reported as
/// unbuildable, and — the clause that carries the weight — "a write to the
/// negative canary must be **blocked**, and must not create a file on the
/// real filesystem under the root".
///
/// A separate test from the read matrix rather than a second pass inside it,
/// for a reason that is structural and not about runtime: the two need
/// different mount geometry. See `negative_write_expectation`'s doc comment —
/// a writable mount at `/` makes a create of *any* under-root name something
/// the provider graph accepts, so the read matrix's negative canary (a name
/// its backing directory merely lacks) is not unserved for writes at all. The
/// source here mounts at `/Games/Skyrim/Data`; the negative canary lives in a
/// sibling directory no mount covers.
///
/// **The real-filesystem assertions are the point, and they are made from
/// this process.** A write that is refused at the API while still leaving a
/// zero-byte file under the root has breached containment and reported
/// success. `vfs-directord`'s test harness is never injected, so `read_dir`,
/// `exists` and `read` here answer about physical disk — the equivalent of
/// `write_seal.rs`'s `drop(hooks)` before it inspects the root, and stronger,
/// because there is no detour in this process to drop. Four things are
/// checked after the negative run:
///
/// 1. the canary is present in its directory at all — the guard that makes
///    the three absences below mean something, because every one of them
///    would also hold against an empty lookalike directory;
/// 2. its bytes are byte-identical to what this harness wrote — no write
///    reached it, and no truncating open emptied it;
/// 3. that directory holds nothing besides the canary and the one artefact
///    `accounted_for_on_real_disk` tolerates — no spelling created a file;
/// 4. no named stream was created on it (vector 11 writes with a creating
///    disposition, and a stream is a create no directory listing shows).
///
/// See `assert_no_escaped_real_files` for the rest of what stops those
/// absences being vacuous: the writability probe below, the twenty-two asserted
/// outcome lines, and the standalone run of this same fixture that *does*
/// produce the artefacts 3 and 4 forbid.
///
/// The canaries sit in directories this harness proves physically writable
/// first, by creating and deleting a probe file in each. A "nothing was
/// created" assertion against a directory that could not be written to
/// anyway establishes nothing.
///
/// The positive canary is mirrored: identical seed bytes in the provider's
/// backing store *and* physically under the managed root. That pairing is
/// what makes both halves of its check meaningful — the writes must land in
/// the provider's copy (asserted on `content_dir`) and must not touch the
/// physical one (asserted on `session.root`), and every vector stays
/// buildable because the physical file the 8.3-name and hardlink
/// constructions need is really there.
#[tokio::test(flavor = "multi_thread")]
async fn escape_matrix_write_access_positive_and_negative_canary() {
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

    let content_dir = tempfile::tempdir().expect("tempdir");
    let stats_dir = tempfile::tempdir().expect("stats tempdir");
    let stats_log = stats_dir.path().join("shim-stats.log");
    let out_dir = tempfile::tempdir().expect("out tempdir");
    let out_file = out_dir.path().join("escape-write-out.tsv");

    let fixture = locate_artifact("vfs-fixture-escape.exe");
    let mut client = connect(&format!("{addr}")).await.expect("connect");

    let session = client
        .create_session(vfs_control::pb::CreateSessionReq {
            name: "escape-matrix-write".into(),
        })
        .await
        .expect("CreateSession")
        .into_inner();
    assert!(!session.id.is_empty());
    assert!(!session.root.is_empty());

    use vfs_control::pb::{source_spec, AddSourceReq, DiskSource, SourceSpec as PbSource};

    // Mounted at a sub-path, deliberately — see the test's own doc comment.
    // Everything under `Games/Skyrim/Data` is served (and writable, since a
    // `DiskProvider` declares `Access::ReadWrite`); everything else under the
    // managed root is owned by no provider at all.
    const SERVED_MOUNT: &str = "/Games/Skyrim/Data";
    client
        .add_source(AddSourceReq {
            session_id: session.id.clone(),
            source: Some(PbSource {
                kind: Some(source_spec::Kind::Disk(DiskSource {
                    path: content_dir.path().to_string_lossy().into_owned(),
                })),
            }),
            mount: SERVED_MOUNT.into(),
            layer: 0,
            root: 0,
            write_layer: false,
        })
        .await
        .expect("AddSource");

    let root = PathBuf::from(&session.root);
    let served_sub = PathBuf::from("Games").join("Skyrim").join("Data");
    let unserved_sub = PathBuf::from("Games").join("Skyrim").join("Unserved");
    std::fs::create_dir_all(root.join(&served_sub)).expect("mkdir served dir under session root");
    std::fs::create_dir_all(root.join(&unserved_sub)).expect("mkdir unserved dir under session root");

    // Both seeds are shorter than the fixture's fixed 22-byte write payload,
    // which matters: the write disposition is `OPEN_ALWAYS` (create, never
    // truncate), so a longer seed would leave a tail behind and every
    // read-back would mismatch for a reason unrelated to containment. See
    // `write_payload` in `vfs-fixture-escape`.
    const POSITIVE_BASENAME: &str = "escape-write-positive-canary.esp";
    const POSITIVE_SEED: &[u8] = b"pos-seed";
    const NEGATIVE_BASENAME: &str = "escape-write-negative-canary.bin";
    const NEGATIVE_SEED: &[u8] = b"neg-seed";
    /// Every write payload the fixture produces starts with this — see
    /// `write_payload`. Used to recognise "a canary write landed here"
    /// without pinning which vector wrote last.
    const PAYLOAD_PREFIX: &str = "vfs-escape-write[";

    let pos_on_disk = root.join(&served_sub).join(POSITIVE_BASENAME);
    let pos_in_provider = content_dir.path().join(POSITIVE_BASENAME);
    std::fs::write(&pos_on_disk, POSITIVE_SEED).expect("write positive canary (session root)");
    std::fs::write(&pos_in_provider, POSITIVE_SEED).expect("write positive canary (content dir)");

    let neg_on_disk = root.join(&unserved_sub).join(NEGATIVE_BASENAME);
    std::fs::write(&neg_on_disk, NEGATIVE_SEED).expect("write negative canary");

    // Both canary directories must be physically writable from here, or every
    // "nothing was created on real disk" assertion below is satisfied by the
    // filesystem rather than by containment.
    for dir in [root.join(&served_sub), root.join(&unserved_sub)] {
        let probe = dir.join(".harness-writability-probe");
        std::fs::write(&probe, b"x").unwrap_or_else(|e| {
            panic!(
                "{dir:?} must be physically writable for this test to prove anything — a create \
                 that could not have succeeded anyway is not evidence of containment: {e}"
            )
        });
        std::fs::remove_file(&probe).expect("remove writability probe");
    }

    // One junction per canary directory: vector 7 opens `<junction>\<target
    // filename>`, so a junction pointing at the served directory cannot serve
    // the unserved canary's run.
    let (pos_link, pos_link_dir) = make_escape_junction("write-pos", &root.join(&served_sub));
    let (neg_link, neg_link_dir) = make_escape_junction("write-neg", &root.join(&unserved_sub));

    // ---------------------------------------------------------------
    // Positive canary: every buildable spelling writes, and reads its own
    // payload back through the same spelling.
    // ---------------------------------------------------------------
    let pos_ctx = EscapeFixtureCtx {
        session_id: &session.id,
        fixture: &fixture,
        stats_log: &stats_log,
        vector7_link_dir: pos_link_dir.as_deref(),
        write_access: true,
    };
    let (pos_exit, pos_lines, _pos_classified, pos_truncated) =
        run_escape_fixture(&mut client, &pos_ctx, &pos_on_disk, &out_file, None).await;
    if std::env::var("VFS_TEST_MATRIX_DUMP").is_ok() {
        eprintln!("=== POSITIVE WRITE lines ===");
        for l in &pos_lines {
            eprintln!("{}\t{}\t{}\t{}", l.vector, l.spelling, l.outcome, l.note);
        }
    }
    assert_eq!(
        pos_exit, 0,
        "vfs-fixture-escape (write mode) must exit 0 against the positive canary — a crash exit \
         means a vector took the process down before the rest of the matrix was attempted. Lines \
         captured: {pos_lines:?}"
    );
    assert!(!pos_truncated, "the shim report's path list truncated on the positive write run");
    for id in ALL_VECTOR_IDS {
        assert!(
            pos_lines.iter().any(|l| &l.vector == id),
            "positive canary, write access: vector {id} produced no line at all in {out_file:?} — \
             a missing line must never be readable as a pass"
        );
    }
    for line in &pos_lines {
        let Some(want) = positive_write_expectation(&line.vector) else { continue };
        if line.outcome.starts_with("unbuildable:") {
            continue; // First-class, environment-dependent; recorded, not a failure.
        }
        assert_eq!(
            line.outcome, want,
            "positive canary, write access, vector {}: expected `{want}` — legitimate writes must \
             keep working through every spelling, or containment has been bought by breaking the \
             filesystem. Got `{}` (spelling: {:?}, note: {:?})",
            line.vector, line.outcome, line.spelling, line.note
        );
    }
    // The writes landed in the provider's store …
    let served_bytes = std::fs::read(&pos_in_provider).expect("read positive canary in provider");
    assert!(
        String::from_utf8_lossy(&served_bytes).starts_with(PAYLOAD_PREFIX),
        "the positive canary's writes must land where the provider graph says they land; \
         {pos_in_provider:?} still holds {:?}",
        String::from_utf8_lossy(&served_bytes)
    );
    // … and nowhere near the byte-identical physical file under the root.
    assert_eq!(
        std::fs::read(&pos_on_disk).expect("read positive canary on real disk"),
        POSITIVE_SEED,
        "a write reached the real file at {pos_on_disk:?} under the managed root. The provider \
         serves this vpath, so every write had somewhere legitimate to go — landing here instead \
         means a spelling escaped to disk"
    );
    assert_no_escaped_real_files(
        &root.join(&served_sub),
        POSITIVE_BASENAME,
        &pos_on_disk,
        "positive canary",
    );

    // ---------------------------------------------------------------
    // Negative canary: a real file under the managed root that no mount
    // covers. Every buildable spelling's write is refused, and real disk is
    // untouched.
    // ---------------------------------------------------------------
    let neg_ctx = EscapeFixtureCtx {
        session_id: &session.id,
        fixture: &fixture,
        stats_log: &stats_log,
        vector7_link_dir: neg_link_dir.as_deref(),
        write_access: true,
    };
    let (neg_exit, neg_lines, _neg_classified, neg_truncated) =
        run_escape_fixture(&mut client, &neg_ctx, &neg_on_disk, &out_file, None).await;
    if std::env::var("VFS_TEST_MATRIX_DUMP").is_ok() {
        eprintln!("=== NEGATIVE WRITE lines ===");
        for l in &neg_lines {
            eprintln!("{}\t{}\t{}\t{}", l.vector, l.spelling, l.outcome, l.note);
        }
    }
    assert_eq!(
        neg_exit, 0,
        "vfs-fixture-escape (write mode) must exit 0 against the negative canary too. Lines \
         captured: {neg_lines:?}"
    );
    assert!(!neg_truncated, "the shim report's path list truncated on the negative write run");
    for id in ALL_VECTOR_IDS {
        assert!(
            neg_lines.iter().any(|l| &l.vector == id),
            "negative canary, write access: vector {id} produced no line at all in {out_file:?}"
        );
    }
    for line in &neg_lines {
        let Some(want) = negative_write_expectation(&line.vector) else { continue };
        if line.outcome.starts_with("unbuildable:") {
            continue; // Never attempted at the OS level; nothing to seal.
        }
        assert_eq!(
            line.outcome, want,
            "negative canary, write access, vector {}: expected `{want}` — a write to a path \
             under the managed root that no provider serves must be blocked, for every buildable \
             spelling. Got `{}` (spelling: {:?}, note: {:?})",
            line.vector, line.outcome, line.spelling, line.note
        );
    }
    // The filesystem half of spec §8 criterion 1, asserted on real disk from
    // this never-injected process.
    assert_eq!(
        std::fs::read(&neg_on_disk).expect("read negative canary on real disk"),
        NEGATIVE_SEED,
        "the negative canary's real bytes at {neg_on_disk:?} changed. A refusal at the API that \
         still modifies the file under the root is the breach this whole matrix exists to catch"
    );
    assert_no_escaped_real_files(
        &root.join(&unserved_sub),
        NEGATIVE_BASENAME,
        &neg_on_disk,
        "negative canary",
    );

    client
        .teardown_session(vfs_control::pb::TeardownReq {
            session_id: session.id,
        })
        .await
        .expect("teardown");

    server.abort();
    let _ = std::fs::remove_dir(&pos_link);
    let _ = std::fs::remove_dir(&neg_link);
}

/// The real-filesystem half of the write matrix, for one canary: nothing was
/// created in its directory and no named stream was created on it.
///
/// **Called with the detours nowhere in sight.** This is the `vfs-directord`
/// test process, which is never injected, so every `std::fs` call here reads
/// physical disk — the same ordering `write_seal.rs` gets by doing
/// `drop(hooks)` before it inspects the root, and stronger, because there is
/// no detour in this process to drop in the first place. A hook-live
/// `exists()` answers about the provider graph, which is exactly the answer a
/// breached containment layer would want it to give.
///
/// **What stops these absences from being vacuous**, in order:
///
/// - the canary itself must be in the listing, so this is provably the
///   physical directory the run targeted and not an empty lookalike;
/// - the caller proved that directory physically writable, by creating and
///   deleting a probe file in it before the run, so a create here genuinely
///   could have succeeded;
/// - the fixture reported an attempted spelling for all twenty-two lines, each
///   naming a path in this directory, and the caller asserted on every one of
///   them;
/// - and the same fixture in the same write mode, run standalone against an
///   ordinary directory, *does* create `<name>.`, `<name> ` and the named
///   stream (vectors 10b, 10c and 11 with a creating disposition). These
///   assertions have teeth; they were watched failing before they were
///   watched passing.
fn assert_no_escaped_real_files(dir: &Path, canary: &str, canary_path: &Path, label: &str) {
    let names = real_dir_names(dir);

    assert!(
        names.iter().any(|n| n == canary),
        "{label}: {canary:?} is not in {dir:?} at all ({names:?}) — this harness is not looking \
         at the physical directory the run targeted, so every `nothing was created` assertion \
         below would pass for the wrong reason"
    );

    let stray: Vec<&String> =
        names.iter().filter(|n| !accounted_for_on_real_disk(n, canary)).collect();
    assert!(
        stray.is_empty(),
        "{label}: these files appeared on the REAL filesystem under the managed root at {dir:?}: \
         {stray:?}. Every one of them is a spelling whose write was answered by disk instead of \
         by the director — spec §8 criterion 1's `must not create a file on the real filesystem \
         under the root`. (Trailing-dot and trailing-space spellings show up here as names that \
         look identical to the canary's; a doubled entry is vector 10b or 10c escaping.)"
    );

    let stream = format!("{}:{ADS_PROBE_STREAM}", canary_path.display());
    assert!(
        std::fs::File::open(&stream).is_err(),
        "{label}: vector 11 created the named stream {stream:?} on the real file under the \
         managed root. A stream is a create that no directory listing shows, which is exactly \
         why it is checked by name rather than left to the listing above"
    );
}

/// Stage 2b exit criterion: **the escape matrix passes against every root,
/// not just the first.**
///
/// `escape_matrix_positive_and_negative_canary` above proves containment for
/// root 0 — the session's own root, the one the daemon creates and the one
/// every path in this tree used to be measured against. That proves nothing
/// about a second root, and the failure it would miss is not subtle: the
/// canonicaliser could have a root-index assumption baked into it (matching
/// only `roots[0]`, or resolving device/junction aliases against root 0's
/// path alone) and root 0's matrix would stay green while every path under
/// root 1 fell through to real disk, unclassified and uncounted.
///
/// So this runs the same fixture, the same two canaries, and the same
/// `positive_expectation`/`negative_expectation` tables against a target
/// under **root 1**: a second real host directory, declared with
/// `SessionRegistry::declare_root` and served by its own provider mounted at
/// `RootId(1)` through the ordinary `AddSourceReq { root: 1 }` path.
///
/// It exercises the whole chain end to end and nothing about it is stubbed:
/// the daemon publishes root 1 into `VFS_VIRTUAL_ROOTS`, the shim's
/// `RootMap` holds both roots, `vpath_under_root` answers `RootId(1)`, the
/// ring payload carries that 1, and `dispatch_director` routes on it. Any
/// link missing turns the positive canary's ordinary spelling into
/// `not-found`, which is what makes this worth its runtime rather than a
/// duplicate of the root-0 run.
#[tokio::test(flavor = "multi_thread")]
async fn escape_matrix_holds_against_a_second_root() {
    let _guard = LAUNCH_LOCK.lock().await;
    ensure_inject_artifacts();

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr: SocketAddr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    let registry = SessionRegistry::new();
    // Cloned before the service takes it: `declare_root` has no RPC of its own
    // (a root's *host path* comes from a config's `[[root]] path`, and
    // `AddSourceReq` carries a root id and no path), so the test declares it
    // the same way a config-driven daemon would. Everything else here — the
    // session, the source on root 1, the launch — goes over gRPC.
    let reg_handle = registry.clone();
    let svc = DirectorService::new(registry);
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(DirectorServer::new(svc))
            .serve_with_incoming(incoming)
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Root 1's own host directory — the "Documents\My Games\Skyrim" shape —
    // and its own backing content dir, deliberately separate so the negative
    // canary is a real file under root 1 that root 1's provider does not have.
    let docs_root = tempfile::tempdir().expect("docs root tempdir");
    let docs_content = tempfile::tempdir().expect("docs content tempdir");
    let stats_dir = tempfile::tempdir().expect("stats tempdir");
    let stats_log = stats_dir.path().join("shim-stats.log");
    let out_dir = tempfile::tempdir().expect("out tempdir");
    let out_file = out_dir.path().join("escape-root1-out.tsv");

    let fixture = locate_artifact("vfs-fixture-escape.exe");
    let mut client = connect(&format!("{addr}")).await.expect("connect");

    let session = client
        .create_session(vfs_control::pb::CreateSessionReq {
            name: "escape-matrix-root1".into(),
        })
        .await
        .expect("CreateSession")
        .into_inner();

    use vfs_control::pb::{source_spec, AddSourceReq, DiskSource, SourceSpec as PbSource};

    // Root 0 still gets a provider: a session whose game directory serves
    // nothing is not the shape being tested, and leaving it unmounted would
    // let a root-0 regression hide here.
    let game_content = tempfile::tempdir().expect("game content tempdir");
    client
        .add_source(AddSourceReq {
            session_id: session.id.clone(),
            source: Some(PbSource {
                kind: Some(source_spec::Kind::Disk(DiskSource {
                    path: game_content.path().to_string_lossy().into_owned(),
                })),
            }),
            mount: "/".into(),
            layer: 0,
            root: 0,
            write_layer: false,
        })
        .await
        .expect("AddSource root 0");

    client
        .add_source(AddSourceReq {
            session_id: session.id.clone(),
            source: Some(PbSource {
                kind: Some(source_spec::Kind::Disk(DiskSource {
                    path: docs_content.path().to_string_lossy().into_owned(),
                })),
            }),
            mount: "/".into(),
            layer: 0,
            root: 1,
            write_layer: false,
        })
        .await
        .expect("AddSource root 1");

    reg_handle
        .declare_root(&session.id, 1, docs_root.path())
        .expect("declare root 1");
    assert!(
        reg_handle.declare_root(&session.id, 0, docs_root.path()).is_err(),
        "root 0 is the session's own root and must not be re-declarable"
    );

    let sub = PathBuf::from("Saves");
    std::fs::create_dir_all(docs_root.path().join(&sub)).expect("mkdir under root 1");
    std::fs::create_dir_all(docs_content.path().join(&sub)).expect("mkdir under root 1 content");

    // Same two-canary construction as the root-0 matrix, one root over.
    const POSITIVE_BASENAME: &str = "escape-positive-canary.esp";
    const POSITIVE_BYTES: &[u8] = b"the-positive-canary-bytes";
    let pos_rel = sub.join(POSITIVE_BASENAME);
    std::fs::write(docs_root.path().join(&pos_rel), POSITIVE_BYTES).expect("positive (root 1)");
    std::fs::write(docs_content.path().join(&pos_rel), POSITIVE_BYTES)
        .expect("positive (root 1 content)");

    const NEGATIVE_BASENAME: &str = "escape-negative-canary.bin";
    let neg_rel = sub.join(NEGATIVE_BASENAME);
    std::fs::write(docs_root.path().join(&neg_rel), b"the-negative-canary-bytes")
        .expect("negative (root 1)");

    // Vector 7's junction, created by this never-injected harness process for
    // the same reason the root-0 matrix does it here — pointed at root 1's
    // own directory, which is the part that would break if junction aliases
    // were resolved against root 0's path alone.
    let vector7_link =
        std::env::temp_dir().join(format!("vfs-escape-junction-root1-{}", std::process::id()));
    let _ = std::fs::remove_dir(&vector7_link);
    let vector7_link_ready = std::process::Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            &vector7_link.to_string_lossy(),
            &docs_root.path().join(&sub).to_string_lossy(),
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let vector7_link_dir = vector7_link_ready.then(|| vector7_link.to_string_lossy().into_owned());
    let ctx = EscapeFixtureCtx {
        session_id: &session.id,
        fixture: &fixture,
        stats_log: &stats_log,
        vector7_link_dir: vector7_link_dir.as_deref(),
        write_access: false,
    };

    let (pos_exit, pos_lines, _, _) = run_escape_fixture(
        &mut client,
        &ctx,
        &docs_root.path().join(&pos_rel),
        &out_file,
        None,
    )
    .await;
    assert_eq!(
        pos_exit, 0,
        "vfs-fixture-escape must exit 0 against root 1's positive canary. Lines: {pos_lines:?}"
    );
    for id in ALL_VECTOR_IDS {
        assert!(
            pos_lines.iter().any(|l| &l.vector == id),
            "root 1 positive canary: vector {id} produced no line at all — a missing line must \
             never be readable as a pass"
        );
    }
    for line in &pos_lines {
        let Some(want) = positive_expectation(&line.vector) else { continue };
        if line.outcome.starts_with("unbuildable:") {
            continue;
        }
        assert_eq!(
            line.outcome, want,
            "root 1 positive canary, vector {}: expected `{want}`, got `{}` (spelling: {:?}, \
             note: {:?}). A blanket `not-found` here means root 1 never reached the director at \
             all — the shim did not learn the root, or the ring did not carry it.",
            line.vector, line.outcome, line.spelling, line.note
        );
    }

    let (neg_exit, neg_lines, _, _) = run_escape_fixture(
        &mut client,
        &ctx,
        &docs_root.path().join(&neg_rel),
        &out_file,
        None,
    )
    .await;
    assert_eq!(
        neg_exit, 0,
        "vfs-fixture-escape must exit 0 against root 1's negative canary. Lines: {neg_lines:?}"
    );
    for line in &neg_lines {
        let Some(want) = negative_expectation(&line.vector) else { continue };
        if line.outcome.starts_with("unbuildable:") {
            continue;
        }
        assert_eq!(
            line.outcome, want,
            "root 1 negative canary, vector {}: expected `{want}`, got `{}` (spelling: {:?}, \
             note: {:?}). `opened` here means a real file under root 1 that no provider serves \
             is still reachable — containment holds for root 0 and not for root 1.",
            line.vector, line.outcome, line.spelling, line.note
        );
    }

    client
        .teardown_session(vfs_control::pb::TeardownReq {
            session_id: session.id,
        })
        .await
        .expect("teardown");

    server.abort();
    if vector7_link_ready {
        let _ = std::fs::remove_dir(&vector7_link);
    }
}

/// **The gap this test recorded is closed, and this is the flip.** It was
/// `documents_metadata_gap_for_unrecognised_spellings`, and it asserted
/// `found`.
///
/// What it recorded: Fix 2(b) from the final whole-branch review of Gate 3
/// found `docs/escape-matrix.md`'s claim of containment for metadata queries
/// "by the same `RootMap::decide` mechanism... regardless of which hook
/// asked" to be false. `qattr_hook`/`qfull_hook`/`qibn_hook`
/// (`vfs-shim/src/hook.rs`) never reach `RootMap::decide` at all — they
/// consult `fuse_path_attr`, which asked `fuse_client::vpath_under_root`,
/// the *client's own* string-prefix predicate. That predicate had none of
/// `RootMap::compute_under_root`'s canonicalisation tables (no
/// device-prefix, volume-GUID, `GLOBALROOT`-unwrap, UNC-admin-share, or
/// junction-alias resolution), so five alternate spellings of an in-root
/// path were classified by one predicate and never routed by the other —
/// and a name-based attribute query on one of them reached real disk, even
/// though the matching *read open* on the identical spelling (vector 4
/// itself) was already sealed.
///
/// What changed: stage 2b task 5 **deleted the second predicate**.
/// `FuseClient` now holds a real `RootMap` — several roots, plus the staged
/// launch directory as an alias for root 0 — and `vpath_under_root` is that
/// map's canonicalising `resolve`, so there is one predicate rather than two
/// that can drift. The volume-GUID spelling this test builds is now
/// recognised by the client, routed to the director, and — since the
/// negative canary is a real file on `session.root` that no provider serves
/// — answered `not-found` rather than handed to real disk.
///
/// Kept rather than deleted, and kept in its original shape, because it is
/// the only end-to-end evidence that the unification reaches this hook
/// family: it launches `vfs-fixture-escape`'s opt-in `4m` vector
/// (`GetFileAttributesW` against vector 4's own volume-GUID spelling) under
/// a real, composed session, against a real on-disk negative canary. Revert
/// the unification and this assertion fails again, which is what makes it
/// worth its runtime.
///
/// **If this ever reads `found` again**, the client predicate has lost its
/// canonicalisation. Do not relax the assertion — find what stopped
/// consulting `RootMap`.
#[tokio::test(flavor = "multi_thread")]
async fn metadata_queries_are_sealed_for_canonicaliser_only_spellings() {
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

    // The DiskProvider's backing store — deliberately NOT session.root, same
    // shape as the escape matrix test's own negative canary: a real file
    // under the managed root that this provider genuinely does not have.
    let content_dir = tempfile::tempdir().expect("tempdir");
    let stats_dir = tempfile::tempdir().expect("stats tempdir");
    let stats_log = stats_dir.path().join("shim-stats.log");
    let out_dir = tempfile::tempdir().expect("out tempdir");
    let out_file = out_dir.path().join("metadata-gap-out.tsv");

    let fixture = locate_artifact("vfs-fixture-escape.exe");
    let mut client = connect(&format!("{addr}")).await.expect("connect");

    let session = client
        .create_session(vfs_control::pb::CreateSessionReq {
            name: "metadata-gap".into(),
        })
        .await
        .expect("CreateSession")
        .into_inner();
    assert!(!session.id.is_empty());
    assert!(!session.root.is_empty());

    use vfs_control::pb::{source_spec, AddSourceReq, DiskSource, SourceSpec as PbSource};

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
        })
        .await
        .expect("AddSource");

    let root = PathBuf::from(&session.root);
    let sub = PathBuf::from("Games").join("Skyrim").join("Data");
    std::fs::create_dir_all(root.join(&sub)).expect("mkdir under session root");

    // Negative canary: real bytes ONLY on session.root — identical
    // construction to `escape_matrix_positive_and_negative_canary`'s own.
    const NEGATIVE_BASENAME: &str = "escape-negative-canary.bin";
    let neg_rel = sub.join(NEGATIVE_BASENAME);
    std::fs::write(root.join(&neg_rel), b"the-negative-canary-bytes")
        .expect("write negative canary");

    let ctx = EscapeFixtureCtx {
        session_id: &session.id,
        fixture: &fixture,
        stats_log: &stats_log,
        vector7_link_dir: None,
        write_access: false,
    };

    let (exit, lines, _classified, _truncated) =
        run_escape_fixture(&mut client, &ctx, &root.join(&neg_rel), &out_file, Some("4m")).await;

    assert_eq!(
        exit, 0,
        "vfs-fixture-escape (isolated vector 4m) must exit 0. Lines captured: {lines:?}"
    );
    let line = lines
        .iter()
        .find(|l| l.vector == "4m")
        .unwrap_or_else(|| panic!("vector 4m produced no line at all in {out_file:?}"));

    if line.outcome.starts_with("unbuildable:") {
        panic!(
            "vector 4's own construction ({}) failed in this environment, so this test cannot \
             exercise the metadata-gap claim here — see vector 4's own `unbuildable` reasons in \
             `docs/escape-matrix.md`. This is an environment limitation, not evidence the gap is \
             closed.",
            line.outcome
        );
    }

    // The headline assertion, and the point of this test: a name-based
    // attribute query on the negative canary, via a spelling only
    // `RootMap`'s canonicaliser ever recognised, is sealed now that the
    // client predicate IS that canonicaliser.
    assert_eq!(
        line.outcome, "not-found",
        "expected the metadata query on the negative canary (via vector 4's volume-GUID \
         spelling: {:?}) to be sealed now that `fuse_client::vpath_under_root` is `RootMap`'s \
         own canonicalising predicate rather than a string-prefix test — got `{}` instead \
         (note: {:?}). `found` here means the client predicate lost its canonicalisation and \
         the qattr_hook/qfull_hook/qibn_hook family is reaching real disk again.",
        line.spelling, line.outcome, line.note
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
        roots: vec![],
        sources: vec![vfs_control::SourceEntry {
            spec: vfs_control::SourceSpec::Disk {
                path: dir.path().to_string_lossy().into_owned(),
            },
            mount: "/".into(),
            root: 0,
            write_layer: false,
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

/// Stage 2b task 5: a config's `[[root]] path` reaches the live session, so
/// the injected shim is told where each root *is* and not merely what it
/// serves.
///
/// This is the half that has no other test: `AddSourceReq` carries a root id
/// and no path, so before `DeclareRoot` existed a two-root config mounted
/// both providers correctly and the shim learned about exactly one root —
/// every path under the second falling through to real disk with nothing
/// reporting it. `RootEntry.path` was parsed, asserted in unit tests, and
/// read by no production code at all.
///
/// Asserted at the session, not at the RPC: the point is that the value
/// arrives somewhere that `Session::launch` will publish, not that a message
/// was sent.
#[tokio::test(flavor = "multi_thread")]
async fn a_configs_declared_root_paths_reach_the_live_session() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let registry = SessionRegistry::new();
    let reg_handle = registry.clone();
    let svc = DirectorService::new(registry);
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(DirectorServer::new(svc))
            .serve_with_incoming(incoming)
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut client = connect(&format!("{addr}")).await.unwrap();
    let game = tempfile::tempdir().unwrap();
    let docs = tempfile::tempdir().unwrap();
    std::fs::write(game.path().join("a.txt"), b"g").unwrap();
    std::fs::write(docs.path().join("a.txt"), b"d").unwrap();

    let cfg = SessionConfig {
        session: vfs_control::SessionMeta { name: Some("two-root-cfg".into()) },
        roots: vec![
            vfs_control::RootEntry {
                id: 0,
                name: "game".into(),
                path: game.path().to_string_lossy().into_owned(),
            },
            vfs_control::RootEntry {
                id: 1,
                name: "docs".into(),
                path: docs.path().to_string_lossy().into_owned(),
            },
        ],
        sources: vec![
            vfs_control::SourceEntry {
                spec: vfs_control::SourceSpec::Disk {
                    path: game.path().to_string_lossy().into_owned(),
                },
                mount: "/".into(),
                root: 0,
                write_layer: false,
            },
            vfs_control::SourceEntry {
                spec: vfs_control::SourceSpec::Disk {
                    path: docs.path().to_string_lossy().into_owned(),
                },
                mount: "/".into(),
                root: 1,
                write_layer: false,
            },
        ],
        launch: None,
        cache: None,
    };
    let (id, _) = apply_session_config(&mut client, &cfg).await.unwrap();

    reg_handle
        .with_session_mut(&id, |live| {
            let declared = live.session.declared_roots();
            assert_eq!(
                declared.len(),
                1,
                "exactly root 1 should be declared — root 0 is the daemon's own \
                 `Session.root` and a config cannot repoint it: {declared:?}"
            );
            assert_eq!(declared[0].0, 1);
            assert_eq!(
                declared[0].1,
                docs.path(),
                "root 1's declared host path is not the one the config named"
            );
            // Both providers are mounted too — declaring must not have
            // replaced mounting, only joined it.
            let kernel = live.session.kernel();
            let read_root = |root: u32| -> Vec<u8> {
                let mut buf = [0u8; 8];
                let (fh, _, _) = kernel
                    .open(vfs_protocol::RootId(root), "a.txt", vfs_director::OPEN_READ)
                    .unwrap();
                let n = kernel.read(fh, 0, &mut buf).unwrap();
                kernel.close(fh).unwrap();
                buf[..n].to_vec()
            };
            assert_eq!(read_root(0), b"g");
            assert_eq!(read_root(1), b"d");
            Ok(())
        })
        .unwrap();

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

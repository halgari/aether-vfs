//! M0 acceptance: daemon → CreateSession → AddSource(disk) → Launch(fixture-read)
//! via a scenario.toml, asserting the fixture reads virtual bytes through the ring.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use tokio::net::TcpListener;
use tonic::transport::Server;
use vfs_control::pb::director_server::DirectorServer;
use vfs_control::SessionConfig;
use vfs_directord::{apply_session_config, connect, DirectorService, SessionRegistry};

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

fn ensure_inject_artifacts() {
    // Session::launch locates shim/payload near the current exe (the test
    // binary). Co-locate them into the profile dir if cargo left them only in
    // deps/ or they were never built for this package.
    let profile = profile_dir();
    let needed = [
        "vfs_shim_dll.dll",
        "vfs_payload.dll",
        "vfs-fixture-read.exe",
        "vfs-fixture-writepath.exe",
    ];
    let missing = needed.iter().any(|n| !profile.join(n).is_file());
    if missing {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..");
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
                "--quiet",
            ])
            .status()
            .expect("spawn cargo");
        assert!(status.success(), "fixture/artifact build failed: {status}");

        // vfs-payload lives in its own workspace (panic = "abort"). Build it
        // into the same target dir so the co-location below finds it unchanged.
        let target_dir = workspace.join("target");
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
    // Copy into profile root so Session::launch's find_near works from the test exe.
    for name in needed {
        let dest = profile.join(name);
        if dest.is_file() {
            continue;
        }
        let src = locate_artifact(name);
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

    let mut stream = client
        .launch(LaunchReq {
            session_id: session.id.clone(),
            exec: fixture.to_string_lossy().into_owned(),
            args: Vec::new(),
            wait: true,
            env: Default::default(),
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
    let overlay_entries: Vec<_> = std::fs::read_dir(&overlay)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(
        overlay_entries.is_empty(),
        "nothing should land in the shim-local overlay fallback \
         ({overlay:?} contains {overlay_entries:?}) — this is the bypass the phase closes"
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

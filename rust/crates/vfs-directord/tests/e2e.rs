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
        "{name} not found near {:?}; build -p vfs-fixture-read -p vfs-shim-dll -p vfs-payload first",
        profile_dir()
    );
}

fn ensure_inject_artifacts() {
    // Session::launch locates shim/payload near the current exe (the test
    // binary). Co-locate them into the profile dir if cargo left them only in
    // deps/ or they were never built for this package.
    let profile = profile_dir();
    let needed = ["vfs_shim_dll.dll", "vfs_payload.dll", "vfs-fixture-read.exe"];
    let missing = needed.iter().any(|n| !profile.join(n).is_file());
    if missing {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..");
        let status = std::process::Command::new(cargo)
            .current_dir(&workspace)
            .args([
                "build",
                "-p",
                "vfs-shim-dll",
                "-p",
                "vfs-payload",
                "-p",
                "vfs-fixture-read",
                "--quiet",
            ])
            .status()
            .expect("spawn cargo");
        assert!(status.success(), "fixture/artifact build failed: {status}");
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

#[tokio::test(flavor = "multi_thread")]
async fn scenario_toml_disk_source_fixture_read() {
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

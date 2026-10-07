//! Session configuration through the daemon: apply, health, list, launch and
//! teardown by name, and declared root paths. Portable: none of these launch
//! a Windows process.

use std::path::PathBuf;
use std::time::Duration;

use tokio::net::TcpListener;
use tonic::transport::Server;
use vfs_control::pb::director_server::DirectorServer;
use vfs_control::SessionConfig;
use vfs_directord::{apply_session_config, connect, DirectorService, SessionRegistry};

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
    let dir = vfs_testkit::tempdir().unwrap();
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
            cache_key: None,
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
    assert!(list
        .sessions
        .iter()
        .any(|s| s.id == id && s.name == "list-me"));

    client
        .teardown_session(vfs_control::pb::TeardownReq { session_id: id })
        .await
        .unwrap();
    server.abort();
}

/// `vfs exec --session NAME` and `vfs down --session NAME`: the `Launch` and
/// `TeardownSession` RPCs take a session's name as well as its id, and
/// `Launch` expands a leading `{RootName}`. Every refusal here happens before
/// anything is spawned, so this runs on any host.
#[tokio::test(flavor = "multi_thread")]
async fn launch_and_teardown_address_a_session_by_name() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let svc = DirectorService::new(SessionRegistry::new());
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(DirectorServer::new(svc))
            .serve_with_incoming(incoming)
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let mut client = connect(&format!("{addr}")).await.unwrap();

    let cfg = SessionConfig {
        session: vfs_control::SessionMeta {
            name: Some("by-name".into()),
        },
        roots: vec![vfs_control::RootEntry {
            id: 0,
            name: "Games".into(),
            path: r"C:\Games\ByName".into(),
        }],
        ..Default::default()
    };
    let (id, _) = apply_session_config(&mut client, &cfg).await.unwrap();

    let launch = |session: &str, exec: &str| vfs_control::pb::LaunchReq {
        session_id: session.into(),
        exec: exec.into(),
        args: vec![],
        wait: true,
        env: Default::default(),
    };
    let st = client
        .launch(launch("no-such", "x.exe"))
        .await
        .expect_err("unknown session");
    assert_eq!(st.code(), tonic::Code::NotFound, "{st:?}");
    assert!(
        st.message().contains("no-such") && st.message().contains("by-name"),
        "the refusal must list what is live: {st:?}"
    );
    let st = client
        .launch(launch("by-name", r"{Nope}\x.exe"))
        .await
        .expect_err("unknown root");
    assert_eq!(st.code(), tonic::Code::InvalidArgument, "{st:?}");
    assert!(
        st.message().contains("Nope") && st.message().contains("Games"),
        "{st:?}"
    );

    client
        .teardown_session(vfs_control::pb::TeardownReq {
            session_id: "by-name".into(),
        })
        .await
        .expect("teardown by name");
    let list = client
        .list_sessions(vfs_control::pb::Empty {})
        .await
        .unwrap()
        .into_inner();
    assert!(list.sessions.iter().all(|s| s.id != id), "{list:?}");
    server.abort();
}

/// `apply_session_config` is all or nothing: a config that fails half-way —
/// a source that cannot be built, a launch refused before anything spawns —
/// leaves no session behind, so the corrected retry of the same named config
/// is not refused as a duplicate. And a second live session under one name is
/// refused (`AlreadyExists`), naming the one that holds it. Nothing here
/// spawns a program, so it runs on any host.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_apply_leaves_no_session_and_a_live_name_is_not_reused() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let svc = DirectorService::new(SessionRegistry::new());
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(DirectorServer::new(svc))
            .serve_with_incoming(incoming)
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let mut client = connect(&format!("{addr}")).await.unwrap();

    let content = vfs_testkit::tempdir().unwrap();
    let good = SessionConfig {
        session: vfs_control::SessionMeta {
            name: Some("half".into()),
        },
        roots: vec![vfs_control::RootEntry {
            id: 0,
            name: "Games".into(),
            path: if cfg!(windows) {
                content.path().join("loc").to_string_lossy().into_owned()
            } else {
                r"C:\Games\Half".into()
            },
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
        ..Default::default()
    };
    let live = |client: &mut vfs_control::pb::director_client::DirectorClient<_>| {
        let mut client = client.clone();
        async move {
            client
                .list_sessions(vfs_control::pb::Empty {})
                .await
                .unwrap()
                .into_inner()
                .sessions
        }
    };

    // A source the daemon cannot build: refused at AddSource.
    let mut bad_source = good.clone();
    bad_source.sources.push(vfs_control::SourceEntry {
        spec: vfs_control::SourceSpec::Zip {
            path: content
                .path()
                .join("missing.zip")
                .to_string_lossy()
                .into_owned(),
        },
        mount: "/".into(),
        root: 0,
        write_layer: false,
        cache_key: None,
    });
    let e = apply_session_config(&mut client, &bad_source)
        .await
        .unwrap_err();
    assert!(e.contains("AddSource"), "{e}");
    assert!(
        live(&mut client).await.is_empty(),
        "a failed AddSource must not leave a session"
    );

    // A launch refused before anything is spawned.
    let mut bad_launch = good.clone();
    bad_launch.launch = Some(vfs_control::LaunchConfig {
        exec: r"{Nope}\x.exe".into(),
        args: vec![],
        wait: true,
        env: Default::default(),
    });
    let e = apply_session_config(&mut client, &bad_launch)
        .await
        .unwrap_err();
    assert!(e.contains("Nope"), "{e}");
    assert!(
        live(&mut client).await.is_empty(),
        "a failed launch must not leave a session"
    );

    // The corrected config applies — its name was not left held.
    let (id, _) = apply_session_config(&mut client, &good)
        .await
        .expect("the corrected retry");
    // …and applying it again while it is live is refused, naming it.
    let e = apply_session_config(&mut client, &good).await.unwrap_err();
    assert!(
        e.contains("AlreadyExists") || e.contains("already named"),
        "{e}"
    );
    assert!(
        e.contains(&id),
        "the refusal must name the live session: {e}"
    );
    let sessions = live(&mut client).await;
    assert_eq!(sessions.len(), 1, "{sessions:?}");

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
    let game = vfs_testkit::tempdir().unwrap();
    let docs = vfs_testkit::tempdir().unwrap();
    std::fs::write(game.path().join("a.txt"), b"g").unwrap();
    std::fs::write(docs.path().join("a.txt"), b"d").unwrap();
    // Each root's location: where the program sees it. On Windows that is a
    // host directory (here the source directory itself, as before); on Linux
    // a `C:\…` path inside the Wine prefix — a host path is refused there.
    let (game_loc, docs_loc) = if cfg!(windows) {
        (game.path().to_path_buf(), docs.path().to_path_buf())
    } else {
        (
            PathBuf::from(r"C:\Games\Game"),
            PathBuf::from(r"C:\users\steamuser\Docs"),
        )
    };

    let cfg = SessionConfig {
        session: vfs_control::SessionMeta {
            name: Some("two-root-cfg".into()),
        },
        roots: vec![
            vfs_control::RootEntry {
                id: 0,
                name: "game".into(),
                path: game_loc.to_string_lossy().into_owned(),
            },
            vfs_control::RootEntry {
                id: 1,
                name: "docs".into(),
                path: docs_loc.to_string_lossy().into_owned(),
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
                cache_key: None,
            },
            vfs_control::SourceEntry {
                spec: vfs_control::SourceSpec::Disk {
                    path: docs.path().to_string_lossy().into_owned(),
                },
                mount: "/".into(),
                root: 1,
                write_layer: false,
                cache_key: None,
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
                "`declared_roots` lists the roots beyond root 0, so exactly root 1 — \
                 root 0's declared path is its location, asserted below: {declared:?}"
            );
            assert_eq!(declared[0].0, 1);
            assert_eq!(
                declared[0].1, docs_loc,
                "root 1's declared location is not the one the config named"
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

    // Root 0 was declared as well: the config's root 0 path replaces the
    // daemon's default, and the session summary reports it.
    let summary = reg_handle
        .list()
        .unwrap()
        .into_iter()
        .find(|s| s.id == id)
        .expect("the session is live");
    assert_eq!(
        summary.root, game_loc,
        "root 0's declared location must reach the live session as its root"
    );

    client
        .teardown_session(vfs_control::pb::TeardownReq { session_id: id })
        .await
        .unwrap();
    server.abort();
}

/// Sessions default to a directory under the system temp dir, and the daemons
/// these tests spawn inherit this process's environment. Point both at `target/`.
#[ctor::ctor]
fn scratch_tmpdir() {
    vfs_testkit::use_scratch_as_tmpdir();
}

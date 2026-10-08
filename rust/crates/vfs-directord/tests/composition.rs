//! Composition + storage integration through the session registry (no inject).

use std::path::Path;
use std::sync::Arc;

use aether_storage::{SourceKey, Storage, StorageConfig};
use tokio::net::TcpListener;
use tonic::transport::Server;
use vfs_control::pb::director_server::DirectorServer;
use vfs_control::pb::{source_spec, AddSourceReq, CreateSessionReq, DiskSource, Empty, ZipSource};
use vfs_control::SourceSpec;
use vfs_director::RootId;
use vfs_directord::{connect, DirectorService, SessionRegistry};
use vfs_embed::{Access, Capabilities, DirEntry, Handle, MemoryProvider, Provider, Stat, VPath};
use vfs_source::build_provider;
use vfs_testkit::zip::write_stored_zip;

/// A one-entry Stored zip named `layer.zip` inside `dir`.
fn layer_zip(dir: &Path, entry: &str, content: &[u8]) -> std::path::PathBuf {
    let path = dir.join("layer.zip");
    write_stored_zip(&path, entry, content);
    path
}

#[test]
fn registry_layered_disk_sources_top_wins() {
    let base = vfs_testkit::tempdir().unwrap();
    let mod_dir = vfs_testkit::tempdir().unwrap();
    std::fs::write(base.path().join("shared.txt"), b"FROM-BASE").unwrap();
    std::fs::write(base.path().join("only-base.txt"), b"BASE").unwrap();
    std::fs::write(mod_dir.path().join("shared.txt"), b"MOD-WIN").unwrap();
    std::fs::write(mod_dir.path().join("only-mod.txt"), b"MOD").unwrap();

    let reg = SessionRegistry::new();
    let summary = reg.create("layered".into()).unwrap();
    let base_be = build_provider(&SourceSpec::Disk {
        path: base.path().to_string_lossy().into_owned(),
    })
    .unwrap();
    let mod_be = build_provider(&SourceSpec::Disk {
        path: mod_dir.path().to_string_lossy().into_owned(),
    })
    .unwrap();
    reg.add_source(&summary.id, 0, "/", 0, base_be).unwrap();
    reg.add_source(&summary.id, 0, "/", 10, mod_be).unwrap();

    reg.with_session_mut(&summary.id, |live| {
        let shared = live.session.read_file("shared.txt").unwrap();
        assert_eq!(shared, b"MOD-WIN");
        let only_base = live.session.read_file("only-base.txt").unwrap();
        assert_eq!(only_base, b"BASE");
        let only_mod = live.session.read_file("only-mod.txt").unwrap();
        assert_eq!(only_mod, b"MOD");
        Ok(())
    })
    .unwrap();
}

#[test]
fn registry_zip_source_reads_entry() {
    let dir = vfs_testkit::tempdir().unwrap();
    let zip = layer_zip(dir.path(), "Data/proof.dat", b"ZIP-BYTES");
    let reg = SessionRegistry::new();
    let summary = reg.create("zip".into()).unwrap();
    let be = build_provider(&SourceSpec::Zip {
        path: zip.to_string_lossy().into_owned(),
    })
    .unwrap();
    reg.add_source(&summary.id, 0, "/", 0, be).unwrap();
    reg.with_session_mut(&summary.id, |live| {
        let got = live.session.read_file("Data/proof.dat").unwrap();
        assert_eq!(got, b"ZIP-BYTES");
        Ok(())
    })
    .unwrap();
}

/// With storage, a slow immutable source is read through the pull-through
/// cache: the second full read is served from it, not from the source.
#[test]
fn registry_cache_hits_on_second_read() {
    let store_dir = vfs_testkit::tempdir().unwrap();
    let storage = Storage::open(store_dir.path(), StorageConfig::default()).unwrap();
    let reg = SessionRegistry::with_storage(Arc::clone(&storage));
    let summary = reg.create("cache".into()).unwrap();
    // Several 64 KiB store blocks.
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let be: Arc<dyn Provider> = Arc::new(SlowImmutable(MemoryProvider::from_files([(
        "blob.bin",
        payload.clone(),
    )])));
    reg.add_source_keyed(
        &summary.id,
        0,
        "/",
        0,
        be,
        SourceKey("composition-test".into()),
    )
    .unwrap();
    let first = reg
        .with_session_mut(&summary.id, |live| Ok(live.session.read_file("blob.bin")))
        .unwrap()
        .unwrap();
    assert_eq!(first, payload);
    let after_first = storage.stats().cache;
    assert!(
        after_first.misses >= 1,
        "the first read fetches from the source: {after_first:?}"
    );
    let second = reg
        .with_session_mut(&summary.id, |live| Ok(live.session.read_file("blob.bin")))
        .unwrap()
        .unwrap();
    assert_eq!(second, payload);
    let after_second = storage.stats().cache;
    assert_eq!(
        after_second.misses, after_first.misses,
        "the second read must not reach the source: {after_second:?}"
    );
    assert!(
        after_second.hits > after_first.hits,
        "the second read is a hit: {after_second:?}"
    );
}

/// A `MemoryProvider` that declares itself slow and immutable, so
/// `Storage::cached` wraps it.
struct SlowImmutable(MemoryProvider);

impl Provider for SlowImmutable {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            access: Access::Read,
            immutable: true,
            slow: true,
            ..self.0.capabilities()
        }
    }
    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        self.0.getattr(p)
    }
    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        self.0.readdir(p)
    }
    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        self.0.open(p, flags)
    }
    fn read_at(&self, h: Handle, off: u64, buf: &mut [u8]) -> Result<usize, i32> {
        self.0.read_at(h, off, buf)
    }
    fn close(&self, h: Handle) -> Result<(), i32> {
        self.0.close(h)
    }
}

/// Historical note: this test used to demonstrate two confirmed gaps in
/// non-root mount support (see `escape-matrix.md`'s "The Mod Organizer
/// consequence" section for the full history) — mount-prefix matching was
/// case-sensitive while every shim vpath is always lowercased, and
/// `Director::readdir` never surfaced a mount registered below the queried
/// directory, so it could be opened by a known path but never discovered by
/// listing its parent. Stage 2b task 1 (`vfs-director::path::strip_prefix`,
/// `vfs-director::director::Director::readdir`) closed both: prefix
/// comparison now folds ASCII case on both sides, and `readdir` contributes
/// the next path component of any deeper mount as a synthetic directory
/// entry. This test now asserts the fixed behavior directly, including a
/// mixed-case mount (the original, `escape-matrix.md`-documented spelling)
/// to prove case is no longer a live concern either.
#[test]
fn non_root_mount_matches_lowercase_open_and_is_discoverable_via_parent_readdir() {
    let root_dir = vfs_testkit::tempdir().unwrap();
    // A real, physical "Data" directory the root disk mount can enumerate,
    // standing in for the base game content a real session always has.
    let data_dir = root_dir.path().join("Data");
    std::fs::create_dir(&data_dir).unwrap();
    std::fs::write(data_dir.join("Skyrim.esm"), b"BASE-CONTENT").unwrap();

    // The mod's staging directory — physically anywhere else entirely, never
    // nested under `root_dir`, exactly the MO2 shape the matrix documents.
    let mod_dir = vfs_testkit::tempdir().unwrap();
    std::fs::write(mod_dir.path().join("foo.esp"), b"MOD-BYTES").unwrap();

    let reg = SessionRegistry::new();
    let summary = reg.create("nonroot".into()).unwrap();
    let root_be = build_provider(&SourceSpec::Disk {
        path: root_dir.path().to_string_lossy().into_owned(),
    })
    .unwrap();
    let mod_be = build_provider(&SourceSpec::Disk {
        path: mod_dir.path().to_string_lossy().into_owned(),
    })
    .unwrap();
    reg.add_source(&summary.id, 0, "/", 0, root_be).unwrap();
    // Mixed case, deliberately — the original `escape-matrix.md`-documented
    // spelling. Case folding at compare time means this must match a
    // lowercased live open exactly as a lowercase-authored mount would.
    reg.add_source(&summary.id, 0, "Data/SomeMod", 10, mod_be)
        .unwrap();

    reg.with_session_mut(&summary.id, |live| {
        // A direct open by a known relative path succeeds through the
        // non-root mount, even though the mount was registered mixed-case
        // and the open is spelled all-lowercase (what the shim always sends).
        let bytes = live.session.read_file("data/somemod/foo.esp").unwrap();
        assert_eq!(bytes, b"MOD-BYTES");

        // The base content is still there and enumerable...
        let base_entries = live
            .session
            .kernel()
            .readdir(RootId::DEFAULT, "data")
            .unwrap();
        assert!(
            base_entries
                .iter()
                .any(|e| e.name.eq_ignore_ascii_case("Skyrim.esm")),
            "expected the real base content to still enumerate: {:?}",
            base_entries.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
        // ...and the mount point itself now appears as a synthetic child
        // entry too: the gap this test used to demonstrate is closed.
        assert!(
            base_entries
                .iter()
                .any(|e| e.name.eq_ignore_ascii_case("somemod")),
            "expected readdir(\"data\") to list the non-root mount point as \
             a synthetic child entry: {:?}",
            base_entries.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
        Ok(())
    })
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn stats_rpc_reports_sessions_and_cache() {
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
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let mut client = connect(&format!("{addr}")).await.unwrap();
    let before = client.stats(Empty {}).await.unwrap().into_inner();
    assert_eq!(before.sessions, 0);

    let session = client
        .create_session(CreateSessionReq { name: "s".into() })
        .await
        .unwrap()
        .into_inner();
    let dir = vfs_testkit::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), b"hi").unwrap();
    client
        .add_source(AddSourceReq {
            session_id: session.id.clone(),
            source: Some(vfs_control::pb::SourceSpec {
                kind: Some(source_spec::Kind::Disk(DiskSource {
                    path: dir.path().to_string_lossy().into_owned(),
                })),
            }),
            mount: "/".into(),
            layer: 0,
            root: 0,
            write_layer: false,
            cache_key: String::new(),
        })
        .await
        .unwrap();

    let after = client.stats(Empty {}).await.unwrap().into_inner();
    assert_eq!(after.sessions, 1);

    client
        .teardown_session(vfs_control::pb::TeardownReq {
            session_id: session.id,
        })
        .await
        .unwrap();
    server.abort();
}

/// The measurement gate's director-side exposure: `Stats` must carry the
/// director's own open counts (`io_stats::open_totals`), not just cache
/// metrics. Drives a real open through `dispatch_director` (the same
/// function the ring calls for `OP_OPEN`) rather than `Session::read_file`,
/// since `read_file` goes straight through the kernel and never touches
/// `io_stats::record_open`.
#[tokio::test(flavor = "multi_thread")]
async fn stats_rpc_reports_open_counts_after_session_activity() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let registry = SessionRegistry::new();
    let svc = DirectorService::new(registry.clone());
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(DirectorServer::new(svc))
            .serve_with_incoming(incoming)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let mut client = connect(&format!("{addr}")).await.unwrap();
    let before = client.stats(Empty {}).await.unwrap().into_inner();

    let session = client
        .create_session(CreateSessionReq {
            name: "stats-opens".into(),
        })
        .await
        .unwrap()
        .into_inner();
    let dir = vfs_testkit::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), b"hi").unwrap();
    client
        .add_source(AddSourceReq {
            session_id: session.id.clone(),
            source: Some(vfs_control::pb::SourceSpec {
                kind: Some(source_spec::Kind::Disk(DiskSource {
                    path: dir.path().to_string_lossy().into_owned(),
                })),
            }),
            mount: "/".into(),
            layer: 0,
            root: 0,
            write_layer: false,
            cache_key: String::new(),
        })
        .await
        .unwrap();

    registry
        .with_session_mut(&session.id, |live| {
            let kernel = live.session.kernel();
            let (st, payload) = vfs_director::ring_dispatch::dispatch_director(
                kernel,
                vfs_protocol::OP_OPEN,
                &vfs_protocol::encode_open_req(0, vfs_director::OPEN_READ, "f.txt"),
                0,
                4096,
                None,
            );
            assert_eq!(st, vfs_protocol::ST_OK, "the served open must succeed");
            assert!(vfs_protocol::decode_open_resp(&payload).is_some());
            Ok(())
        })
        .unwrap();

    let after = client.stats(Empty {}).await.unwrap().into_inner();
    assert!(
        after.opens_ok > before.opens_ok,
        "opens_ok did not reflect the served open: before={} after={}",
        before.opens_ok,
        after.opens_ok
    );

    client
        .teardown_session(vfs_control::pb::TeardownReq {
            session_id: session.id,
        })
        .await
        .unwrap();
    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn add_zip_source_via_grpc() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let registry = SessionRegistry::new();
    let probe = registry.clone();
    let svc = DirectorService::new(registry);
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(DirectorServer::new(svc))
            .serve_with_incoming(incoming)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let dir = vfs_testkit::tempdir().unwrap();
    let zip = layer_zip(dir.path(), "hello.txt", b"hello");
    let mut client = connect(&format!("{addr}")).await.unwrap();
    let session = client
        .create_session(CreateSessionReq {
            name: "zip-rpc".into(),
        })
        .await
        .unwrap()
        .into_inner();
    client
        .add_source(AddSourceReq {
            session_id: session.id.clone(),
            source: Some(vfs_control::pb::SourceSpec {
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
        .expect("AddSource zip");

    probe
        .with_session_mut(&session.id, |live| {
            let got = live.session.read_file("hello.txt").unwrap();
            assert_eq!(got, b"hello");
            Ok(())
        })
        .unwrap();

    client
        .teardown_session(vfs_control::pb::TeardownReq {
            session_id: session.id,
        })
        .await
        .unwrap();
    server.abort();
}

#[test]
fn config_load_toml_file() {
    let dir = vfs_testkit::tempdir().unwrap();
    let path = dir.path().join("scenario.toml");
    std::fs::write(
        &path,
        r#"
[session]
name = "from-file"
[[source]]
type = "disk"
path = "C:/x"
layer = 3
[launch]
exec = "a.exe"
wait = false
"#,
    )
    .unwrap();
    let cfg = vfs_control::load(&path).unwrap();
    assert_eq!(cfg.session.name.as_deref(), Some("from-file"));
    // `layer` is no longer a schema field (stage 2b task 2 — see
    // `vfs_control::config`'s module doc): a config file left over from
    // before that change, still carrying a stray `layer = 3`, must load
    // without error rather than be rejected, and the source must still
    // desugar onto root 0 exactly as an undecorated flat `[[source]]`
    // entry always has.
    assert_eq!(cfg.sources[0].root, 0);
    assert!(!cfg.launch.unwrap().wait);
}

/// Sessions default to a directory under the system temp dir, and the daemons
/// these tests spawn inherit this process's environment. Point both at `target/`.
#[ctor::ctor]
fn scratch_tmpdir() {
    vfs_testkit::use_scratch_as_tmpdir();
}

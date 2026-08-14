//! Composition + cache integration through the session registry (no inject).

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use tokio::net::TcpListener;
use tonic::transport::Server;
use vfs_control::pb::director_server::DirectorServer;
use vfs_control::pb::{source_spec, AddSourceReq, CreateSessionReq, DiskSource, Empty, ZipSource};
use vfs_control::SourceSpec;
use vfs_directord::{connect, DirectorService, SessionRegistry};
use vfs_source::build_provider;

fn write_stored_zip(dir: &Path, entry: &str, content: &[u8]) -> std::path::PathBuf {
    let path = dir.join("layer.zip");
    let mut buf = Vec::new();
    let crc = crc32(content);
    let n = entry.len() as u16;
    buf.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 4]);
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
    buf.extend_from_slice(&n.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(entry.as_bytes());
    buf.extend_from_slice(content);
    let cd_start = buf.len() as u32;
    buf.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 6]);
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
    buf.extend_from_slice(&n.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&[0u8; 8]);
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(entry.as_bytes());
    let cd_size = buf.len() as u32 - cd_start;
    buf.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 4]);
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&cd_size.to_le_bytes());
    buf.extend_from_slice(&cd_start.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    std::fs::File::create(&path).unwrap().write_all(&buf).unwrap();
    path
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[test]
fn registry_layered_disk_sources_top_wins() {
    let base = tempfile::tempdir().unwrap();
    let mod_dir = tempfile::tempdir().unwrap();
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
    reg.add_source(&summary.id, "/", 0, base_be).unwrap();
    reg.add_source(&summary.id, "/", 10, mod_be).unwrap();

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
    let dir = tempfile::tempdir().unwrap();
    let zip = write_stored_zip(dir.path(), "Data/proof.dat", b"ZIP-BYTES");
    let reg = SessionRegistry::new();
    let summary = reg.create("zip".into()).unwrap();
    let be = build_provider(&SourceSpec::Zip {
        path: zip.to_string_lossy().into_owned(),
    })
    .unwrap();
    reg.add_source(&summary.id, "/", 0, be).unwrap();
    reg.with_session_mut(&summary.id, |live| {
        let got = live.session.read_file("Data/proof.dat").unwrap();
        assert_eq!(got, b"ZIP-BYTES");
        Ok(())
    })
    .unwrap();
}

#[test]
fn registry_cache_hits_on_second_read() {
    let dir = tempfile::tempdir().unwrap();
    // Large enough to span multiple 1MiB? Use small block via custom cache.
    use vfs_cache::{BlockCache, CacheConfig};
    let cache = Arc::new(BlockCache::new(CacheConfig {
        block_size: 16,
        ram_budget: 1024 * 1024,
        disk_dir: None,
    }));
    let reg = SessionRegistry::with_cache(cache.clone());
    let summary = reg.create("cache".into()).unwrap();
    let payload = vec![7u8; 40];
    std::fs::write(dir.path().join("blob.bin"), &payload).unwrap();
    let be = build_provider(&SourceSpec::Disk {
        path: dir.path().to_string_lossy().into_owned(),
    })
    .unwrap();
    reg.add_source(&summary.id, "/", 0, be).unwrap();
    reg.with_session_mut(&summary.id, |live| {
        let a = live.session.read_file("blob.bin").unwrap();
        let b = live.session.read_file("blob.bin").unwrap();
        assert_eq!(a, payload);
        assert_eq!(b, payload);
        Ok(())
    })
    .unwrap();
    let stats = cache.stats();
    assert!(stats.hits >= 1, "expected cache hits after second full read: {stats:?}");
    assert!(stats.misses >= 1, "expected at least one miss: {stats:?}");
}

/// A non-root mount only ever matches a vpath spelled in the exact case the
/// mount was registered with (`vfs-director::path::strip_prefix` is a plain,
/// case-sensitive string comparison) — and every vpath the shim ever sends
/// over the ring is already lowercased
/// (`vfs-shim::fuse_client::normalize_path_for_root`), so a mixed-case mount
/// like `escape-matrix.md`'s old `"Data/SomeMod"` remedy can never match a
/// live open. Registering the mount in the same lowercase spelling the shim
/// always sends fixes exactly that half of the problem: a direct, known-path
/// open through the mount succeeds.
///
/// The other half is not a spelling problem and this test does not pretend
/// otherwise: `Director::readdir` only asks a mount for entries when that
/// mount's own prefix is at-or-above the queried path
/// (`strip_prefix(query, mount.prefix)`); a mount rooted *below* the queried
/// directory (here, `"data/somemod"` below a `readdir("data")`) never
/// contributes an entry for itself to that listing. So even a
/// correctly-cased non-root mount is invisible to a caller that discovers
/// content by enumerating its parent directory rather than opening a name it
/// already knows — a real, confirmed gap in non-root mount support, not
/// merely an undocumented one.
#[test]
fn non_root_mount_matches_lowercase_open_but_not_parent_readdir() {
    let root_dir = tempfile::tempdir().unwrap();
    // A real, physical "Data" directory the root disk mount can enumerate,
    // standing in for the base game content a real session always has.
    let data_dir = root_dir.path().join("Data");
    std::fs::create_dir(&data_dir).unwrap();
    std::fs::write(data_dir.join("Skyrim.esm"), b"BASE-CONTENT").unwrap();

    // The mod's staging directory — physically anywhere else entirely, never
    // nested under `root_dir`, exactly the MO2 shape the matrix documents.
    let mod_dir = tempfile::tempdir().unwrap();
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
    reg.add_source(&summary.id, "/", 0, root_be).unwrap();
    // Lowercase throughout — the spelling that actually matches what the
    // shim sends, unlike the doc's old mixed-case example.
    reg.add_source(&summary.id, "data/somemod", 10, mod_be).unwrap();

    reg.with_session_mut(&summary.id, |live| {
        // A direct open by a known relative path succeeds through the
        // non-root mount.
        let bytes = live.session.read_file("data/somemod/foo.esp").unwrap();
        assert_eq!(bytes, b"MOD-BYTES");

        // The base content is still there and enumerable...
        let base_entries = live.session.kernel().readdir("data").unwrap();
        assert!(
            base_entries.iter().any(|e| e.name.eq_ignore_ascii_case("Skyrim.esm")),
            "expected the real base content to still enumerate: {:?}",
            base_entries.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
        // ...but the mount point itself never appears as a child entry: the
        // confirmed gap this test exists to demonstrate.
        assert!(
            base_entries.iter().all(|e| !e.name.eq_ignore_ascii_case("somemod")),
            "readdir(\"data\") unexpectedly listed the non-root mount point \
             as a child entry — if this starts passing, the gap this test \
             documents has been closed and escape-matrix.md needs updating: {:?}",
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
        .create_session(CreateSessionReq {
            name: "s".into(),
        })
        .await
        .unwrap()
        .into_inner();
    let dir = tempfile::tempdir().unwrap();
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
    let dir = tempfile::tempdir().unwrap();
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
        })
        .await
        .unwrap();

    registry
        .with_session_mut(&session.id, |live| {
            let kernel = live.session.kernel();
            let (st, payload) = vfs_director::ring_dispatch::dispatch_director(
                kernel,
                vfs_protocol::OP_OPEN,
                &vfs_protocol::encode_open_req(vfs_director::OPEN_READ, "f.txt"),
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

    let dir = tempfile::tempdir().unwrap();
    let zip = write_stored_zip(dir.path(), "hello.txt", b"hello");
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
    let dir = tempfile::tempdir().unwrap();
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
    assert_eq!(cfg.sources[0].layer, 3);
    assert!(!cfg.launch.unwrap().wait);
}

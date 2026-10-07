//! The pull-through cache end to end (spec §8): a remote source served over
//! gRPC by an in-process `vfs-source` server, declared immutable and slow, is
//! added to a session of an in-process daemon that has storage. The first
//! full read of a file fetches from the source; the second is served from
//! the store; and when the source's file changes (a new size), the new bytes
//! are served, never the old cached blocks.
//!
//! Portable: it runs on Linux and Windows CI.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::net::TcpListener;
use tonic::transport::Server;
use vfs_control::pb::director_server::DirectorServer;
use vfs_control::pb::{Empty, StatsResp, TeardownReq};
use vfs_control::{SessionConfig, SessionMeta, SourceEntry, SourceSpec};
use vfs_directord::{apply_session_config, connect, DirectorService, SessionRegistry};
use vfs_embed::{
    Access, Capabilities, DirEntry, Handle, MemoryProvider, Provider, Stat, Storage, StorageConfig,
    VPath,
};
use vfs_source::pb::source_server::SourceServer;
use vfs_source::ProviderSourceService;

/// A source that declares itself `Read`, immutable and slow — what a CDN or
/// remote mod host is — and whose content the test can replace between
/// sessions. Handles come from the `MemoryProvider` current at `open`; the
/// test swaps only while no handle is open.
struct SwappableSlowSource(RwLock<Arc<MemoryProvider>>);

impl SwappableSlowSource {
    fn new(data: Vec<u8>) -> Self {
        Self(RwLock::new(Arc::new(Self::tree(data))))
    }
    fn tree(data: Vec<u8>) -> MemoryProvider {
        MemoryProvider::from_files([("data.bin", data)])
    }
    fn replace(&self, data: Vec<u8>) {
        *self.0.write().unwrap() = Arc::new(Self::tree(data));
    }
    fn cur(&self) -> Arc<MemoryProvider> {
        Arc::clone(&self.0.read().unwrap())
    }
}

impl Provider for SwappableSlowSource {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            access: Access::Read,
            immutable: true,
            slow: true,
            ..self.cur().capabilities()
        }
    }
    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        self.cur().getattr(p)
    }
    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        self.cur().readdir(p)
    }
    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        self.cur().open(p, flags)
    }
    fn read_at(&self, h: Handle, off: u64, buf: &mut [u8]) -> Result<usize, i32> {
        self.cur().read_at(h, off, buf)
    }
    fn close(&self, h: Handle) -> Result<(), i32> {
        self.cur().close(h)
    }
}

/// `len` bytes of a pattern that differs per `seed` and never repeats on a
/// block boundary, so a stale or misplaced block cannot compare equal.
fn pattern(len: usize, seed: u32) -> Vec<u8> {
    (0..len as u32)
        .map(|i| (i.wrapping_mul(31).wrapping_add(seed) % 251) as u8)
        .collect()
}

fn remote_config(name: &str, endpoint: &str) -> SessionConfig {
    SessionConfig {
        session: SessionMeta {
            name: Some(name.into()),
        },
        roots: vec![],
        sources: vec![SourceEntry {
            spec: SourceSpec::Remote {
                endpoint: endpoint.into(),
            },
            mount: "/".into(),
            root: 0,
            write_layer: false,
            cache_key: None,
        }],
        launch: None,
        cache: None,
    }
}

/// Reads `data.bin` through the session's kernel, off the async executor
/// (the remote provider blocks on its gRPC calls).
async fn read_through_session(reg: &SessionRegistry, id: &str) -> Vec<u8> {
    let reg = reg.clone();
    let id = id.to_string();
    tokio::task::spawn_blocking(move || {
        reg.with_session_mut(&id, |live| {
            live.session
                .read_file("data.bin")
                .map_err(|st| format!("read data.bin: status {st}"))
        })
    })
    .await
    .unwrap()
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_remote_source_is_served_from_the_store_on_the_second_pass() {
    // The remote source, over gRPC.
    const MIB: usize = 1024 * 1024;
    let v1 = pattern(MIB, 7);
    let source = Arc::new(SwappableSlowSource::new(v1.clone()));
    let source_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let source_addr = source_listener.local_addr().unwrap();
    let source_svc = ProviderSourceService::new(Arc::clone(&source) as Arc<dyn Provider>);
    let source_server = tokio::spawn(async move {
        Server::builder()
            .add_service(SourceServer::new(source_svc))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(
                source_listener,
            ))
            .await
    });

    // The daemon, with storage in a temp dir.
    let store_dir = vfs_testkit::tempdir().unwrap();
    let storage = Storage::open(store_dir.path(), StorageConfig::default()).unwrap();
    let registry = SessionRegistry::with_storage(Arc::clone(&storage));
    let daemon_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let daemon_addr = daemon_listener.local_addr().unwrap();
    let svc = DirectorService::new(registry.clone());
    let daemon_server = tokio::spawn(async move {
        Server::builder()
            .add_service(DirectorServer::new(svc))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(
                daemon_listener,
            ))
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let mut client = connect(&format!("{daemon_addr}")).await.unwrap();
    let endpoint = format!("{source_addr}");
    let stats = |s: &StatsResp| {
        format!(
            "hits={} misses={} from_cache={} from_source={} logical={}",
            s.cache_hits,
            s.cache_misses,
            s.cache_bytes_from_cache,
            s.cache_bytes_from_source,
            s.cache_logical_bytes
        )
    };

    let (id, _) = apply_session_config(&mut client, &remote_config("pull-1", &endpoint))
        .await
        .unwrap();
    let before = client.stats(Empty {}).await.unwrap().into_inner();
    assert_eq!(
        (before.cache_hits, before.cache_misses),
        (0, 0),
        "nothing read yet: {}",
        stats(&before)
    );

    // Pass 1: every block is a miss, fetched from the source.
    let pass1 = read_through_session(&registry, &id).await;
    assert!(pass1 == v1, "pass 1 must return the source's bytes");
    let after1 = client.stats(Empty {}).await.unwrap().into_inner();
    assert!(
        after1.cache_misses > 0 && after1.cache_bytes_from_source > 0,
        "pass 1 fetches from the source: {}",
        stats(&after1)
    );
    assert!(
        after1.cache_logical_bytes >= MIB as u64,
        "pass 1 leaves the file in the cache: {}",
        stats(&after1)
    );

    // Pass 2: served from the store, the source never asked.
    let pass2 = read_through_session(&registry, &id).await;
    assert!(pass2 == v1, "pass 2 must return the same bytes");
    let after2 = client.stats(Empty {}).await.unwrap().into_inner();
    assert_eq!(
        after2.cache_misses,
        after1.cache_misses,
        "pass 2 must not reach the source: {}",
        stats(&after2)
    );
    assert!(
        after2.cache_hits > after1.cache_hits,
        "pass 2 is served by hits: {}",
        stats(&after2)
    );
    assert!(
        after2.cache_bytes_from_cache >= after1.cache_bytes_from_cache + MIB as u64,
        "pass 2's whole file comes from the cache: {}",
        stats(&after2)
    );
    assert_eq!(
        after2.cache_bytes_from_source,
        after1.cache_bytes_from_source,
        "{}",
        stats(&after2)
    );
    client
        .teardown_session(TeardownReq {
            session_id: id.clone(),
        })
        .await
        .unwrap();

    // The source's file changes version (a new size and new content). A new
    // session over the same endpoint must see the new bytes, not the old
    // cached blocks.
    let v2 = pattern(MIB + 4096 + 17, 99);
    source.replace(v2.clone());
    let (id2, _) = apply_session_config(&mut client, &remote_config("pull-2", &endpoint))
        .await
        .unwrap();
    let pass3 = read_through_session(&registry, &id2).await;
    assert_eq!(pass3.len(), v2.len(), "the new version's size is served");
    assert!(pass3 == v2, "the new version's bytes are served");
    let after3 = client.stats(Empty {}).await.unwrap().into_inner();
    assert!(
        after3.cache_misses > after2.cache_misses,
        "the new version is fetched from the source: {}",
        stats(&after3)
    );
    // And it too is cached from then on.
    let pass4 = read_through_session(&registry, &id2).await;
    assert!(pass4 == v2);
    let after4 = client.stats(Empty {}).await.unwrap().into_inner();
    assert_eq!(
        after4.cache_misses,
        after3.cache_misses,
        "{}",
        stats(&after4)
    );

    client
        .teardown_session(TeardownReq { session_id: id2 })
        .await
        .unwrap();
    daemon_server.abort();
    source_server.abort();
    let _ = daemon_server.await;
    let _ = source_server.await;
    drop(client);
    drop(registry);
    // Best effort: release the store's lock and files before the temp dir
    // is removed (Windows cannot delete open files).
    let _ = storage.close();
}

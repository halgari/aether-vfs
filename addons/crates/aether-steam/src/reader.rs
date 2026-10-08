//! Random access to depot files: map a byte range to the chunks covering it,
//! fetch only those (bounded concurrency, retries across CDN servers),
//! decode, verify and splice.
use crate::cdn::{CdnConfig, CdnServer, CdnTokenSource, Fetcher};
use crate::chunk::decode_chunk;
use crate::error::SteamError;
use crate::ids::{AppId, ChunkId, DepotId, DepotKey, ManifestId};
use crate::manifest::{ChunkRef, DepotManifest, FileEntry};
use aether_archive::{RangeRead, Xxh64};
use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt};
use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};
use tokio::runtime::Handle;
use tokio::sync::Semaphore;

/// The largest manifest body this crate will download from the CDN, before
/// even trying to unzip it. A real depot manifest's CDN response is at most
/// a few MiB; this is generous headroom against a CDN response that lies
/// about (or omits) `Content-Length` to try to make the fetcher buffer an
/// unbounded amount of memory. Separate from, and much smaller than,
/// [`DepotManifest`]'s own cap on the *decompressed* zip entry size.
const MAX_MANIFEST_BODY_BYTES: u64 = 64 << 20;

/// Bytes of headroom added to `2 * chunk.len` for a chunk download's body
/// cap: real chunk bodies are near their decompressed size once
/// AES-encrypted (compression plus a little format overhead), so this only
/// needs to absorb the AES/VSZa/zip envelope, not a hostile amplification.
const CHUNK_BODY_SLACK_BYTES: u64 = 64 * 1024;

/// Reads one depot through the CDN: shared by every file opened from it.
/// Cheap to clone.
#[derive(Clone)]
pub struct DepotReader {
    inner: Arc<ReaderInner>,
}

struct ReaderInner {
    fetcher: Fetcher,
    key: DepotKey,
    permits: Semaphore,
    cache: Mutex<ChunkLru>,
}

impl DepotReader {
    /// `servers` must be non-empty. `tokens` is consulted when a server
    /// answers 403; without it a 403 just moves on to the next server.
    ///
    /// The reader gets an HTTP client and a server pool of its own, and the
    /// pool is not probed: it keeps to the first server until that fails.
    /// [`SteamContent::reader`](crate::SteamContent::reader) is the way to
    /// readers that share one client and one probed pool.
    pub fn new(
        app: AppId,
        depot: DepotId,
        key: DepotKey,
        servers: Vec<CdnServer>,
        cfg: CdnConfig,
        tokens: Option<Arc<dyn CdnTokenSource>>,
    ) -> Result<Self, SteamError> {
        Ok(DepotReader::from_fetcher(
            Fetcher::new(app, depot, servers, cfg, tokens)?,
            key,
        ))
    }

    /// A reader over `fetcher`, which may share its HTTP client and CDN
    /// pool with the readers of other depots.
    pub(crate) fn from_fetcher(fetcher: Fetcher, key: DepotKey) -> Self {
        let cfg = fetcher.config();
        let permits = Semaphore::new(cfg.max_concurrent_chunks.max(1));
        let cache = Mutex::new(ChunkLru::new(cfg.chunk_cache_bytes));
        DepotReader {
            inner: Arc::new(ReaderInner {
                fetcher,
                key,
                permits,
                cache,
            }),
        }
    }

    pub fn app(&self) -> AppId {
        self.inner.fetcher.app
    }

    pub fn depot(&self) -> DepotId {
        self.inner.fetcher.depot
    }

    /// Download, decrypt and parse manifest `id` of this depot. `request_code`
    /// comes from [`SteamSession::manifest_request_code`](crate::SteamSession::manifest_request_code);
    /// 0 means none.
    pub async fn fetch_manifest(
        &self,
        id: ManifestId,
        request_code: u64,
    ) -> Result<DepotManifest, SteamError> {
        let depot = self.depot();
        let mut path = format!("/depot/{depot}/manifest/{id}/5");
        if request_code != 0 {
            path.push_str(&format!("/{request_code}"));
        }
        let key = self.inner.key.clone();
        self.inner
            .fetcher
            .fetch(
                &path,
                "manifest",
                // Not `path`: the request code stays out of the label.
                &format!("steam {depot} manifest {id}"),
                MAX_MANIFEST_BODY_BYTES,
                move |body| DepotManifest::from_cdn_bytes(&body, depot, id, &key),
            )
            .await
    }

    /// One chunk's decoded bytes, from the in-memory cache or the CDN.
    pub async fn chunk(&self, c: &ChunkRef) -> Result<Bytes, SteamError> {
        if let Some(b) = self.inner.cache.lock().unwrap().get(&c.id) {
            return Ok(b);
        }
        let _permit = self
            .inner
            .permits
            .acquire()
            .await
            .map_err(|_| SteamError::Protocol("depot reader closed".into()))?;
        if let Some(b) = self.inner.cache.lock().unwrap().get(&c.id) {
            return Ok(b);
        }
        let key = self.inner.key.clone();
        let chunk = *c;
        let max_body = 2 * u64::from(c.len) + CHUNK_BODY_SLACK_BYTES;
        let data = self
            .inner
            .fetcher
            .fetch(
                &format!("/depot/{}/chunk/{}", self.depot(), c.id),
                "chunk",
                &format!("steam {} {}", self.depot(), c.id),
                max_body,
                move |raw| decode_chunk(&raw, &key, &chunk).map(Bytes::from),
            )
            .await?;
        self.inner.cache.lock().unwrap().put(c.id, data.clone());
        Ok(data)
    }

    /// Open `path` (any case, `/` or `\`) from `manifest`, which must belong
    /// to this reader's depot.
    pub fn open(
        &self,
        manifest: Arc<DepotManifest>,
        path: &str,
    ) -> Result<SteamDepotFile, SteamError> {
        if manifest.depot() != self.depot() {
            return Err(SteamError::Protocol(format!(
                "manifest of depot {} opened with the reader for depot {}",
                manifest.depot(),
                self.depot()
            )));
        }
        let index = manifest
            .file_index(path)
            .ok_or_else(|| SteamError::FileNotFound(path.to_string()))?;
        Ok(SteamDepotFile {
            reader: self.clone(),
            manifest,
            index,
        })
    }
}

/// One file in a depot, readable at any offset. Cheap to clone.
#[derive(Clone)]
pub struct SteamDepotFile {
    reader: DepotReader,
    manifest: Arc<DepotManifest>,
    index: usize,
}

impl SteamDepotFile {
    pub fn entry(&self) -> &FileEntry {
        &self.manifest.files()[self.index]
    }

    pub fn depot(&self) -> DepotId {
        self.reader.depot()
    }

    pub fn len(&self) -> u64 {
        self.entry().size
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes `off..off + len`. Fetches only the chunks covering the range,
    /// up to the reader's concurrency limit at once. The whole range must be
    /// inside the file.
    pub async fn read_range(&self, off: u64, len: u64) -> Result<Vec<u8>, SteamError> {
        let size = self.len();
        let end = off
            .checked_add(len)
            .filter(|&e| e <= size)
            .ok_or(SteamError::OutOfRange { off, len, size })?;
        let chunks = self.entry().chunks_for(off, len);
        let parts: Vec<Bytes> =
            futures_util::future::try_join_all(chunks.iter().map(|c| self.reader.chunk(c))).await?;
        let mut out = Vec::with_capacity(len as usize);
        for (c, data) in chunks.iter().zip(&parts) {
            let from = off.max(c.offset) - c.offset;
            let to = end.min(c.end()) - c.offset;
            out.extend_from_slice(&data[from as usize..to as usize]);
        }
        Ok(out)
    }

    /// Read the whole file in order and check its xxHash64 against
    /// Wabbajack's `expected` hash. There is no separate length check here:
    /// the manifest's own validation already guarantees `entry().chunks`
    /// tiles `0..entry().size` exactly, so hashing every chunk in order
    /// covers the whole file by construction.
    pub async fn verify_xxh64(&self, expected: Xxh64) -> Result<(), SteamError> {
        let got = self.xxh64().await?;
        if got != expected {
            return Err(SteamError::Integrity(format!(
                "{}: xxHash64 {got} but the modlist expects {expected}",
                self.entry().path
            )));
        }
        Ok(())
    }

    /// Read the whole file in order and return its xxHash64 (Wabbajack's
    /// file hash).
    pub async fn xxh64(&self) -> Result<Xxh64, SteamError> {
        let mut hasher = xxhash_rust::xxh64::Xxh64::new(0);
        let window = self
            .reader
            .inner
            .fetcher
            .config()
            .max_concurrent_chunks
            .max(1);
        // Owned chunk refs, so the future is `Send` for any lifetime (it
        // can be boxed or spawned).
        let reader = &self.reader;
        let mut stream = futures_util::stream::iter(self.entry().chunks.clone())
            .map(|c| async move { reader.chunk(&c).await })
            .buffered(window);
        while let Some(data) = stream.try_next().await? {
            hasher.update(&data);
        }
        Ok(Xxh64(hasher.digest()))
    }

    /// A synchronous [`RangeRead`] view that runs reads on `handle`.
    ///
    /// `handle` must be a handle to a **multi-thread** Tokio runtime (the
    /// `#[tokio::main]`/`Runtime::new` default, or an explicit
    /// `Builder::new_multi_thread`). `Handle::block_on` on a
    /// `current_thread` runtime cannot drive that runtime's IO or timer
    /// drivers by itself — only a thread actually inside that runtime's own
    /// `Runtime::block_on` can — so reads issued from a non-runtime thread
    /// (see [`BlockingDepotFile`]) would simply hang forever waiting on a
    /// socket or timeout nothing is ever polling.
    pub fn into_blocking(self, handle: Handle) -> BlockingDepotFile {
        BlockingDepotFile { file: self, handle }
    }
}

/// [`SteamDepotFile`] as a blocking [`RangeRead`]. Call it from plain threads
/// or `spawn_blocking`, never from a tokio worker: there it returns an error
/// instead of blocking the runtime.
///
/// The [`Handle`] behind it (see [`SteamDepotFile::into_blocking`]) must
/// belong to a multi-thread runtime; on a `current_thread` runtime,
/// `Handle::block_on` from a plain (non-runtime) thread cannot drive that
/// runtime's IO/timer drivers and a read hangs instead of completing or
/// erroring.
///
/// This relies on catching the panic `Handle::block_on` raises when called
/// on a runtime worker thread, so it only works under `panic = "unwind"`
/// (Cargo's default); under `panic = "abort"` that panic aborts the process
/// instead of being caught here.
#[derive(Clone)]
pub struct BlockingDepotFile {
    file: SteamDepotFile,
    handle: Handle,
}

impl BlockingDepotFile {
    pub fn file(&self) -> &SteamDepotFile {
        &self.file
    }
}

impl RangeRead for BlockingDepotFile {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let fut = self.file.read_range(off, buf.len() as u64);
        // Handle::block_on panics, before polling, when called on a runtime
        // worker; turn that into an error rather than wedge the runtime.
        let r =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.handle.block_on(fut)))
                .map_err(|_| {
                    io::Error::other(
                        "block_on panicked (probably called on a tokio worker thread); \
                 use SteamDepotFile::read_range().await there instead",
                    )
                })?;
        buf.copy_from_slice(&r?);
        Ok(())
    }

    fn len(&self) -> u64 {
        self.file.len()
    }
}

/// Decoded chunks by id, evicting the least recently used over a byte budget.
struct ChunkLru {
    cap: usize,
    used: usize,
    tick: u64,
    map: HashMap<ChunkId, (Bytes, u64)>,
}

impl ChunkLru {
    fn new(cap: usize) -> Self {
        ChunkLru {
            cap,
            used: 0,
            tick: 0,
            map: HashMap::new(),
        }
    }

    fn get(&mut self, id: &ChunkId) -> Option<Bytes> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(id).map(|(b, t)| {
            *t = tick;
            b.clone()
        })
    }

    fn put(&mut self, id: ChunkId, data: Bytes) {
        if data.len() > self.cap || self.map.contains_key(&id) {
            return;
        }
        self.tick += 1;
        self.used += data.len();
        self.map.insert(id, (data, self.tick));
        while self.used > self.cap {
            let oldest = *self
                .map
                .iter()
                .min_by_key(|(_, (_, t))| *t)
                .map(|(k, _)| k)
                .expect("over budget implies non-empty");
            let (b, _) = self.map.remove(&oldest).expect("key just found");
            self.used -= b.len();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdn::{CdnEnd, CdnObserver, CdnPool, CdnRequest};
    use crate::testutil::{FakeCdn, FixtureFile, KEY, Mode, manifest_body, split};
    use std::future::Future;
    use std::time::Duration;

    fn data(n: usize) -> Vec<u8> {
        (0..n as u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect()
    }

    fn fast_cfg() -> CdnConfig {
        CdnConfig {
            request_timeout: Duration::from_millis(500),
            cooldown_base: Duration::from_millis(10),
            cooldown_max: Duration::from_millis(50),
            max_attempts: 4,
            ..CdnConfig::default()
        }
    }

    /// One depot (1) with one file "Data\\big.bin" of 10 000 bytes in
    /// 1000-byte chunks, served by `cdns`.
    async fn setup(
        cdns: &[&FakeCdn],
        cfg: CdnConfig,
    ) -> (DepotReader, Arc<DepotManifest>, Vec<u8>) {
        let bytes = data(10_000);
        let parts = split(&bytes, 1000);
        let body = manifest_body(
            DepotId(1),
            ManifestId(2),
            &[FixtureFile {
                path: "Data\\big.bin",
                data: &bytes,
                chunk: 1000,
            }],
            &[],
            None,
        );
        for cdn in cdns {
            cdn.put_chunks(DepotId(1), &parts, &KEY);
            cdn.put("/depot/1/manifest/2/5/77", body.clone());
        }
        let servers = cdns.iter().map(|c| c.server()).collect();
        let reader = DepotReader::new(AppId(10), DepotId(1), KEY, servers, cfg, None).unwrap();
        let manifest = Arc::new(reader.fetch_manifest(ManifestId(2), 77).await.unwrap());
        (reader, manifest, bytes)
    }

    #[tokio::test]
    async fn range_read_fetches_only_covering_chunks() {
        let cdn = FakeCdn::start().await;
        let (reader, m, bytes) = setup(&[&cdn], fast_cfg()).await;
        let f = reader.open(m, "data/BIG.bin").unwrap();
        assert_eq!(f.len(), 10_000);
        cdn.clear_log();
        let got = f.read_range(1500, 2000).await.unwrap(); // chunks 1, 2, 3
        assert_eq!(got, bytes[1500..3500]);
        assert_eq!(cdn.log().len(), 3);
        // Served from the chunk cache the second time.
        assert_eq!(f.read_range(2000, 10).await.unwrap(), bytes[2000..2010]);
        assert_eq!(cdn.log().len(), 3);
        assert_eq!(f.read_range(9990, 10).await.unwrap(), bytes[9990..]);
        assert!(f.read_range(5, 0).await.unwrap().is_empty());
        assert!(matches!(
            f.read_range(9990, 11).await,
            Err(SteamError::OutOfRange { .. })
        ));
        assert!(matches!(
            reader.open(f.manifest.clone(), "nope.bin"),
            Err(SteamError::FileNotFound(_))
        ));
    }

    #[tokio::test]
    async fn manifest_url_omits_a_zero_request_code() {
        let cdn = FakeCdn::start().await;
        let body = manifest_body(DepotId(1), ManifestId(3), &[], &[], None);
        cdn.put("/depot/1/manifest/3/5", body);
        let reader = DepotReader::new(
            AppId(10),
            DepotId(1),
            KEY,
            vec![cdn.server()],
            fast_cfg(),
            None,
        )
        .unwrap();
        reader.fetch_manifest(ManifestId(3), 0).await.unwrap();
    }

    #[tokio::test]
    async fn fails_over_on_error_status_corruption_and_timeout() {
        for mode in [Mode::Status(503), Mode::Corrupt, Mode::Hang, Mode::Truncate] {
            let bad = FakeCdn::start().await;
            let good = FakeCdn::start().await;
            let (reader, m, bytes) = setup(&[&bad, &good], fast_cfg()).await;
            bad.set_mode(mode.clone());
            let f = reader.open(m, "Data\\big.bin").unwrap();
            assert_eq!(f.read_range(0, 10_000).await.unwrap(), bytes, "{mode:?}");
        }
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        let cdn = FakeCdn::start().await;
        let (reader, m, _) = setup(&[&cdn], fast_cfg()).await;
        cdn.set_mode(Mode::Status(500));
        cdn.clear_log();
        let f = reader.open(m, "Data\\big.bin").unwrap();
        let err = f.read_range(0, 1).await.unwrap_err().to_string();
        assert!(
            err.contains("gave up after 4 attempts") && err.contains("HTTP 500"),
            "{err}"
        );
        assert_eq!(cdn.log().len(), 4);
    }

    struct FakeTokens(Mutex<u32>);
    impl CdnTokenSource for FakeTokens {
        fn cdn_auth_token<'a>(
            &'a self,
            _app: AppId,
            _depot: DepotId,
            _host: &'a str,
        ) -> std::pin::Pin<Box<dyn Future<Output = Result<Option<String>, SteamError>> + Send + 'a>>
        {
            *self.0.lock().unwrap() += 1;
            Box::pin(async { Ok(Some("?token=abc".to_string())) })
        }
    }

    #[tokio::test]
    async fn requests_a_cdn_auth_token_on_403() {
        let cdn = FakeCdn::start().await;
        let (plain, m, bytes) = setup(&[&cdn], fast_cfg()).await;
        drop(plain);
        cdn.set_mode(Mode::RequireToken("token=abc".into()));
        let tokens = Arc::new(FakeTokens(Mutex::new(0)));
        let reader = DepotReader::new(
            AppId(10),
            DepotId(1),
            KEY,
            vec![cdn.server()],
            fast_cfg(),
            Some(tokens.clone()),
        )
        .unwrap();
        let f = reader.open(m, "Data\\big.bin").unwrap();
        assert_eq!(f.read_range(0, 3000).await.unwrap(), bytes[..3000]);
        assert_eq!(*tokens.0.lock().unwrap(), 1, "one token per host");
        assert!(cdn.log().iter().any(|p| p.ends_with("?token=abc")));
    }

    /// Every download the observer was told about.
    #[derive(Default)]
    struct Seen(Mutex<Vec<(String, u64, u32, CdnEnd)>>);
    impl CdnObserver for Seen {
        fn request(&self, r: &CdnRequest<'_>) {
            let seen = (r.label.to_string(), r.bytes, r.retries, r.end);
            self.0.lock().unwrap().push(seen);
        }
    }

    /// A reader of depot 1 on `cdn` that reports to `seen`.
    fn observed(
        cdn: &FakeCdn,
        tokens: Option<Arc<dyn CdnTokenSource>>,
        seen: &Arc<Seen>,
    ) -> DepotReader {
        let cfg = fast_cfg();
        let fetcher = Fetcher::shared(
            AppId(10),
            DepotId(1),
            crate::cdn::http_client(&cfg).unwrap(),
            Arc::new(CdnPool::new(vec![cdn.server()], &cfg).unwrap()),
            cfg,
            tokens,
            Some(seen.clone()),
        );
        DepotReader::from_fetcher(fetcher, KEY)
    }

    #[tokio::test]
    async fn the_observer_hears_of_every_download_and_never_sees_a_token() {
        let cdn = FakeCdn::start().await;
        let (plain, m, bytes) = setup(&[&cdn], fast_cfg()).await;
        drop(plain);
        cdn.set_mode(Mode::RequireToken("token=abc".into()));
        let seen = Arc::new(Seen::default());
        let tokens = Arc::new(FakeTokens(Mutex::new(0)));
        let reader = observed(&cdn, Some(tokens), &seen);
        reader.fetch_manifest(ManifestId(2), 77).await.unwrap();
        let f = reader.open(m, "Data\\big.bin").unwrap();
        assert_eq!(f.read_range(0, 2000).await.unwrap(), bytes[..2000]);
        // The token did go out on the wire.
        assert!(cdn.log().iter().any(|p| p.ends_with("?token=abc")));

        let got = seen.0.lock().unwrap().clone();
        assert_eq!(got.len(), 3, "{got:?}");
        // The manifest: its id, not its request code (77).
        assert_eq!(got[0].0, "steam 1 manifest 2");
        // It met the 403, asked for a token and tried again.
        assert_eq!((got[0].2, got[0].3), (1, CdnEnd::Ok));
        let chunks = f.entry().chunks_for(0, 2000);
        let mut labels: Vec<&str> = got[1..].iter().map(|g| g.0.as_str()).collect();
        labels.sort_unstable();
        let mut expect: Vec<String> = chunks.iter().map(|c| format!("steam 1 {}", c.id)).collect();
        expect.sort_unstable();
        assert_eq!(labels, expect);
        for (label, n, _, end) in &got {
            assert!(*n > 0 && *end == CdnEnd::Ok, "{got:?}");
            for secret in ["token", "abc", "?", "://", "127.0.0.1", "77"] {
                assert!(!label.contains(secret), "{label}");
            }
        }
    }

    #[tokio::test]
    async fn the_observer_hears_of_failed_and_cancelled_downloads() {
        let cdn = FakeCdn::start().await;
        let (plain, m, _) = setup(&[&cdn], fast_cfg()).await;
        drop(plain);
        let seen = Arc::new(Seen::default());
        let reader = observed(&cdn, None, &seen);
        let f = reader.open(m, "Data\\big.bin").unwrap();
        let label = |i: usize| format!("steam 1 {}", f.entry().chunks[i].id);

        cdn.set_mode(Mode::Status(500));
        f.read_range(0, 1).await.unwrap_err();
        // Four attempts (`fast_cfg`), no body.
        assert_eq!(
            seen.0.lock().unwrap().pop(),
            Some((label(0), 0, 3, CdnEnd::Failed))
        );

        cdn.set_mode(Mode::Hang);
        tokio::time::timeout(Duration::from_millis(50), f.read_range(1000, 1))
            .await
            .unwrap_err();
        let got = seen.0.lock().unwrap().pop().unwrap();
        assert_eq!((got.0, got.3), (label(1), CdnEnd::Cancelled));
    }

    #[tokio::test]
    async fn a_host_that_refuses_whatever_the_token_gives_way_to_the_next() {
        let (refusing, good) = (FakeCdn::start().await, FakeCdn::start().await);
        let (plain, m, bytes) = setup(&[&refusing, &good], fast_cfg()).await;
        drop(plain);
        refusing.set_mode(Mode::Status(403));
        refusing.clear_log();
        let tokens = Arc::new(FakeTokens(Mutex::new(0)));
        let reader = DepotReader::new(
            AppId(10),
            DepotId(1),
            KEY,
            vec![refusing.server(), good.server()],
            fast_cfg(),
            Some(tokens.clone()),
        )
        .unwrap();
        let f = reader.open(m, "Data\\big.bin").unwrap();
        // One chunk. The pool keeps to the first server, which refuses.
        assert_eq!(f.read_range(0, 10).await.unwrap(), bytes[..10]);
        // It was tried bare, then once more with a token, and no further:
        // no token for every attempt, and the chunk came from the other.
        assert_eq!(refusing.log().len(), 2, "{:?}", refusing.log());
        assert_eq!(*tokens.0.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn concurrency_is_bounded() {
        let cdn = FakeCdn::start().await;
        let cfg = CdnConfig {
            max_concurrent_chunks: 2,
            ..fast_cfg()
        };
        let (reader, m, bytes) = setup(&[&cdn], cfg).await;
        cdn.set_delay(Duration::from_millis(30));
        let f = reader.open(m, "Data\\big.bin").unwrap();
        assert_eq!(f.read_range(0, 10_000).await.unwrap(), bytes);
        assert!(cdn.max_in_flight() <= 2, "{}", cdn.max_in_flight());
    }

    #[tokio::test]
    async fn verify_xxh64_checks_the_whole_file() {
        let cdn = FakeCdn::start().await;
        let (reader, m, bytes) = setup(&[&cdn], fast_cfg()).await;
        let f = reader.open(m, "Data\\big.bin").unwrap();
        f.verify_xxh64(Xxh64::of(&bytes)).await.unwrap();
        assert_eq!(f.xxh64().await.unwrap(), Xxh64::of(&bytes));
        let err = f.verify_xxh64(Xxh64(1)).await.unwrap_err();
        assert!(matches!(err, SteamError::Integrity(_)));
    }

    #[test]
    fn blocking_adapter_works_off_runtime_and_refuses_on_a_worker() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (cdn, f, bytes) = rt.block_on(async {
            let cdn = FakeCdn::start().await;
            let (reader, m, bytes) = setup(&[&cdn], fast_cfg()).await;
            (cdn, reader.open(m, "Data\\big.bin").unwrap(), bytes)
        });
        let bf = f.into_blocking(rt.handle().clone());
        // A plain thread.
        let t = bf.clone();
        let expect = bytes[4000..4100].to_vec();
        std::thread::spawn(move || {
            let mut buf = [0u8; 100];
            t.read_at(4000, &mut buf).unwrap();
            assert_eq!(buf.to_vec(), expect);
        })
        .join()
        .unwrap();
        // The blocking pool.
        let t = bf.clone();
        let v = rt
            .block_on(
                rt.spawn_blocking(move || aether_archive::range::read_vec(&t, 0, 10).unwrap()),
            )
            .unwrap();
        assert_eq!(v, bytes[..10]);
        // A runtime worker: an error, not a hang or a panic.
        let t = bf.clone();
        let r = rt.block_on(async move {
            tokio::spawn(async move { t.read_at(0, &mut [0u8; 4]) })
                .await
                .unwrap()
        });
        assert!(r.is_err());
        // Out of range maps to UnexpectedEof like other RangeRead sources.
        let e = bf.read_at(9999, &mut [0u8; 2]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
        drop(cdn);
    }

    #[tokio::test]
    async fn empty_files_read_and_verify_without_the_network() {
        let cdn = FakeCdn::start().await;
        let body = manifest_body(
            DepotId(1),
            ManifestId(4),
            &[FixtureFile {
                path: "empty.ini",
                data: b"",
                chunk: 1000,
            }],
            &[],
            None,
        );
        cdn.put("/depot/1/manifest/4/5", body);
        let reader = DepotReader::new(
            AppId(10),
            DepotId(1),
            KEY,
            vec![cdn.server()],
            fast_cfg(),
            None,
        )
        .unwrap();
        let m = Arc::new(reader.fetch_manifest(ManifestId(4), 0).await.unwrap());
        let f = reader.open(m, "EMPTY.INI").unwrap();
        cdn.clear_log();
        assert!(f.is_empty());
        assert!(f.read_range(0, 0).await.unwrap().is_empty());
        f.verify_xxh64(Xxh64::of(b"")).await.unwrap();
        assert!(cdn.log().is_empty());
    }

    #[test]
    fn many_blocking_readers_at_once() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (cdn, f, bytes) = rt.block_on(async {
            let cdn = FakeCdn::start().await;
            let cfg = CdnConfig {
                max_concurrent_chunks: 2,
                ..fast_cfg()
            };
            let (reader, m, bytes) = setup(&[&cdn], cfg).await;
            (cdn, reader.open(m, "Data\\big.bin").unwrap(), bytes)
        });
        cdn.set_delay(Duration::from_millis(5));
        let bf = f.into_blocking(rt.handle().clone());
        let threads: Vec<_> = (0..8u64)
            .map(|i| {
                let (t, bytes) = (bf.clone(), bytes.clone());
                std::thread::spawn(move || {
                    let off = i * 1100;
                    let mut buf = vec![0u8; 1500];
                    t.read_at(off, &mut buf).unwrap();
                    assert_eq!(buf, bytes[off as usize..off as usize + 1500]);
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        drop(cdn);
    }

    #[test]
    fn lru_evicts_least_recently_used() {
        let id = |n| ChunkId([n; 20]);
        let mut lru = ChunkLru::new(10);
        lru.put(id(1), Bytes::from(vec![0; 4]));
        lru.put(id(2), Bytes::from(vec![0; 4]));
        assert!(lru.get(&id(1)).is_some()); // 1 is now newer than 2
        lru.put(id(3), Bytes::from(vec![0; 4])); // over budget: evict 2
        assert!(lru.get(&id(2)).is_none());
        assert!(lru.get(&id(1)).is_some() && lru.get(&id(3)).is_some());
        lru.put(id(4), Bytes::from(vec![0; 11])); // larger than the budget: skipped
        assert!(lru.get(&id(4)).is_none());
    }
}

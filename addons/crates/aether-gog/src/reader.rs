//! Random access to depot files: map a byte range to the chunks covering
//! it, fetch those from the product's secure link, check the compressed
//! MD5, inflate, check the inflated MD5, and splice. A chunk that fails a
//! check is fetched again per the [`RetryPolicy`](aether_net::RetryPolicy)
//! and is never served or cached.
//!
//! Game I/O is many small concurrent reads, so a chunk is downloaded once
//! however many reads want it at the same time (they share one spawned
//! download), and a large read fetches its chunks at most
//! [`HttpConfig::parallel_parts`](aether_net::HttpConfig) at a time, in
//! order, copying each out as it arrives: it holds no more than that many
//! inflated chunks beyond the cache.
//!
//! Ported from NexusMods.App `src/NexusMods.Networking.GOG/ChunkedStreamSource.cs`
//! (GPL-3.0).
use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use aether_archive::RangeRead;
use aether_net::SourceError;
use aether_net::http::{check, read_body_max};
use bytes::Bytes;
use futures_util::future::{BoxFuture, FutureExt, Shared};
use futures_util::stream::{self, StreamExt};
use md5::{Digest, Md5};
use tokio::runtime::Handle;

use crate::content::{GogContent, SecureLink};
use crate::error::GogError;
use crate::ids::ProductId;
use crate::manifest::{Chunk, hex, inflate};

/// Slack over a chunk's compressed size for its download cap.
const CHUNK_BODY_SLACK: u64 = 64 << 10;

/// One file of a depot, readable at any offset. Cheap to clone.
#[derive(Clone)]
pub struct GogDepotFile {
    content: GogContent,
    product: ProductId,
    chunks: Arc<[Chunk]>,
    /// `ends[i]`: offset just past chunk `i` in the chunk stream.
    ends: Arc<[u64]>,
    /// Where the file starts in the chunk stream (non-zero only inside a
    /// small-files container).
    base: u64,
    len: u64,
}

impl GogDepotFile {
    pub(crate) fn new(
        content: GogContent,
        product: ProductId,
        chunks: Arc<[Chunk]>,
        base: u64,
        len: u64,
    ) -> Self {
        let ends: Vec<u64> = chunks
            .iter()
            .scan(0u64, |end, c| {
                *end += c.size;
                Some(*end)
            })
            .collect();
        GogDepotFile {
            content,
            product,
            chunks,
            ends: ends.into(),
            base,
            len,
        }
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn product(&self) -> ProductId {
        self.product
    }

    /// Read up to `buf.len()` bytes at `offset`; returns how many were read,
    /// which is short only at the end of the file and 0 at or past it.
    pub async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, GogError> {
        if offset >= self.len || buf.is_empty() {
            return Ok(0);
        }
        let n = (self.len - offset).min(buf.len() as u64);
        let start = self.base + offset;
        let end = start + n;
        // The first chunk ending after `start`, through the first ending at
        // or after `end`.
        let first = self.ends.partition_point(|&e| e <= start);
        let last = self.ends.partition_point(|&e| e < end);
        let wanted = first..=last.min(self.chunks.len() - 1);
        let ahead = self.content.inner.http.config().parallel_parts.max(1);
        let mut parts = stream::iter(
            wanted
                .clone()
                .map(|i| self.content.chunk(self.product, &self.chunks[i])),
        )
        .buffered(ahead);
        let mut at = 0usize;
        for i in wanted {
            let data = parts.next().await.expect("one result per wanted chunk")?;
            let chunk_start = self.ends[i] - self.chunks[i].size;
            let from = start.max(chunk_start) - chunk_start;
            let to = end.min(self.ends[i]) - chunk_start;
            let piece = &data[from as usize..to as usize];
            buf[at..at + piece.len()].copy_from_slice(piece);
            at += piece.len();
        }
        debug_assert_eq!(at as u64, n);
        Ok(at)
    }

    /// A synchronous [`RangeRead`] view that runs reads on `handle`, which
    /// must belong to a **multi-thread** runtime (see aether-steam's
    /// `SteamDepotFile::into_blocking` for why).
    pub fn into_blocking(self, handle: Handle) -> BlockingGogFile {
        BlockingGogFile { file: self, handle }
    }
}

/// [`GogDepotFile`] as a blocking [`RangeRead`]. Call it from plain threads
/// or `spawn_blocking`, never from a tokio worker: there it returns an
/// error instead of blocking the runtime (this relies on `panic = "unwind"`).
#[derive(Clone)]
pub struct BlockingGogFile {
    file: GogDepotFile,
    handle: Handle,
}

impl BlockingGogFile {
    pub fn file(&self) -> &GogDepotFile {
        &self.file
    }
}

impl RangeRead for BlockingGogFile {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let size = self.file.len();
        if off
            .checked_add(buf.len() as u64)
            .is_none_or(|end| end > size)
        {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "read of {} bytes at {off} past end of {size}-byte GOG file",
                    buf.len()
                ),
            ));
        }
        let n = block_on(&self.handle, self.file.read_at(off, buf))??;
        if n != buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short read from GOG file",
            ));
        }
        Ok(())
    }

    fn len(&self) -> u64 {
        self.file.len()
    }
}

/// Run `fut` to completion on `handle` from a synchronous thread. On a
/// tokio worker thread, where blocking would stall the runtime,
/// `Handle::block_on` panics before polling; that is caught and returned
/// as an error (this relies on `panic = "unwind"`).
pub(crate) fn block_on<F: std::future::Future>(handle: &Handle, fut: F) -> io::Result<F::Output> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle.block_on(fut))).map_err(|_| {
        io::Error::other(
            "block_on panicked (probably called on a tokio worker thread); \
             use GogDepotFile::read_at().await there instead",
        )
    })
}

/// A chunk download in flight, shared by every read waiting for it.
pub(crate) type ChunkFetch = Shared<BoxFuture<'static, Result<Bytes, Arc<GogError>>>>;

impl GogContent {
    /// One chunk's inflated bytes, from the in-memory cache, a download of
    /// it already under way, or a new download. The download runs as its
    /// own task, so it finishes (and fills the cache) even if every read
    /// waiting for it is dropped.
    pub(crate) async fn chunk(&self, product: ProductId, c: &Chunk) -> Result<Bytes, GogError> {
        let id = c.compressed_md5;
        let fetch = {
            // The in-flight map is locked across the cache check, and a
            // download caches its chunk before leaving the map, so a chunk
            // is never fetched twice at once.
            let mut inflight = self.inner.inflight.lock().unwrap();
            if let Some(b) = self.inner.chunks.lock().unwrap().get(&id) {
                return Ok(b);
            }
            match inflight.get(&id) {
                Some(f) => f.clone(),
                None => {
                    let this = self.clone();
                    let c = *c;
                    let task = tokio::spawn(async move {
                        let r = this.download_chunk(product, &c).await;
                        if let Ok(b) = &r {
                            this.inner.chunks.lock().unwrap().put(id, b.clone());
                        }
                        this.inner.inflight.lock().unwrap().remove(&id);
                        r.map_err(Arc::new)
                    });
                    let f = async move {
                        task.await
                            .unwrap_or_else(|e| Err(Arc::new(GogError::Io(io::Error::other(e)))))
                    }
                    .boxed()
                    .shared();
                    inflight.insert(id, f.clone());
                    f
                }
            }
        };
        // The last waiter gets the error itself; any others a copy.
        fetch
            .await
            .map_err(|e| Arc::try_unwrap(e).unwrap_or_else(|e| e.duplicate()))
    }

    /// Download, check and inflate one chunk.
    async fn download_chunk(&self, product: ProductId, c: &Chunk) -> Result<Bytes, GogError> {
        let link = self.secure_link(product).await?;
        let data = match self.fetch_chunk(&link, c).await {
            // The link was refused (expired early or revoked): once more
            // with a fresh one.
            Err(SourceError::Status {
                status: 401 | 403, ..
            }) => {
                // Only this link: another download may have renewed it.
                self.drop_secure_link(product, &link).await;
                let link = self.secure_link(product).await?;
                self.fetch_chunk(&link, c).await?
            }
            r => r?,
        };
        Ok(data)
    }

    async fn fetch_chunk(&self, link: &SecureLink, c: &Chunk) -> Result<Bytes, SourceError> {
        let http = &self.inner.http;
        let id = hex(&c.compressed_md5);
        let url = link.chunk_url(&id)?;
        // The link's token may sit in the URL path, which `redact` keeps:
        // errors name the chunk by a URL with the secrets replaced.
        let shown = link.shown_chunk_url(&id);
        let job = http.start(format!("gog chunk {id}"), Some(c.compressed_size));
        let max = c.compressed_size.saturating_add(CHUNK_BODY_SLACK);
        let r = http
            .config()
            .retry
            .run(&job, || async {
                let _permit = http.permit(&url).await;
                let attempt = async {
                    let resp = http
                        .client()
                        .get(url.clone())
                        .send()
                        .await
                        .map_err(|e| SourceError::network(&shown, e))?;
                    let body = read_body_max(check(resp, &shown).await?, &shown, &job, max).await?;
                    let chunk = *c;
                    tokio::task::spawn_blocking(move || verify_chunk(&body, &chunk))
                        .await
                        .map_err(|e| SourceError::Io(io::Error::other(e)))?
                };
                // Scrubbed before the retry loop reports it.
                attempt.await.map_err(|e| link.scrub(e))
            })
            .await;
        job.complete(r)
    }
}

/// Check `body` against the chunk's compressed MD5, inflate it and check
/// the result's length and MD5. Any mismatch is a retryable
/// [`SourceError::CorruptPart`].
fn verify_chunk(body: &[u8], c: &Chunk) -> Result<Bytes, SourceError> {
    let corrupt = |msg: String| SourceError::CorruptPart {
        what: format!("GOG chunk {}", hex(&c.compressed_md5)),
        msg,
    };
    let got: [u8; 16] = Md5::digest(body).into();
    if got != c.compressed_md5 {
        return Err(corrupt(format!(
            "compressed MD5 is {} ({} bytes)",
            hex(&got),
            body.len()
        )));
    }
    let raw = inflate(body, c.size).map_err(corrupt)?;
    if raw.len() as u64 != c.size {
        return Err(corrupt(format!(
            "inflates to {} bytes, not {}",
            raw.len(),
            c.size
        )));
    }
    let got: [u8; 16] = Md5::digest(&raw).into();
    if got != c.md5 {
        return Err(corrupt(format!("inflated MD5 is {}", hex(&got))));
    }
    Ok(Bytes::from(raw))
}

/// Inflated chunks by compressed MD5, evicting the least recently used
/// beyond `max_chunks` entries or `max_bytes` bytes.
pub(crate) struct ChunkLru {
    max_chunks: usize,
    max_bytes: usize,
    used: usize,
    tick: u64,
    map: HashMap<[u8; 16], (Bytes, u64)>,
}

impl ChunkLru {
    pub(crate) fn new(max_chunks: usize, max_bytes: usize) -> Self {
        ChunkLru {
            max_chunks,
            max_bytes,
            used: 0,
            tick: 0,
            map: HashMap::new(),
        }
    }

    pub(crate) fn get(&mut self, id: &[u8; 16]) -> Option<Bytes> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(id).map(|(b, t)| {
            *t = tick;
            b.clone()
        })
    }

    pub(crate) fn put(&mut self, id: [u8; 16], data: Bytes) {
        if self.max_chunks == 0 || data.len() > self.max_bytes || self.map.contains_key(&id) {
            return;
        }
        self.tick += 1;
        self.used += data.len();
        self.map.insert(id, (data, self.tick));
        while self.used > self.max_bytes || self.map.len() > self.max_chunks {
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
    use std::io::Write;

    fn chunk_of(raw: &[u8]) -> (Vec<u8>, Chunk) {
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(raw).unwrap();
        let body = e.finish().unwrap();
        let c = Chunk {
            compressed_md5: Md5::digest(&body).into(),
            md5: Md5::digest(raw).into(),
            size: raw.len() as u64,
            compressed_size: body.len() as u64,
        };
        (body, c)
    }

    #[test]
    fn verify_checks_both_md5s_and_the_length() {
        let (body, c) = chunk_of(b"hello chunk");
        assert_eq!(&verify_chunk(&body, &c).unwrap()[..], b"hello chunk");
        let mut bad = body.clone();
        bad[3] ^= 1;
        let e = verify_chunk(&bad, &c).unwrap_err();
        assert!(matches!(e, SourceError::CorruptPart { .. }) && e.is_retryable());
        // A manifest whose inflated MD5 disagrees with the data.
        let wrong = Chunk { md5: [0; 16], ..c };
        assert!(verify_chunk(&body, &wrong).is_err());
        let short = Chunk { size: 5, ..c };
        assert!(verify_chunk(&body, &short).is_err());
    }

    #[test]
    fn lru_evicts_by_count_and_bytes() {
        let id = |n| [n; 16];
        let mut lru = ChunkLru::new(2, 10);
        lru.put(id(1), Bytes::from(vec![0; 4]));
        lru.put(id(2), Bytes::from(vec![0; 4]));
        assert!(lru.get(&id(1)).is_some());
        lru.put(id(3), Bytes::from(vec![0; 1])); // over the count: evict 2
        assert!(lru.get(&id(2)).is_none());
        assert!(lru.get(&id(1)).is_some() && lru.get(&id(3)).is_some());
        lru.put(id(4), Bytes::from(vec![0; 5])); // over both: evict 1, the oldest
        assert!(lru.get(&id(1)).is_none());
        assert!(lru.get(&id(3)).is_some() && lru.get(&id(4)).is_some());
        lru.put(id(6), Bytes::from(vec![0; 9])); // over the bytes: evict 3 and 4
        assert!(lru.get(&id(3)).is_none() && lru.get(&id(4)).is_none());
        lru.put(id(5), Bytes::from(vec![0; 11])); // bigger than the budget
        assert!(lru.get(&id(5)).is_none() && lru.get(&id(6)).is_some());
    }
}

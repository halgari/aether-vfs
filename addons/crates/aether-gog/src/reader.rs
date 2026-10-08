//! Random access to depot files: map a byte range to the chunks covering
//! it, fetch those from the product's secure link, check the compressed
//! MD5, inflate, check the inflated MD5, and splice. A chunk that fails a
//! check is fetched again per the [`RetryPolicy`](aether_net::RetryPolicy)
//! and is never served or cached.
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
        chunks: Vec<Chunk>,
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
            chunks: chunks.into(),
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
        let parts = futures_util::future::try_join_all(
            wanted
                .clone()
                .map(|i| self.content.chunk(self.product, &self.chunks[i])),
        )
        .await?;
        let mut at = 0usize;
        for (i, data) in wanted.zip(&parts) {
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
        let fut = self.file.read_at(off, buf);
        // Handle::block_on panics, before polling, on a runtime worker.
        let n =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.handle.block_on(fut)))
                .map_err(|_| {
                io::Error::other(
                    "block_on panicked (probably called on a tokio worker thread); \
                 use GogDepotFile::read_at().await there instead",
                )
            })??;
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

impl GogContent {
    /// One chunk's inflated bytes, from the in-memory cache or the CDN.
    pub(crate) async fn chunk(&self, product: ProductId, c: &Chunk) -> Result<Bytes, GogError> {
        if let Some(b) = self.inner.chunks.lock().unwrap().get(&c.compressed_md5) {
            return Ok(b);
        }
        let link = self.secure_link(product).await?;
        let data = match self.fetch_chunk(&link, c).await {
            // The link was refused (expired early or revoked): once more
            // with a fresh one.
            Err(SourceError::Status {
                status: 401 | 403, ..
            }) => {
                self.drop_secure_link(product).await;
                let link = self.secure_link(product).await?;
                self.fetch_chunk(&link, c).await?
            }
            r => r?,
        };
        self.inner
            .chunks
            .lock()
            .unwrap()
            .put(c.compressed_md5, data.clone());
        Ok(data)
    }

    async fn fetch_chunk(&self, link: &SecureLink, c: &Chunk) -> Result<Bytes, SourceError> {
        let http = &self.inner.http;
        let id = hex(&c.compressed_md5);
        let url = link.chunk_url(&id)?;
        let job = http.start(format!("gog chunk {id}"), Some(c.compressed_size));
        let max = c.compressed_size.saturating_add(CHUNK_BODY_SLACK);
        let r = http
            .config()
            .retry
            .run(&job, || async {
                let _permit = http.permit(&url).await;
                let resp = http
                    .client()
                    .get(url.clone())
                    .send()
                    .await
                    .map_err(|e| SourceError::network(&url, e))?;
                let body = read_body_max(check(resp, &url).await?, &url, &job, max).await?;
                let chunk = *c;
                tokio::task::spawn_blocking(move || verify_chunk(&body, &chunk))
                    .await
                    .map_err(|e| SourceError::Io(io::Error::other(e)))?
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

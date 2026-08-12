//! [`CachingBackend`]: wraps any [`Backend`] with block-aligned caching.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use vfs_protocol::{
    bad_fh, map_io_err, Backend, BackendHandle, DirEntry, Stat, OPEN_WRITE,
};

use crate::store::{BlockCache, BlockKey};

struct OpenRec {
    inner: BackendHandle,
    size: u64,
    file_id: u64,
    is_dir: bool,
}

/// Caching facade over an inner backend. `source_id` namespaces cache keys.
pub struct CachingBackend {
    inner: Arc<dyn Backend>,
    cache: Arc<BlockCache>,
    source_id: u64,
    next: AtomicU64,
    opens: Mutex<HashMap<u64, OpenRec>>,
}

impl CachingBackend {
    pub fn new(inner: Arc<dyn Backend>, cache: Arc<BlockCache>, source_id: u64) -> Self {
        Self {
            inner,
            cache,
            source_id,
            next: AtomicU64::new(1),
            opens: Mutex::new(HashMap::new()),
        }
    }

    fn file_id_for(path: &str, size: u64, mtime: i64) -> u64 {
        // Stable-enough for immutable sources: path hash mixed with size/mtime.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in path.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h ^= size;
        h = h.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        h ^ (mtime as u64)
    }
}

impl Backend for CachingBackend {
    fn getattr(&self, path: &str) -> Result<Option<Stat>, i32> {
        self.inner.getattr(path)
    }

    fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, i32> {
        self.inner.readdir(path)
    }

    fn open(&self, path: &str, flags: u32) -> Result<(BackendHandle, u64, bool), i32> {
        if flags & OPEN_WRITE != 0 {
            return Err(vfs_protocol::bad_request());
        }
        let st = self.inner.getattr(path)?;
        let (inner, size, is_dir) = self.inner.open(path, flags)?;
        let (file_id, size) = if let Some(s) = st {
            (Self::file_id_for(path, s.size, s.mtime), size)
        } else {
            (Self::file_id_for(path, size, 0), size)
        };
        let h = self.next.fetch_add(1, Ordering::Relaxed);
        self.opens
            .lock()
            .map_err(|_| map_io_err())?
            .insert(
                h,
                OpenRec {
                    inner,
                    size,
                    file_id,
                    is_dir,
                },
            );
        Ok((h, size, is_dir))
    }

    fn read(&self, bh: BackendHandle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let rec = {
            let g = self.opens.lock().map_err(|_| map_io_err())?;
            let r = g.get(&bh).ok_or_else(bad_fh)?;
            OpenRec {
                inner: r.inner,
                size: r.size,
                file_id: r.file_id,
                is_dir: r.is_dir,
            }
        };
        if rec.is_dir {
            return Err(vfs_protocol::is_dir());
        }
        if offset >= rec.size || buf.is_empty() {
            return Ok(0);
        }
        let end = (offset + buf.len() as u64).min(rec.size);
        let mut written = 0usize;
        let bs = self.cache.block_size();
        let mut off = offset;
        while off < end {
            let block_index = off / bs;
            let block_off = (off % bs) as usize;
            let key = BlockKey {
                source_id: self.source_id,
                file_id: rec.file_id,
                block_index,
            };
            let block = if let Some(b) = self.cache.get(&key) {
                b
            } else {
                let block_start = block_index * bs;
                let want = bs.min(rec.size.saturating_sub(block_start)) as usize;
                let mut raw = vec![0u8; want];
                let mut filled = 0usize;
                while filled < want {
                    let n = self
                        .inner
                        .read(rec.inner, block_start + filled as u64, &mut raw[filled..])?;
                    if n == 0 {
                        break;
                    }
                    filled += n;
                }
                raw.truncate(filled);
                self.cache.put(key, raw.clone());
                raw
            };
            if block_off >= block.len() {
                break;
            }
            let take = (end - off).min((block.len() - block_off) as u64) as usize;
            buf[written..written + take]
                .copy_from_slice(&block[block_off..block_off + take]);
            written += take;
            off += take as u64;
        }
        Ok(written)
    }

    fn release(&self, bh: BackendHandle) -> Result<(), i32> {
        let rec = self
            .opens
            .lock()
            .map_err(|_| map_io_err())?
            .remove(&bh)
            .ok_or_else(bad_fh)?;
        self.inner.release(rec.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::CacheConfig;
    use vfs_protocol::OPEN_READ;

    struct CountingBackend {
        data: Vec<u8>,
        reads: AtomicU64,
    }

    impl Backend for CountingBackend {
        fn getattr(&self, path: &str) -> Result<Option<Stat>, i32> {
            if path == "f" {
                Ok(Some(Stat {
                    kind: vfs_protocol::KIND_FILE,
                    size: self.data.len() as u64,
                    mtime: 1,
                }))
            } else {
                Ok(None)
            }
        }
        fn readdir(&self, _: &str) -> Result<Vec<DirEntry>, i32> {
            Ok(vec![])
        }
        fn open(&self, path: &str, _: u32) -> Result<(BackendHandle, u64, bool), i32> {
            if path != "f" {
                return Err(vfs_protocol::not_found());
            }
            Ok((1, self.data.len() as u64, false))
        }
        fn read(&self, _: BackendHandle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            let start = offset as usize;
            if start >= self.data.len() {
                return Ok(0);
            }
            let n = buf.len().min(self.data.len() - start);
            buf[..n].copy_from_slice(&self.data[start..start + n]);
            Ok(n)
        }
        fn release(&self, _: BackendHandle) -> Result<(), i32> {
            Ok(())
        }
    }

    #[test]
    fn second_read_is_cache_hit() {
        let raw = Arc::new(CountingBackend {
            data: vec![9u8; 100],
            reads: AtomicU64::new(0),
        });
        let cache = Arc::new(BlockCache::new(CacheConfig {
            block_size: 32,
            ram_budget: 1024,
            disk_dir: None,
        }));
        let be = CachingBackend::new(raw.clone(), cache.clone(), 1);
        let (h, _, _) = be.open("f", OPEN_READ).unwrap();
        let mut buf = [0u8; 50];
        assert_eq!(be.read(h, 0, &mut buf).unwrap(), 50);
        let reads_after_first = raw.reads.load(Ordering::Relaxed);
        assert!(reads_after_first >= 1);
        assert_eq!(be.read(h, 0, &mut buf).unwrap(), 50);
        assert_eq!(
            raw.reads.load(Ordering::Relaxed),
            reads_after_first,
            "second read should not touch source"
        );
        assert!(cache.stats().hits >= 1);
        be.release(h).unwrap();
    }

    #[test]
    fn unaligned_cross_block_read() {
        // Pattern: bytes 0..=99 are value = index.
        let mut data = vec![0u8; 100];
        for (i, b) in data.iter_mut().enumerate() {
            *b = i as u8;
        }
        let raw = Arc::new(CountingBackend {
            data,
            reads: AtomicU64::new(0),
        });
        let cache = Arc::new(BlockCache::new(CacheConfig {
            block_size: 16,
            ram_budget: 4096,
            disk_dir: None,
        }));
        let be = CachingBackend::new(raw, cache, 7);
        let (h, _, _) = be.open("f", OPEN_READ).unwrap();
        let mut buf = [0u8; 20];
        // Starts mid-block (offset 10) and spans into the next.
        assert_eq!(be.read(h, 10, &mut buf).unwrap(), 20);
        for i in 0..20 {
            assert_eq!(buf[i], (10 + i) as u8, "byte at {i}");
        }
        be.release(h).unwrap();
    }
}

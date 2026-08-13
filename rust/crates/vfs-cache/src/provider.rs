//! [`CachingProvider`]: wraps any [`Provider`] with block-aligned caching.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use vfs_provider::{
    bad_fh, bad_request, is_dir, map_io_err, Capabilities, DirEntry, Handle, Provider, Stat,
    VPath, OPEN_WRITE,
};

use crate::store::{BlockCache, BlockKey};

struct OpenRec {
    inner: Handle,
    size: u64,
    file_id: u64,
    is_dir: bool,
}

/// Caching facade over an inner provider. `source_id` namespaces cache keys.
pub struct CachingProvider {
    inner: Arc<dyn Provider>,
    cache: Arc<BlockCache>,
    source_id: u64,
    next: AtomicU64,
    opens: Mutex<HashMap<u64, OpenRec>>,
}

impl CachingProvider {
    pub fn new(inner: Arc<dyn Provider>, cache: Arc<BlockCache>, source_id: u64) -> Self {
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

impl Provider for CachingProvider {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities().cached()
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        self.inner.getattr(p)
    }

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        self.inner.readdir(p)
    }

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        if flags & OPEN_WRITE != 0 {
            return Err(bad_request());
        }
        let path = p.rel;
        let st = self.inner.getattr(p)?;
        let (inner, size, is_dir) = self.inner.open(p, flags)?;
        // DEFERRED (Stage 2): file_id_for keys on `path` alone, not `p.root`.
        // Two different roots serving the same relative path with the same
        // size and mtime would collide on the same cache entry. Inert today
        // because every call site addresses VPath under RootId(0) — Stage 2
        // makes roots real and must fold `p.root` into this key.
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

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let rec = {
            let g = self.opens.lock().map_err(|_| map_io_err())?;
            let r = g.get(&h).ok_or_else(bad_fh)?;
            OpenRec {
                inner: r.inner,
                size: r.size,
                file_id: r.file_id,
                is_dir: r.is_dir,
            }
        };
        if rec.is_dir {
            return Err(is_dir());
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
                        .read_at(rec.inner, block_start + filled as u64, &mut raw[filled..])?;
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

    fn close(&self, h: Handle) -> Result<(), i32> {
        let rec = self
            .opens
            .lock()
            .map_err(|_| map_io_err())?
            .remove(&h)
            .ok_or_else(bad_fh)?;
        self.inner.close(rec.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::CacheConfig;
    use vfs_provider::{KIND_FILE, OPEN_READ};

    #[test]
    fn caching_provider_over_the_fixture_tree_passes_conformance() {
        use vfs_provider::conformance::MemFixture;
        let inner: Arc<dyn Provider> = Arc::new(MemFixture::new());
        let cache = Arc::new(BlockCache::new(CacheConfig::default()));
        let p: Arc<dyn Provider> = Arc::new(CachingProvider::new(inner, cache, 1));
        vfs_provider::assert_conformance(p);
    }

    struct CountingProvider {
        data: Vec<u8>,
        reads: AtomicU64,
    }

    impl Provider for CountingProvider {
        fn capabilities(&self) -> Capabilities {
            Capabilities::read_only()
        }
        fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
            if p.rel == "f" {
                Ok(Some(Stat {
                    kind: KIND_FILE,
                    size: self.data.len() as u64,
                    mtime: 1,
                }))
            } else {
                Ok(None)
            }
        }
        fn readdir(&self, _: VPath) -> Result<Vec<DirEntry>, i32> {
            Ok(vec![])
        }
        fn open(&self, p: VPath, _: u32) -> Result<(Handle, u64, bool), i32> {
            if p.rel != "f" {
                return Err(vfs_provider::not_found());
            }
            Ok((1, self.data.len() as u64, false))
        }
        fn read_at(&self, _: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            let start = offset as usize;
            if start >= self.data.len() {
                return Ok(0);
            }
            let n = buf.len().min(self.data.len() - start);
            buf[..n].copy_from_slice(&self.data[start..start + n]);
            Ok(n)
        }
        fn close(&self, _: Handle) -> Result<(), i32> {
            Ok(())
        }
    }

    #[test]
    fn second_read_is_cache_hit() {
        let raw = Arc::new(CountingProvider {
            data: vec![9u8; 100],
            reads: AtomicU64::new(0),
        });
        let cache = Arc::new(BlockCache::new(CacheConfig {
            block_size: 32,
            ram_budget: 1024,
            disk_dir: None,
        }));
        let be = CachingProvider::new(raw.clone(), cache.clone(), 1);
        let (h, _, _) = be.open(VPath::at_default("f"), OPEN_READ).unwrap();
        let mut buf = [0u8; 50];
        assert_eq!(be.read_at(h, 0, &mut buf).unwrap(), 50);
        let reads_after_first = raw.reads.load(Ordering::Relaxed);
        assert!(reads_after_first >= 1);
        assert_eq!(be.read_at(h, 0, &mut buf).unwrap(), 50);
        assert_eq!(
            raw.reads.load(Ordering::Relaxed),
            reads_after_first,
            "second read should not touch source"
        );
        assert!(cache.stats().hits >= 1);
        be.close(h).unwrap();
    }

    #[test]
    fn unaligned_cross_block_read() {
        // Pattern: bytes 0..=99 are value = index.
        let mut data = vec![0u8; 100];
        for (i, b) in data.iter_mut().enumerate() {
            *b = i as u8;
        }
        let raw = Arc::new(CountingProvider {
            data,
            reads: AtomicU64::new(0),
        });
        let cache = Arc::new(BlockCache::new(CacheConfig {
            block_size: 16,
            ram_budget: 4096,
            disk_dir: None,
        }));
        let be = CachingProvider::new(raw, cache, 7);
        let (h, _, _) = be.open(VPath::at_default("f"), OPEN_READ).unwrap();
        let mut buf = [0u8; 20];
        // Starts mid-block (offset 10) and spans into the next.
        assert_eq!(be.read_at(h, 10, &mut buf).unwrap(), 20);
        for (i, b) in buf.iter().enumerate().take(20) {
            assert_eq!(*b, (10 + i) as u8, "byte at {i}");
        }
        be.close(h).unwrap();
    }

    #[test]
    fn caching_answers_the_slow_marker() {
        use std::sync::Arc;
        use vfs_provider::{Access, Capabilities, DirEntry, Handle, Provider, Stat, VPath};

        struct SlowInner;
        impl Provider for SlowInner {
            fn capabilities(&self) -> Capabilities {
                Capabilities { slow: true, preferred_block: Some(1 << 20), ..Capabilities::read_only() }
            }
            fn getattr(&self, _p: VPath) -> Result<Option<Stat>, i32> { Ok(None) }
            fn readdir(&self, _p: VPath) -> Result<Vec<DirEntry>, i32> { Ok(Vec::new()) }
            fn open(&self, _p: VPath, _f: u32) -> Result<(Handle, u64, bool), i32> {
                Err(vfs_provider::not_found())
            }
            fn close(&self, _h: Handle) -> Result<(), i32> { Ok(()) }
            fn read_at(&self, _h: Handle, _o: u64, _b: &mut [u8]) -> Result<usize, i32> { Ok(0) }
        }

        let cache = Arc::new(crate::BlockCache::new(crate::CacheConfig::default()));
        let p = CachingProvider::new(Arc::new(SlowInner), cache, 1);
        let caps = p.capabilities();
        assert!(!caps.slow, "a cached provider is no longer slow");
        assert_eq!(caps.access, Access::Read, "access passes through");
        assert_eq!(caps.preferred_block, Some(1 << 20), "the block hint survives");
    }
}

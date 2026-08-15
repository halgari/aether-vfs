//! [`CachingProvider`]: wraps any [`Provider`] with block-aligned caching.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use vfs_provider::{
    bad_fh, is_dir, map_io_err, Capabilities, DirEntry, Handle, Provider, RootId, SetAttr, Stat,
    VPath,
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

    /// Stable-enough for immutable sources within one root: path hash mixed
    /// with size/mtime/root. This is a heuristic identity, not a content
    /// hash — it is only sound because `immutable` sources are the only
    /// ones this cache is allowed to hold onto without re-checking the OS
    /// (see the write/set_len invalidation paths below, which throw the
    /// whole file away rather than trust this key across a mutation).
    /// Two different roots serving the same relative path with the same
    /// size and mtime are two different files, so `root` is mixed in
    /// exactly like `path`: it is part of what identifies "which file",
    /// not a separate cache dimension the way `source_id` is (that one
    /// namespaces distinct provider instances sharing one `BlockCache`).
    fn file_id_for(root: RootId, path: &str, size: u64, mtime: i64) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        h ^= u64::from(root.0);
        h = h.wrapping_mul(0x100_0000_01b3);
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
        // The cache is a read accelerator, not a gate: it has no business
        // refusing a write the inner provider accepts. `capabilities()`
        // already forwards the inner's access level via `.cached()`, so
        // rejecting writes here made that declaration a lie the moment a
        // caller acted on it — exactly the bug class this method now avoids.
        let root = p.root;
        let path = p.rel;
        let st = self.inner.getattr(p)?;
        let (inner, size, is_dir) = self.inner.open(p, flags)?;
        let (file_id, size) = if let Some(s) = st {
            (Self::file_id_for(root, path, s.size, s.mtime), size)
        } else {
            (Self::file_id_for(root, path, size, 0), size)
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

    fn write_at(&self, h: Handle, offset: u64, buf: &[u8]) -> Result<usize, i32> {
        let (inner_h, file_id) = {
            let g = self.opens.lock().map_err(|_| map_io_err())?;
            let r = g.get(&h).ok_or_else(bad_fh)?;
            (r.inner, r.file_id)
        };
        let n = self.inner.write_at(inner_h, offset, buf)?;
        // Drop every block cached under this handle's file identity: serving
        // a stale block after a write is the same bug class as refusing the
        // write outright, just quieter. `BlockCache` only invalidates whole
        // files today (no partial-range primitive), so this is coarser than
        // strictly necessary, but it is correct and it is what exists.
        self.cache.invalidate_file(self.source_id, file_id);
        if n > 0 {
            if let Ok(mut g) = self.opens.lock() {
                if let Some(r) = g.get_mut(&h) {
                    let end = offset + n as u64;
                    if end > r.size {
                        r.size = end;
                    }
                }
            }
        }
        Ok(n)
    }

    fn set_len(&self, h: Handle, len: u64) -> Result<(), i32> {
        let (inner_h, file_id) = {
            let g = self.opens.lock().map_err(|_| map_io_err())?;
            let r = g.get(&h).ok_or_else(bad_fh)?;
            (r.inner, r.file_id)
        };
        self.inner.set_len(inner_h, len)?;
        // Blocks at and beyond the new length are stale (truncated), and a
        // grow zero-fills range that was never cached — drop the whole file
        // rather than reason about which blocks survive a shrink-then-grow.
        self.cache.invalidate_file(self.source_id, file_id);
        if let Ok(mut g) = self.opens.lock() {
            if let Some(r) = g.get_mut(&h) {
                r.size = len;
            }
        }
        Ok(())
    }

    fn flush(&self, h: Handle) -> Result<(), i32> {
        let inner_h = {
            let g = self.opens.lock().map_err(|_| map_io_err())?;
            g.get(&h).ok_or_else(bad_fh)?.inner
        };
        self.inner.flush(inner_h)
    }

    fn mkdir(&self, p: VPath) -> Result<(), i32> {
        self.inner.mkdir(p)
    }

    fn remove(&self, p: VPath) -> Result<(), i32> {
        self.inner.remove(p)
    }

    fn rename(&self, from: VPath, to: VPath) -> Result<(), i32> {
        self.inner.rename(from, to)
    }

    fn set_attr(&self, p: VPath, attr: SetAttr) -> Result<(), i32> {
        self.inner.set_attr(p, attr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::CacheConfig;
    use vfs_provider::{RootId, KIND_FILE, OPEN_READ};

    #[test]
    fn caching_provider_over_the_fixture_tree_passes_conformance() {
        use vfs_provider::conformance::MemFixture;
        let inner: Arc<dyn Provider> = Arc::new(MemFixture::new());
        let cache = Arc::new(BlockCache::new(CacheConfig::default()));
        let p: Arc<dyn Provider> = Arc::new(CachingProvider::new(inner, cache, 1));
        vfs_provider::assert_conformance(p);
    }

    /// The systematic guard for the write-rejection bug this module used to
    /// have: `RwMemFixture` is `Access::ReadWrite`, so this exercises
    /// `assert_writable`'s cases (including a same-handle write-then-read,
    /// which is exactly the shape a stale cached block would corrupt) through
    /// `CachingProvider`. Stage 1 added a cached-provider conformance test,
    /// but its inner provider (`MemFixture`, above) is read-only, so the
    /// write cases never ran and this gap sat open.
    #[test]
    fn caching_provider_over_a_writable_fixture_passes_conformance() {
        let inner: Arc<dyn Provider> = Arc::new(vfs_provider::RwMemFixture::new());
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

    /// Serves the same relative path under two roots with identical size and
    /// mtime but different bytes — exactly what Stage 2b makes possible once
    /// `RootId` is real. Encodes the requested root into the returned handle
    /// so `read_at` can hand back root-specific content without needing any
    /// open-record bookkeeping of its own.
    struct TwoRootProvider;

    impl Provider for TwoRootProvider {
        fn capabilities(&self) -> Capabilities {
            Capabilities::read_only()
        }
        fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
            if p.rel == "f" {
                Ok(Some(Stat { kind: KIND_FILE, size: 4, mtime: 1000 }))
            } else {
                Ok(None)
            }
        }
        fn readdir(&self, _p: VPath) -> Result<Vec<DirEntry>, i32> {
            Ok(vec![])
        }
        fn open(&self, p: VPath, _flags: u32) -> Result<(Handle, u64, bool), i32> {
            if p.rel != "f" {
                return Err(vfs_provider::not_found());
            }
            // Handle doubles as the root id so `read_at` knows which root's
            // bytes to serve without any extra state.
            Ok((u64::from(p.root.0), 4, false))
        }
        fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
            let data: &[u8] = if h == 0 { b"AAAA" } else { b"BBBB" };
            let start = offset as usize;
            if start >= data.len() {
                return Ok(0);
            }
            let n = buf.len().min(data.len() - start);
            buf[..n].copy_from_slice(&data[start..start + n]);
            Ok(n)
        }
        fn close(&self, _h: Handle) -> Result<(), i32> {
            Ok(())
        }
    }

    #[test]
    fn two_roots_same_path_size_and_mtime_do_not_collide() {
        let inner: Arc<dyn Provider> = Arc::new(TwoRootProvider);
        let cache = Arc::new(BlockCache::new(CacheConfig::default()));
        let p = CachingProvider::new(inner, cache, 1);

        let (h0, _, _) = p.open(VPath::new(RootId(0), "f"), OPEN_READ).unwrap();
        let mut buf0 = [0u8; 4];
        assert_eq!(p.read_at(h0, 0, &mut buf0).unwrap(), 4);
        p.close(h0).unwrap();

        let (h1, _, _) = p.open(VPath::new(RootId(1), "f"), OPEN_READ).unwrap();
        let mut buf1 = [0u8; 4];
        assert_eq!(p.read_at(h1, 0, &mut buf1).unwrap(), 4);
        p.close(h1).unwrap();

        assert_eq!(&buf0, b"AAAA", "root 0 should read its own bytes");
        assert_eq!(
            &buf1, b"BBBB",
            "root 1 got root 0's cached bytes back — file_id_for collided across roots"
        );
    }
}

//! [`CachingProvider`]: wraps any [`Provider`] with block-aligned caching.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use vfs_provider::{
    bad_fh, is_dir, map_io_err, Capabilities, DirEntry, Handle, Provider, RootId, SetAttr, Stat,
    VPath,
};

use crate::store::{Block, BlockCache, BlockKey};

struct OpenRec {
    inner: Handle,
    size: u64,
    file_id: u64,
    is_dir: bool,
}

/// Smallest and largest block size a declared `preferred_block` can select.
/// Below 4 KiB a block is smaller than a page and the per-block bookkeeping
/// starts to dominate; above 4 MiB one miss reads more than any plausible
/// request needs and the RAM budget holds too few blocks to be an LRU at all.
const MIN_PREFERRED_BLOCK: u64 = 4 * 1024;
const MAX_PREFERRED_BLOCK: u64 = 4 * 1024 * 1024;

/// Caching facade over an inner provider. `source_id` namespaces cache keys.
pub struct CachingProvider {
    inner: Arc<dyn Provider>,
    cache: Arc<BlockCache>,
    source_id: u64,
    /// This provider's block size. Per-provider rather than per-cache because
    /// `preferred_block` is a property of the source, while one `BlockCache` is
    /// shared by every source in the process. Mixing block sizes in one cache is
    /// sound: `source_id` namespaces the keys, so two providers can never
    /// disagree about what `block_index` means for the same key.
    block_size: u64,
    next: AtomicU64,
    opens: Mutex<HashMap<u64, OpenRec>>,
}

impl CachingProvider {
    pub fn new(inner: Arc<dyn Provider>, cache: Arc<BlockCache>, source_id: u64) -> Self {
        // `Capabilities::preferred_block` is documented as "block-size hint for
        // `cached`" and was, until this point, declared by providers, threaded
        // through `weakest`, propagated by `cached()` — and then ignored by the
        // only component it was addressed to. A source that states its natural
        // unit (a zip's chunk, a remote's frame) now gets it.
        let hinted = inner
            .capabilities()
            .preferred_block
            .map(|b| u64::from(b).clamp(MIN_PREFERRED_BLOCK, MAX_PREFERRED_BLOCK))
            .unwrap_or_else(|| cache.block_size());
        // A block the cache cannot hold is worse than a smaller one that it can.
        // `BlockCache::put` drops any block larger than one shard's budget, so
        // choosing such a size turns the cache off: every read fetches a whole
        // block from the source, the insert is discarded, and the next read does
        // it again. Nothing errors and nothing slows down visibly — the leaf just
        // gets 20 MiB of requests for five 4 KiB reads.
        //
        // The mismatch is structural, not hypothetical: shard geometry comes from
        // `CacheConfig::block_size` while this size comes from the *source's*
        // `preferred_block`, and the two are set by different people. So ask the
        // cache what it can actually hold and stay inside it. Shrinking the block
        // costs read amplification the source would have preferred; exceeding it
        // costs the entire cache.
        let block_size = match cache.max_cacheable_block() {
            // Zero is `ram_budget: 0` — the RAM tier is off by request and the
            // disk tier has no such limit, so there is nothing to clamp to.
            0 => hinted,
            cap => hinted.min(cap),
        };
        Self {
            inner,
            cache,
            source_id,
            block_size,
            next: AtomicU64::new(1),
            opens: Mutex::new(HashMap::new()),
        }
    }

    /// The block size in use, after applying the inner provider's declared
    /// `preferred_block`, the sanity clamp, and the cache's largest cacheable
    /// block.
    pub fn block_size(&self) -> u64 {
        self.block_size
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
        let bs = self.block_size;
        let mut off = offset;
        while off < end {
            let block_index = off / bs;
            let block_off = (off % bs) as usize;
            let key = BlockKey {
                source_id: self.source_id,
                file_id: rec.file_id,
                block_index,
            };
            // On a hit this is a refcount bump: the copy below moves only the
            // bytes this read actually asked for, and it happens with the
            // cache's lock already released. Before, `get` handed back a clone
            // of the whole block, so a 4 KiB read through a 1 MiB block memcpy'd
            // 1 MiB and discarded all but 4 KiB of it.
            let block: Block = if let Some(b) = self.cache.get(&key) {
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
                // One copy on the miss path (`Vec` -> `Arc<[u8]>`), which is the
                // same single copy the old `put(raw.clone())` paid.
                let raw: Block = raw.into();
                self.cache.put(key, Block::clone(&raw));
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
        // Drop every block cached under this handle's file identity, in every
        // tier: serving a stale block after a write is the same bug class as
        // refusing the write outright, just quieter. `BlockCache` only
        // invalidates whole files today (no partial-range primitive), so this is
        // coarser than strictly necessary.
        //
        // The comment that used to stand here said this path "is correct". It was
        // not, and the claim outlived the check: `invalidate_file` built its key
        // list from the RAM map, so a block already evicted to a `.blk` file was
        // not touched, and the read below this write handed back the pre-write
        // bytes. Both tiers are enumerated now — and this asks whether that
        // succeeded instead of assuming it.
        let inv = self.cache.invalidate_file(self.source_id, file_id);
        // The bytes are at the leaf whatever the cache managed to do, so the size
        // bookkeeping is unconditional and happens before any early return.
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
        if !inv.is_complete() {
            // The write landed but the cache may still be able to serve pre-write
            // bytes for this file, and this call is the last moment anyone knows
            // that. Returning `Ok(n)` here is how the corruption becomes
            // invisible; an I/O error on a write is a failure every caller already
            // has a path for. `write_at` is positional, so the honest response —
            // retrying the same bytes at the same offset — is idempotent.
            return Err(map_io_err());
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
        let inv = self.cache.invalidate_file(self.source_id, file_id);
        if let Ok(mut g) = self.opens.lock() {
            if let Some(r) = g.get_mut(&h) {
                r.size = len;
            }
        }
        if !inv.is_complete() {
            // Same trade as `write_at`: the length change is done, but a block the
            // cache could not drop can still be served, so the caller hears about
            // it. `set_len` is idempotent, so a retry is safe.
            return Err(map_io_err());
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
    use vfs_provider::{RootId, KIND_FILE, OPEN_READ, OPEN_WRITE};

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

    /// Declares `preferred_block` and counts the bytes the cache asks it for.
    struct HintingProvider {
        hint: Option<u32>,
        size: u64,
        bytes_requested: AtomicU64,
    }

    impl Provider for HintingProvider {
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                immutable: true,
                slow: true,
                preferred_block: self.hint,
                ..Capabilities::read_only()
            }
        }
        fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
            if p.rel == "f" {
                Ok(Some(Stat {
                    kind: KIND_FILE,
                    size: self.size,
                    mtime: 5,
                }))
            } else {
                Ok(None)
            }
        }
        fn readdir(&self, _: VPath) -> Result<Vec<DirEntry>, i32> {
            Ok(vec![])
        }
        fn open(&self, _p: VPath, _: u32) -> Result<(Handle, u64, bool), i32> {
            Ok((1, self.size, false))
        }
        fn read_at(&self, _: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
            let n = buf.len().min((self.size - offset.min(self.size)) as usize);
            self.bytes_requested.fetch_add(n as u64, Ordering::Relaxed);
            buf[..n].fill(0xC3);
            Ok(n)
        }
        fn close(&self, _: Handle) -> Result<(), i32> {
            Ok(())
        }
    }

    /// `preferred_block` was declared by providers, combined by
    /// `Capabilities::weakest`, propagated by `cached()` — and then ignored by
    /// the one component it was addressed to. It now selects the block size, and
    /// this asserts the *effect* (how much the source is asked for), not just
    /// the getter, because a getter that agrees while the fetch path still uses
    /// the cache-wide size is precisely the kind of test this project distrusts.
    #[test]
    fn a_declared_preferred_block_sizes_the_fetch() {
        let hinted = Arc::new(HintingProvider {
            hint: Some(4096),
            size: 64 * 1024,
            bytes_requested: AtomicU64::new(0),
        });
        // Cache default is 1 MiB; the provider asks for 4 KiB.
        let cache = Arc::new(BlockCache::new(CacheConfig::default()));
        let p = CachingProvider::new(hinted.clone(), cache, 1);
        assert_eq!(p.block_size(), 4096);
        let (h, _, _) = p.open(VPath::at_default("f"), OPEN_READ).unwrap();
        let mut buf = [0u8; 4096];
        assert_eq!(p.read_at(h, 0, &mut buf).unwrap(), 4096);
        assert_eq!(
            hinted.bytes_requested.load(Ordering::Relaxed),
            4096,
            "one 4 KiB read should fetch one 4 KiB block, not a cache-sized one"
        );
        p.close(h).unwrap();
    }

    #[test]
    fn without_a_hint_the_cache_wide_block_size_is_used() {
        let plain = Arc::new(HintingProvider {
            hint: None,
            size: 64 * 1024,
            bytes_requested: AtomicU64::new(0),
        });
        let cache = Arc::new(BlockCache::new(CacheConfig {
            block_size: 16 * 1024,
            ram_budget: 1 << 20,
            disk_dir: None,
        }));
        let p = CachingProvider::new(plain.clone(), cache, 1);
        assert_eq!(p.block_size(), 16 * 1024);
        let (h, _, _) = p.open(VPath::at_default("f"), OPEN_READ).unwrap();
        let mut buf = [0u8; 4096];
        assert_eq!(p.read_at(h, 0, &mut buf).unwrap(), 4096);
        assert_eq!(
            plain.bytes_requested.load(Ordering::Relaxed),
            16 * 1024,
            "with no hint the fetch is one cache-configured block"
        );
        p.close(h).unwrap();
    }

    /// A hint is a hint, not an instruction: an absurd one is clamped rather
    /// than honoured. A 1-byte block would make the per-block bookkeeping the
    /// entire cost; a 1 GiB one would let a single miss read a gigabyte.
    #[test]
    fn an_out_of_range_preferred_block_is_clamped() {
        let mk = |hint| {
            let inner = Arc::new(HintingProvider {
                hint: Some(hint),
                size: 1 << 20,
                bytes_requested: AtomicU64::new(0),
            });
            let cache = Arc::new(BlockCache::new(CacheConfig::default()));
            CachingProvider::new(inner, cache, 1).block_size()
        };
        assert_eq!(mk(1), MIN_PREFERRED_BLOCK, "absurdly small is clamped up");
        assert_eq!(mk(512), MIN_PREFERRED_BLOCK);
        assert_eq!(mk(u32::MAX), MAX_PREFERRED_BLOCK, "absurdly large is clamped down");
        assert_eq!(mk(65536), 65536, "a sane hint is honoured exactly");
    }

    /// **The silently-disabled cache, from outside.** A `preferred_block` bigger
    /// than one shard's budget used to be fetched, refused by every `put`, and
    /// re-fetched on the next read — with no error, no counter and no log line.
    /// The configuration is the measured one: a 4 KiB `block_size` over a 64 MiB
    /// budget gives 64 shards of 1 MiB each, and the source asks for 4 MiB.
    ///
    /// This asserts the effect on the leaf rather than the getter, because the
    /// symptom was never a wrong number in the cache — it was 20 MiB of reads
    /// leaving the process to satisfy 20 KiB of requests.
    #[test]
    fn a_preferred_block_larger_than_the_cache_can_hold_still_caches() {
        let hinted = Arc::new(HintingProvider {
            hint: Some(4 << 20),
            size: 8 << 20,
            bytes_requested: AtomicU64::new(0),
        });
        let cache = Arc::new(BlockCache::new(CacheConfig {
            block_size: 4096,
            ram_budget: 64 * 1024 * 1024,
            disk_dir: None,
        }));
        assert_eq!(
            cache.max_cacheable_block(),
            1 << 20,
            "64 MiB across 64 shards — the premise of this test"
        );
        let p = CachingProvider::new(hinted.clone(), cache.clone(), 1);
        assert!(
            p.block_size() <= cache.max_cacheable_block(),
            "chose a {} byte block for a cache whose largest cacheable block is {}",
            p.block_size(),
            cache.max_cacheable_block()
        );

        let (h, _, _) = p.open(VPath::at_default("f"), OPEN_READ).unwrap();
        let mut buf = [0u8; 4096];
        for i in 0..5u64 {
            assert_eq!(p.read_at(h, i * 4096, &mut buf).unwrap(), 4096);
        }
        let pulled = hinted.bytes_requested.load(Ordering::Relaxed);
        assert!(
            pulled <= p.block_size(),
            "five 4 KiB reads from inside one block pulled {pulled} bytes from the \
             leaf, against a {} byte block. Every `put` was refused for exceeding \
             the shard budget, so the cache held nothing and each read re-fetched a \
             whole block — 20 MiB measured on exactly this configuration.",
            p.block_size()
        );
        let s = cache.stats();
        assert_eq!(
            s.oversized_rejects, 0,
            "the cache refused a block it was handed, so it is not caching"
        );
        assert!(s.hits >= 4, "reads 2 through 5 are inside block 0 and must hit");
        p.close(h).unwrap();
    }

    /// A private directory per test. Two disk-tier tests sharing one would delete
    /// each other's `.blk` files and the failure would read as a cache bug.
    fn disk_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("vfs-cache-p{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn blk_names(dir: &std::path::Path) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut v: Vec<String> = rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".blk"))
            .collect();
        v.sort();
        v
    }

    /// Writable in-memory leaf holding one file, `f`.
    ///
    /// `mtime` is fixed and the tests below overwrite bytes without changing the
    /// length, so `file_id_for` yields the *same* cache identity before and after
    /// the write. That is deliberate: the cache has to invalidate, not be rescued
    /// by its key changing underneath it. It is also the ordinary case — an
    /// in-place same-size write, or any write inside one mtime tick.
    struct RwLeaf {
        body: Mutex<Vec<u8>>,
    }

    impl RwLeaf {
        fn new(len: usize, fill: u8) -> Arc<Self> {
            Arc::new(RwLeaf {
                body: Mutex::new(vec![fill; len]),
            })
        }
        fn head(&self, n: usize) -> Vec<u8> {
            self.body.lock().unwrap()[..n].to_vec()
        }
    }

    impl Provider for RwLeaf {
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                access: vfs_provider::Access::ReadWrite,
                immutable: false,
                slow: true,
                preferred_block: None,
            }
        }
        fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
            if p.rel != "f" {
                return Ok(None);
            }
            Ok(Some(Stat {
                kind: KIND_FILE,
                size: self.body.lock().unwrap().len() as u64,
                mtime: 11,
            }))
        }
        fn readdir(&self, _: VPath) -> Result<Vec<DirEntry>, i32> {
            Ok(vec![])
        }
        fn open(&self, p: VPath, _: u32) -> Result<(Handle, u64, bool), i32> {
            if p.rel != "f" {
                return Err(vfs_provider::not_found());
            }
            Ok((1, self.body.lock().unwrap().len() as u64, false))
        }
        fn read_at(&self, _: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
            let body = self.body.lock().unwrap();
            let start = (offset as usize).min(body.len());
            let n = buf.len().min(body.len() - start);
            buf[..n].copy_from_slice(&body[start..start + n]);
            Ok(n)
        }
        fn write_at(&self, _: Handle, offset: u64, buf: &[u8]) -> Result<usize, i32> {
            let mut body = self.body.lock().unwrap();
            let start = offset as usize;
            if start + buf.len() > body.len() {
                body.resize(start + buf.len(), 0);
            }
            body[start..start + buf.len()].copy_from_slice(buf);
            Ok(buf.len())
        }
        fn close(&self, _: Handle) -> Result<(), i32> {
            Ok(())
        }
    }

    /// Populate the cache by reading the whole file through `p`, and return what
    /// came back. Leaves the early blocks evicted from RAM but still present on
    /// disk, which is the state the two tests below are about.
    fn read_whole(p: &CachingProvider, h: Handle, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        assert_eq!(p.read_at(h, 0, &mut out).unwrap(), len);
        out
    }

    /// **The corruption, end to end through `CachingProvider`.**
    ///
    /// A block that eviction moved out of RAM is still servable from its `.blk`
    /// file. Invalidation used to build its key list from the RAM map, which by
    /// definition no longer contains that key, so the `.blk` survived a write and
    /// `get` re-served it: `write_at` returned success and the very next read of
    /// the same range through the same handle handed back the pre-write bytes.
    ///
    /// Nothing here is contrived. The block size is small so the test is fast; the
    /// budget holds two blocks so eviction is deterministic; the write is a
    /// same-size overwrite, which is the common case and keeps the file identity
    /// stable so the stale key is the one looked up.
    #[test]
    fn a_write_invalidates_a_block_eviction_pushed_to_the_disk_tier() {
        const LEN: usize = 640;
        let dir = disk_dir("write-invalidation");
        let leaf = RwLeaf::new(LEN, b'A');
        let cache = Arc::new(BlockCache::new(CacheConfig {
            block_size: 64,
            ram_budget: 128, // exactly two blocks
            disk_dir: Some(dir.clone()),
        }));
        let p = CachingProvider::new(leaf.clone(), cache.clone(), 1);
        let (h, size, _) = p
            .open(VPath::at_default("f"), OPEN_READ | OPEN_WRITE)
            .unwrap();
        assert_eq!(size, LEN as u64);
        assert_eq!(p.block_size(), 64);

        // Read the file through: all ten blocks are written to the disk tier and
        // all but the last two are evicted from RAM.
        assert_eq!(read_whole(&p, h, LEN), vec![b'A'; LEN]);
        assert!(
            cache.stats().ram_evicts > 0,
            "the fixture never evicted, so this test is not about eviction"
        );
        assert_eq!(
            blk_names(&dir).len(),
            10,
            "the disk tier should hold every block: {:?}",
            blk_names(&dir)
        );

        assert_eq!(p.write_at(h, 0, &[b'B'; 64]).unwrap(), 64);
        assert_eq!(
            blk_names(&dir),
            Vec::<String>::new(),
            "a write left .blk files behind for the file it wrote: {:?}. \
             Invalidation enumerated the RAM map, so blocks eviction had already \
             moved to disk were never named.",
            blk_names(&dir)
        );

        let mut after = [0u8; 64];
        assert_eq!(p.read_at(h, 0, &mut after).unwrap(), 64);
        // Guard the claim before making it: the leaf really did take the write, so
        // a stale read here is the cache's doing and not a lost write.
        assert_eq!(leaf.head(64), vec![b'B'; 64], "the leaf never took the write");
        assert_eq!(
            &after[..],
            &[b'B'; 64][..],
            "read-after-write returned pre-write bytes: block 0 had been evicted \
             to disk, invalidation missed it, and `get` re-served the .blk file"
        );
        p.close(h).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The adjacent failure mode: a write that lands while invalidation only
    /// partly succeeds. The caller has to learn, because this call is the last
    /// moment anyone knows the cache may still answer with pre-write bytes.
    ///
    /// A directory standing where a `.blk` file belongs is unremovable by
    /// `remove_file` — a faithful stand-in for a locked file or a revoked ACL
    /// without needing either. It also does not require knowing the file identity
    /// the cache hashed: the `.blk` file is picked out of the directory listing.
    #[test]
    fn a_write_whose_invalidation_cannot_finish_reports_an_error() {
        const LEN: usize = 640;
        let dir = disk_dir("write-invalidation-fails");
        let leaf = RwLeaf::new(LEN, b'A');
        let cache = Arc::new(BlockCache::new(CacheConfig {
            block_size: 64,
            ram_budget: 128,
            disk_dir: Some(dir.clone()),
        }));
        let p = CachingProvider::new(leaf.clone(), cache.clone(), 1);
        let (h, _, _) = p
            .open(VPath::at_default("f"), OPEN_READ | OPEN_WRITE)
            .unwrap();
        read_whole(&p, h, LEN);
        let victim = blk_names(&dir)
            .into_iter()
            .next()
            .expect("the disk tier should be populated");
        std::fs::remove_file(dir.join(&victim)).unwrap();
        std::fs::create_dir(dir.join(&victim)).unwrap();

        let err = p.write_at(h, 0, &[b'B'; 64]).unwrap_err();
        assert_eq!(
            err,
            map_io_err(),
            "a write whose invalidation failed reported success"
        );
        assert!(
            cache.stats().invalidate_failures >= 1,
            "the failure is not visible in the stats either"
        );
        // The bytes did land. The error is about the cache, not the write, which is
        // why the caller's correct response — repeating the same positional write —
        // is idempotent.
        assert_eq!(leaf.head(64), vec![b'B'; 64]);
        p.close(h).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
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

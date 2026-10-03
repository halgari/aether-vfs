//! Compressing new blocks by write class, and counting what writes stored.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;

use crate::class::WriteClass;
use crate::codec::{EncodedBlock, Hash128, encode_block};
use crate::config::{BulkCompression, StoreConfig};
use crate::stats::{ClassWriteStats, WriteStats};

#[cfg(feature = "gpu-zstd")]
use crate::gpu::{EngineFactory, GpuEncoder};

enum Bulk {
    Zstd(i32),
    #[cfg(feature = "gpu-zstd")]
    Gpu(GpuEncoder),
}

#[derive(Default)]
struct Counters {
    logical: AtomicU64,
    new_blocks: AtomicU64,
    new_raw: AtomicU64,
    stored: AtomicU64,
    uncompressed: AtomicU64,
}

impl Counters {
    fn get(&self) -> ClassWriteStats {
        ClassWriteStats {
            logical_bytes: self.logical.load(Ordering::Relaxed),
            new_blocks: self.new_blocks.load(Ordering::Relaxed),
            new_raw_bytes: self.new_raw.load(Ordering::Relaxed),
            stored_bytes: self.stored.load(Ordering::Relaxed),
            uncompressed_blocks: self.uncompressed.load(Ordering::Relaxed),
        }
    }
}

/// The store's compressors: CPU zstd for foreground writes, [`StoreConfig::bulk`] for bulk
/// writes.
pub(crate) struct Codec {
    fg_level: i32,
    bulk: Bulk,
    counters: [Counters; 2],
}

fn slot(class: WriteClass) -> usize {
    match class {
        WriteClass::Foreground => 0,
        WriteClass::Bulk => 1,
    }
}

impl Codec {
    pub(crate) fn new(cfg: &StoreConfig) -> Codec {
        #[cfg(feature = "gpu-zstd")]
        return Codec::with_factory(cfg, crate::gpu::gzc_factory());
        #[cfg(not(feature = "gpu-zstd"))]
        Codec::build(cfg)
    }

    #[cfg(not(feature = "gpu-zstd"))]
    fn build(cfg: &StoreConfig) -> Codec {
        let bulk = match cfg.bulk {
            BulkCompression::Foreground => Bulk::Zstd(cfg.zstd_level),
            BulkCompression::Zstd(l) => Bulk::Zstd(l),
        };
        Codec {
            fg_level: cfg.zstd_level,
            bulk,
            counters: Default::default(),
        }
    }

    /// As [`Codec::new`], with the GPU opened by `factory` (tests inject a stand-in).
    #[cfg(feature = "gpu-zstd")]
    pub(crate) fn with_factory(cfg: &StoreConfig, factory: EngineFactory) -> Codec {
        let bulk = match &cfg.bulk {
            BulkCompression::Foreground => Bulk::Zstd(cfg.zstd_level),
            BulkCompression::Zstd(l) => Bulk::Zstd(*l),
            BulkCompression::Gpu(g) => Bulk::Gpu(GpuEncoder::new(g.clone(), factory)),
        };
        Codec {
            fg_level: cfg.zstd_level,
            bulk,
            counters: Default::default(),
        }
    }

    /// What compresses `class` writes now: `zstd:6`, `GPU opt16p1`, or
    /// `GPU opt16p1 (off: zstd:6)` once the GPU was turned off.
    pub(crate) fn label(&self, class: WriteClass) -> String {
        match (class, &self.bulk) {
            (WriteClass::Foreground, _) => format!("zstd:{}", self.fg_level),
            (WriteClass::Bulk, Bulk::Zstd(l)) => format!("zstd:{l}"),
            #[cfg(feature = "gpu-zstd")]
            (WriteClass::Bulk, Bulk::Gpu(g)) => match g.failed() {
                None => format!("GPU {}", g.level().name()),
                Some(_) => format!("GPU {} (off: zstd:{})", g.level().name(), self.fg_level),
            },
        }
    }

    /// Compresses `blocks` (with their `hashes`) for a `class` write. CPU work runs in `pool`
    /// (the store's), or rayon's global pool.
    pub(crate) fn encode(
        &self,
        pool: Option<&rayon::ThreadPool>,
        blocks: &[&[u8]],
        hashes: &[Hash128],
        class: WriteClass,
    ) -> io::Result<Vec<EncodedBlock>> {
        let cpu = |level: i32| -> io::Result<Vec<EncodedBlock>> {
            install(pool, || {
                blocks
                    .par_iter()
                    .zip(hashes)
                    .map(|(b, h)| encode_block(b, *h, level))
                    .collect()
            })
        };
        let encoded = match (class, &self.bulk) {
            (WriteClass::Foreground, _) => cpu(self.fg_level)?,
            (WriteClass::Bulk, Bulk::Zstd(l)) => cpu(*l)?,
            #[cfg(feature = "gpu-zstd")]
            (WriteClass::Bulk, Bulk::Gpu(g)) => self.encode_gpu(g, pool, blocks, hashes)?,
        };
        let c = &self.counters[slot(class)];
        c.new_blocks
            .fetch_add(encoded.len() as u64, Ordering::Relaxed);
        for e in &encoded {
            c.new_raw
                .fetch_add(e.header.raw_len as u64, Ordering::Relaxed);
            c.stored.fetch_add(e.header.record_len(), Ordering::Relaxed);
            if e.header.flags & crate::codec::FLAG_COMPRESSED == 0 {
                c.uncompressed.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(encoded)
    }

    #[cfg(feature = "gpu-zstd")]
    fn encode_gpu(
        &self,
        g: &GpuEncoder,
        pool: Option<&rayon::ThreadPool>,
        blocks: &[&[u8]],
        hashes: &[Hash128],
    ) -> io::Result<Vec<EncodedBlock>> {
        let frames = g.compress(blocks);
        let verify = g.verify();
        let level = self.fg_level;
        install(pool, || {
            frames
                .into_par_iter()
                .zip(blocks)
                .zip(hashes)
                .map(|((f, b), h)| match f {
                    Some(f) if !verify || decodes_to(&f, b) => {
                        Ok(crate::codec::from_frame(b, *h, f))
                    }
                    Some(_) => {
                        g.disable("a GPU frame did not decode to its block".into());
                        encode_block(b, *h, level)
                    }
                    None => encode_block(b, *h, level),
                })
                .collect()
        })
    }

    /// Counts `bytes` of a `class` write as written (new or deduplicated).
    pub(crate) fn wrote(&self, class: WriteClass, bytes: u64) {
        self.counters[slot(class)]
            .logical
            .fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn stats(&self) -> WriteStats {
        WriteStats {
            foreground: self.counters[0].get(),
            bulk: self.counters[1].get(),
            index: Default::default(),
            #[cfg(feature = "gpu-zstd")]
            gpu: match &self.bulk {
                Bulk::Gpu(g) => Some(g.stats()),
                Bulk::Zstd(_) => None,
            },
        }
    }

    /// Compresses what the GPU has queued and stops it. Bulk writes after this use the CPU.
    pub(crate) fn shutdown(&self) {
        #[cfg(feature = "gpu-zstd")]
        if let Bulk::Gpu(g) = &self.bulk {
            g.shutdown();
        }
    }
}

#[cfg(feature = "gpu-zstd")]
fn decodes_to(frame: &[u8], block: &[u8]) -> bool {
    zstd::bulk::decompress(frame, block.len()).is_ok_and(|d| d == block)
}

fn install<R: Send>(pool: Option<&rayon::ThreadPool>, f: impl FnOnce() -> R + Send) -> R {
    match pool {
        Some(p) => p.install(f),
        None => f(),
    }
}

#[cfg(test)]
mod tests {
    use crate::class::{WriteClass, with_write_class};
    use crate::config::{BulkCompression, StoreConfig};
    use crate::store::BlockStore;
    use crate::store::tests::{BS, random_bytes, read_all, test_config};

    /// Text-like bytes: compress well at any level.
    fn texty(seed: u64, len: usize) -> Vec<u8> {
        let words = [
            &b"iron "[..],
            b"sword ",
            b"of ",
            b"the ",
            b"dragonborn ",
            b"whiterun ",
        ];
        let mut out = Vec::with_capacity(len);
        let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        while out.len() < len {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            out.extend_from_slice(words[(x % words.len() as u64) as usize]);
        }
        out.truncate(len);
        out
    }

    #[test]
    fn write_stats_count_per_class_and_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = StoreConfig {
            bulk: BulkCompression::Zstd(1),
            ..test_config()
        };
        let store = BlockStore::open(dir.path(), cfg).unwrap();
        assert_eq!(store.compression(WriteClass::Foreground), "zstd:6");
        assert_eq!(store.compression(WriteClass::Bulk), "zstd:1");
        let a = texty(1, 3 * BS + 100);
        let r = random_bytes(2, 2 * BS);
        store.set_len(b"a", a.len() as u64).unwrap();
        store.set_len(b"r", r.len() as u64).unwrap();
        store.set_len(b"a2", a.len() as u64).unwrap();
        with_write_class(WriteClass::Bulk, || store.write_blocks(b"a", 0, &a)).unwrap();
        store.write_blocks(b"r", 0, &r).unwrap();
        // Same content again, explicitly bulk: all deduplicated.
        store
            .write_blocks_as(b"a2", 0, &a, WriteClass::Bulk)
            .unwrap();
        let st = store.write_stats();
        let b = st.bulk;
        assert_eq!(b.logical_bytes, 2 * a.len() as u64);
        assert_eq!(b.new_blocks, 4);
        assert_eq!(b.new_raw_bytes, a.len() as u64);
        assert_eq!(b.dedup_bytes(), a.len() as u64);
        assert!(b.stored_bytes < b.new_raw_bytes);
        assert_eq!(b.uncompressed_blocks, 0);
        let f = st.foreground;
        assert_eq!(f.logical_bytes, r.len() as u64);
        assert_eq!(f.new_blocks, 2);
        assert_eq!(f.uncompressed_blocks, 2);
        assert_eq!(
            f.stored_bytes,
            r.len() as u64 + 2 * crate::codec::HEADER_LEN as u64
        );
        let both = f.plus(&b);
        assert_eq!(both.new_blocks, 6);
        assert_eq!(both.since(&f), b);
        assert_eq!(read_all(&store, b"a"), a);
        assert_eq!(read_all(&store, b"a2"), a);
        assert_eq!(read_all(&store, b"r"), r);
    }

    #[test]
    fn usage_is_summed_per_class_of_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlockStore::open(dir.path(), test_config()).unwrap();
        let a = texty(3, 4 * BS);
        let r = random_bytes(4, BS + 10);
        for (id, d) in [(&b"Ca"[..], &a), (b"Cb", &a), (b"Lr", &r)] {
            store.set_len(id, d.len() as u64).unwrap();
            store.write_blocks(id, 0, d).unwrap();
        }
        // A file with nothing stored yet.
        store.set_len(b"Lempty", 5 * BS as u64).unwrap();
        let u = store.usage_by(|id| Some(id[0])).unwrap();
        let c = u[&b'C'];
        assert_eq!(c.files, 2);
        assert_eq!(c.logical_bytes, 2 * a.len() as u64);
        // Ca and Cb share their blocks.
        let distinct = {
            let mut v: Vec<&[u8]> = a.chunks(BS).collect();
            v.sort();
            v.dedup();
            v.len() as u64
        };
        assert_eq!(c.blocks, distinct);
        assert!(c.stored_bytes < a.len() as u64);
        let l = u[&b'L'];
        assert_eq!(l.files, 2);
        assert_eq!(l.logical_bytes, r.len() as u64);
        assert_eq!(l.blocks, 2);
        assert_eq!(
            l.stored_bytes,
            r.len() as u64 + 2 * crate::codec::HEADER_LEN as u64
        );
        let only_c = store.usage_by(|id| (id[0] == b'C').then_some(())).unwrap();
        assert_eq!(only_c[&()], c);
    }
}

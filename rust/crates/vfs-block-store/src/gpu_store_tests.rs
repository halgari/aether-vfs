//! The store with GPU bulk compression: through a stand-in engine (no GPU needed), and through
//! the real one (`#[ignore]`d: run with `--features gpu-zstd -- --ignored` on a machine with a
//! GPU).

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use crate::class::{with_write_class, WriteClass};
use crate::codec::{RecordHeader, FLAG_COMPRESSED, HEADER_LEN};
use crate::config::{BulkCompression, StoreConfig};
use crate::gpu::tests::{fake_factory, test_cfg, Fake};
use crate::gpu::{GpuConfig, GpuLevel};
use crate::store::tests::{random_bytes, read_all};
use crate::store::BlockStore;

const BS: usize = 64 * 1024;

fn gpu_config(g: GpuConfig) -> StoreConfig {
    StoreConfig {
        block_size: BS as u32,
        max_pack_size: 64 << 20,
        index_cache_bytes: 4 << 20,
        bulk: BulkCompression::Gpu(g),
        ..StoreConfig::default()
    }
}

/// DDS-like bytes: a header, then 8-byte BC1-ish blocks of slowly varying colour endpoints and
/// noisy index bits. Compresses a little, as real textures do.
pub(crate) fn dds_like(seed: u64, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    out.extend_from_slice(b"DDS |\0\0\0\x07\x10\x08\0");
    out.resize(128.min(len), 0);
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut c0: u16 = (seed as u16) | 0x0821;
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        if x.is_multiple_of(5) {
            c0 = c0.wrapping_add((x >> 20) as u16 & 0x0841);
        }
        let c1 = c0.wrapping_sub(0x0421);
        let idx = if x.is_multiple_of(3) {
            0x5555_5555u32
        } else {
            (x >> 32) as u32
        };
        let mut b = [0u8; 8];
        b[0..2].copy_from_slice(&c0.to_le_bytes());
        b[2..4].copy_from_slice(&c1.to_le_bytes());
        b[4..8].copy_from_slice(&idx.to_le_bytes());
        out.extend_from_slice(&b);
    }
    out.truncate(len);
    out
}

/// A file of every kind of block: DDS-like, text-like, zeros, random, and a short last block.
fn mixed(seed: u64) -> Vec<u8> {
    let mut v = dds_like(seed, 3 * BS);
    v.extend(
        std::iter::repeat_n(
            &b"Dragonborn, dragonborn, by his honor is sworn. "[..],
            1500,
        )
        .flatten()
        .take(BS),
    );
    v.extend(std::iter::repeat_n(0u8, BS));
    v.extend(random_bytes(seed, 2 * BS));
    v.extend(
        std::iter::repeat_n(&b"Fus Ro Dah! "[..], 200)
            .flatten()
            .take(1234),
    );
    v
}

fn put_bulk(store: &BlockStore, id: &[u8], data: &[u8]) {
    store.set_len(id, data.len() as u64).unwrap();
    with_write_class(WriteClass::Bulk, || store.write_blocks(id, 0, data)).unwrap();
}

/// Every block of `id`'s records: (flags, raw_len, stored_len).
fn records(store: &BlockStore, id: &[u8]) -> Vec<(u8, u32, u32)> {
    let r = store.index.read().unwrap();
    let seg = r.segment(id, 0).unwrap().unwrap();
    let ids = crate::manifest::decode_ids(&seg, 0);
    ids.iter()
        .map(|&b| {
            let loc = r.block(b).unwrap().unwrap();
            use std::io::{Read, Seek, SeekFrom};
            let mut h = [0u8; HEADER_LEN];
            let mut f =
                std::fs::File::open(crate::pack::pack_path(&store.pack_dir, loc.pack)).unwrap();
            f.seek(SeekFrom::Start(loc.offset)).unwrap();
            f.read_exact(&mut h).unwrap();
            let h = RecordHeader::decode(&h).unwrap();
            (h.flags, h.raw_len, h.stored_len)
        })
        .collect()
}

fn check_mixed_records(store: &BlockStore, id: &[u8]) {
    let recs = records(store, id);
    assert_eq!(recs.len(), 8);
    // Text and zeros compress; random blocks are stored raw; DDS-like ones compress.
    for (i, (flags, raw, stored)) in recs.iter().enumerate() {
        match i {
            5 | 6 => assert_eq!(
                (*flags, raw, stored),
                (0, &(BS as u32), &(BS as u32)),
                "block {i}"
            ),
            _ => assert_eq!(*flags, FLAG_COMPRESSED, "block {i}"),
        }
    }
    assert_eq!(recs[7].1, 1234);
}

#[test]
fn bulk_writes_go_through_the_engine_and_read_back() {
    let dir = vfs_testkit::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let store = BlockStore::open_with_engine(
        dir.path(),
        gpu_config(test_cfg()),
        fake_factory(fake.clone()),
    )
    .unwrap();
    assert_eq!(store.compression(WriteClass::Bulk), "GPU opt16p1");
    assert_eq!(store.compression(WriteClass::Foreground), "zstd:6");
    let d = mixed(1);
    put_bulk(&store, b"m", &d);
    assert_eq!(read_all(&store, b"m"), d);
    check_mixed_records(&store, b"m");
    assert_eq!(fake.blocks.load(Ordering::SeqCst), 8);
    // Foreground writes never reach the engine.
    let f = mixed(2);
    store.set_len(b"f", f.len() as u64).unwrap();
    store.write_blocks(b"f", 0, &f).unwrap();
    assert_eq!(fake.blocks.load(Ordering::SeqCst), 8);
    let st = store.write_stats();
    assert_eq!(st.bulk.new_blocks, 8);
    assert_eq!(st.bulk.uncompressed_blocks, 2);
    // Its text, zeros and short block are deduplicated against "m".
    assert_eq!(st.foreground.new_blocks, 5);
    let g = st.gpu.unwrap();
    assert_eq!((g.blocks, g.cpu_fallback_blocks), (8, 0));
    assert!(store.verify().unwrap().is_ok());
    store.close().unwrap();
    let store = BlockStore::open(
        dir.path(),
        StoreConfig {
            bulk: BulkCompression::Foreground,
            ..gpu_config(test_cfg())
        },
    )
    .unwrap();
    assert_eq!(read_all(&store, b"m"), d);
    assert_eq!(read_all(&store, b"f"), f);
}

#[test]
fn concurrent_bulk_writers_share_batches() {
    let dir = vfs_testkit::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let cfg = GpuConfig {
        batch_deadline: Duration::from_millis(50),
        ..test_cfg()
    };
    let store =
        BlockStore::open_with_engine(dir.path(), gpu_config(cfg), fake_factory(fake.clone()))
            .unwrap();
    let files: Vec<Vec<u8>> = (0..16).map(|i| dds_like(100 + i, 4 * BS)).collect();
    for (i, d) in files.iter().enumerate() {
        store
            .set_len(format!("f{i}").as_bytes(), d.len() as u64)
            .unwrap();
    }
    let start = std::sync::Barrier::new(files.len());
    std::thread::scope(|s| {
        for (i, d) in files.iter().enumerate() {
            let (store, start) = (&store, &start);
            s.spawn(move || {
                start.wait();
                with_write_class(WriteClass::Bulk, || {
                    store.write_blocks(format!("f{i}").as_bytes(), 0, d)
                })
                .unwrap()
            });
        }
    });
    // 64 blocks from 16 writers; the stand-in's batch holds 64.
    let batches = fake.batches.load(Ordering::SeqCst);
    assert!(batches <= 4, "{batches} batches for 16 writers");
    assert_eq!(fake.blocks.load(Ordering::SeqCst), 64);
    for (i, d) in files.iter().enumerate() {
        assert_eq!(&read_all(&store, format!("f{i}").as_bytes()), d);
    }
}

#[test]
fn an_unavailable_gpu_falls_back_to_cpu_zstd() {
    let dir = vfs_testkit::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    fake.fail_open.store(true, Ordering::SeqCst);
    let store = BlockStore::open_with_engine(
        dir.path(),
        gpu_config(test_cfg()),
        fake_factory(fake.clone()),
    )
    .unwrap();
    let d = mixed(3);
    put_bulk(&store, b"m", &d);
    put_bulk(&store, b"n", &mixed(4));
    assert_eq!(read_all(&store, b"m"), d);
    check_mixed_records(&store, b"m");
    assert_eq!(
        store.compression(WriteClass::Bulk),
        "GPU opt16p1 (off: zstd:6)"
    );
    let g = store.write_stats().gpu.unwrap();
    assert_eq!(g.blocks, 0);
    // "n" shares its text, zeros and short block with "m".
    assert_eq!(g.cpu_fallback_blocks, 13);
    assert!(g.failed.unwrap().contains("no adapter"));
    assert!(store.verify().unwrap().is_ok());
}

#[test]
fn a_batch_failure_mid_write_loses_nothing() {
    let dir = vfs_testkit::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    fake.fail_batch.store(2, Ordering::SeqCst);
    let store = BlockStore::open_with_engine(
        dir.path(),
        gpu_config(test_cfg()),
        fake_factory(fake.clone()),
    )
    .unwrap();
    // 200 blocks: the stand-in's first batch of 64 succeeds, the second fails.
    let d = dds_like(7, 200 * BS);
    put_bulk(&store, b"big", &d);
    assert_eq!(read_all(&store, b"big"), d);
    let g = store.write_stats().gpu.unwrap();
    assert_eq!(g.blocks, 64);
    assert_eq!(g.cpu_fallback_blocks, 136);
    assert!(store.verify().unwrap().is_ok());
}

#[test]
fn close_stops_the_gpu_service_and_later_opens_read_everything() {
    let dir = vfs_testkit::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let d = mixed(5);
    {
        let store = BlockStore::open_with_engine(
            dir.path(),
            gpu_config(test_cfg()),
            fake_factory(fake.clone()),
        )
        .unwrap();
        put_bulk(&store, b"m", &d);
        store.close().unwrap();
    }
    // Without the GPU setting at all: the frames are plain zstd.
    let store = BlockStore::open(
        dir.path(),
        StoreConfig {
            bulk: BulkCompression::Foreground,
            ..gpu_config(test_cfg())
        },
    )
    .unwrap();
    assert_eq!(read_all(&store, b"m"), d);
    assert!(store.verify().unwrap().is_ok());
}

// ---- The real GPU. ----

fn real(level: GpuLevel) -> StoreConfig {
    gpu_config(GpuConfig {
        level,
        vram_budget_mib: 2048,
        ..GpuConfig::default()
    })
}

#[test]
#[ignore = "needs a GPU"]
fn real_gpu_frames_decode_through_the_read_path() {
    for level in GpuLevel::ALL {
        let dir = vfs_testkit::tempdir().unwrap();
        let store = BlockStore::open(dir.path(), real(level)).unwrap();
        let d = mixed(11);
        put_bulk(&store, b"m", &d);
        let g = store.write_stats().gpu.unwrap();
        assert_eq!(g.failed, None, "{level:?}");
        assert_eq!(g.blocks, 8, "{level:?}");
        assert_eq!(read_all(&store, b"m"), d, "{level:?}");
        check_mixed_records(&store, b"m");
        // Every compressed record is a frame stock libzstd decodes to the block.
        assert!(store.verify().unwrap().is_ok());
        store.close().unwrap();
        let store = BlockStore::open(
            dir.path(),
            StoreConfig {
                block_size: BS as u32,
                ..StoreConfig::default()
            },
        )
        .unwrap();
        assert_eq!(read_all(&store, b"m"), d, "{level:?} reopened on the CPU");
    }
}

#[test]
#[ignore = "needs a GPU"]
fn real_gpu_batches_many_writers_together() {
    let dir = vfs_testkit::tempdir().unwrap();
    let store = BlockStore::open(dir.path(), real(GpuLevel::Lvl9s12seg)).unwrap();
    let files: Vec<Vec<u8>> = (0..32).map(|i| dds_like(500 + i, 64 * BS + 77)).collect();
    for (i, d) in files.iter().enumerate() {
        store
            .set_len(format!("f{i}").as_bytes(), d.len() as u64)
            .unwrap();
    }
    std::thread::scope(|s| {
        for (i, d) in files.iter().enumerate() {
            let store = &store;
            s.spawn(move || {
                with_write_class(WriteClass::Bulk, || {
                    store.write_blocks(format!("f{i}").as_bytes(), 0, d)
                })
                .unwrap()
            });
        }
    });
    let g = store.write_stats().gpu.unwrap();
    assert_eq!(g.failed, None);
    assert_eq!(g.blocks, 32 * 65);
    // 2,080 blocks from 32 writers in a handful of GPU submissions, not one per write.
    assert!(g.batches <= 12, "{} batches", g.batches);
    eprintln!(
        "32 writers, {} blocks, {} GPU batches ({})",
        g.blocks,
        g.batches,
        g.adapter.unwrap()
    );
    for (i, d) in files.iter().enumerate() {
        assert_eq!(&read_all(&store, format!("f{i}").as_bytes()), d);
    }
    assert!(store.verify().unwrap().is_ok());
}

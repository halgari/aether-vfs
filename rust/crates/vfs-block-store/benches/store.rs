//! Benchmarks. `BLOCK_STORE_BENCH_FILES` sets the file count for the stat scan (default 500000).

use std::hint::black_box;
use std::time::Duration;

use block_store::{BlockStore, CompactOptions, StoreConfig};
use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};

const BS: usize = 64 * 1024;

fn random_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    blake3::Hasher::new()
        .update(&seed.to_le_bytes())
        .finalize_xof()
        .fill(&mut out);
    out
}

/// Roughly 3:1 compressible data.
fn compressible_bytes(seed: u64, len: usize) -> Vec<u8> {
    let noise = random_bytes(seed, len / 4);
    (0..len)
        .map(|i| {
            if i % 4 == 0 {
                noise[i / 4]
            } else {
                (i / 256) as u8
            }
        })
        .collect()
}

fn open(dir: &std::path::Path) -> BlockStore {
    BlockStore::open(
        dir,
        StoreConfig {
            max_pack_size: 256 << 20,
            ..StoreConfig::default()
        },
    )
    .unwrap()
}

fn stat_scan(c: &mut Criterion) {
    let files: u64 = std::env::var("BLOCK_STORE_BENCH_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500_000);
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    for i in 0..files {
        store
            .set_len(
                format!("mods/some-game/file-{i:08}.dat").as_bytes(),
                12345 + i,
            )
            .unwrap();
    }
    store.flush().unwrap();
    let mut g = c.benchmark_group("stat");
    g.sample_size(10).measurement_time(Duration::from_secs(20));
    g.throughput(Throughput::Elements(files));
    g.bench_function(format!("scan_{files}_files"), |b| {
        b.iter(|| {
            for i in 0..files {
                black_box(
                    store
                        .stat(format!("mods/some-game/file-{i:08}.dat").as_bytes())
                        .unwrap(),
                );
            }
        })
    });
    g.finish();
}

fn reads(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let blocks = 4096u64; // 256 MiB
    store.set_len(b"big", blocks * BS as u64).unwrap();
    for chunk in 0..blocks / 64 {
        store
            .write_blocks(b"big", chunk * 64, &compressible_bytes(chunk, 64 * BS))
            .unwrap();
    }
    store.flush().unwrap();
    let mut buf = vec![0u8; BS];
    let mut g = c.benchmark_group("read");
    g.throughput(Throughput::Bytes(BS as u64));
    let mut n = 0u64;
    g.bench_function("warm_random_64k_block", |b| {
        b.iter(|| {
            n = n
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let block = (n >> 33) % blocks;
            black_box(store.read(b"big", block * BS as u64, &mut buf).unwrap());
        })
    });
    g.finish();
}

fn writes(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let size = 16 * BS * 16; // 16 MiB
    let mut g = c.benchmark_group("write");
    g.sample_size(10).throughput(Throughput::Bytes(size as u64));
    let mut seed = 0u64;
    g.bench_function("16mib_compressible_new", |b| {
        b.iter_batched(
            || {
                seed += 1;
                (format!("w{seed}"), compressible_bytes(seed, size))
            },
            |(id, data)| {
                store.set_len(id.as_bytes(), data.len() as u64).unwrap();
                store.write_blocks(id.as_bytes(), 0, &data).unwrap();
            },
            BatchSize::LargeInput,
        )
    });
    let same = compressible_bytes(u64::MAX, size);
    store.set_len(b"dedup-src", size as u64).unwrap();
    store.write_blocks(b"dedup-src", 0, &same).unwrap();
    g.bench_function("16mib_all_dedup_hits", |b| {
        b.iter(|| {
            seed += 1;
            let id = format!("d{seed}");
            store.set_len(id.as_bytes(), size as u64).unwrap();
            store.write_blocks(id.as_bytes(), 0, &same).unwrap();
        })
    });
    g.finish();
}

fn compaction(c: &mut Criterion) {
    let mut g = c.benchmark_group("compact");
    g.sample_size(10);
    g.bench_function("256mib_half_garbage", |b| {
        b.iter_batched(
            || {
                let dir = tempfile::tempdir().unwrap();
                let store = BlockStore::open(
                    dir.path(),
                    StoreConfig {
                        max_pack_size: 32 << 20,
                        ..StoreConfig::default()
                    },
                )
                .unwrap();
                for i in 0..64u64 {
                    let id = format!("f{i}");
                    store.set_len(id.as_bytes(), 64 * BS as u64).unwrap();
                    store
                        .write_blocks(id.as_bytes(), 0, &compressible_bytes(i, 64 * BS))
                        .unwrap();
                }
                for i in (0..64u64).step_by(2) {
                    store.delete(format!("f{i}").as_bytes()).unwrap();
                }
                store.flush().unwrap();
                (dir, store)
            },
            |(dir, store)| {
                black_box(
                    store
                        .compact(CompactOptions {
                            min_garbage_ratio: 0.3,
                            max_bytes: u64::MAX,
                        })
                        .unwrap(),
                );
                drop(store);
                drop(dir);
            },
            BatchSize::PerIteration,
        )
    });
    g.finish();
}

criterion_group!(benches, stat_scan, reads, writes, compaction);
criterion_main!(benches);

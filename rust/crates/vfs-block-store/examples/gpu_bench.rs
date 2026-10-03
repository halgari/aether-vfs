//! Bulk writes through the store's write path with CPU zstd and with the GPU compressor.
//!
//! ```sh
//! cargo run --release -p vfs-block-store --features gpu-zstd --example gpu_bench -- \
//!     [--input DIR] [--max-bytes 2147483648] [--writers 16] [--run-blocks 64] \
//!     [--configs zstd:6,gpu:opt16p1]
//! ```
//!
//! With `--input`, the files under DIR (DDS, NIF, anything) are loaded first, up to
//! `--max-bytes`, taking files from each top-level directory in turn so that a mix of mods is
//! used. Without it, the data is synthetic: DDS-like texture blocks, text and incompressible
//! bytes. Each configuration writes every file into a fresh store (in `$TMPDIR`) from
//! `--writers` threads as bulk writes, in runs of `--run-blocks` blocks (64 is what
//! `vfs-storage`'s layers write at once, and what a 4 MiB cache fetch unit holds), and
//! prints logical MB/s and the stored ratio. The GPU is opened (and warmed) before the clock
//! starts; its opening time is printed apart.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use vfs_block_store::{
    BlockStore, BulkCompression, GpuConfig, GpuLevel, StoreConfig, WriteClass, with_write_class,
};

const BS: usize = 64 * 1024;

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut ents: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    ents.sort();
    for p in ents {
        if p.is_dir() {
            files_under(&p, out);
        } else if p.is_file() {
            out.push(p);
        }
    }
}

/// Up to `max` bytes of files, round robin over `dir`'s top-level directories.
fn load(dir: &Path, max: usize) -> Vec<Vec<u8>> {
    let mut tops: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("read --input")
        .flatten()
        .map(|e| e.path())
        .collect();
    tops.sort();
    let mut lists: Vec<std::vec::IntoIter<PathBuf>> = tops
        .iter()
        .map(|t| {
            let mut v = Vec::new();
            if t.is_dir() {
                files_under(t, &mut v);
            } else {
                v.push(t.clone());
            }
            v.into_iter()
        })
        .collect();
    let mut out = Vec::new();
    let mut total = 0;
    'outer: loop {
        let mut any = false;
        for l in &mut lists {
            if let Some(p) = l.next() {
                any = true;
                let d = std::fs::read(&p).unwrap_or_default();
                if d.is_empty() {
                    continue;
                }
                total += d.len();
                out.push(d);
                if total >= max {
                    break 'outer;
                }
            }
        }
        if !any {
            break;
        }
    }
    out
}

fn xorshift(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

/// DDS-like, text-like and random files, about 70/20/10 by bytes.
fn synthetic(max: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut total = 0;
    let mut seed = 0x1234_5678_9abc_def1u64;
    while total < max {
        let kind = xorshift(&mut seed) % 10;
        let len = (1 << 20) + (xorshift(&mut seed) % (8 << 20)) as usize;
        let mut v = Vec::with_capacity(len);
        match kind {
            0..=6 => {
                v.extend_from_slice(b"DDS |\0\0\0\x07\x10\x08\0");
                v.resize(128, 0);
                let mut c0 = xorshift(&mut seed) as u16;
                while v.len() < len {
                    let x = xorshift(&mut seed);
                    if x.is_multiple_of(5) {
                        c0 = c0.wrapping_add((x >> 20) as u16 & 0x0841);
                    }
                    let idx = if x.is_multiple_of(3) {
                        0x5555_5555u32
                    } else {
                        (x >> 32) as u32
                    };
                    v.extend_from_slice(&c0.to_le_bytes());
                    v.extend_from_slice(&c0.wrapping_sub(0x0421).to_le_bytes());
                    v.extend_from_slice(&idx.to_le_bytes());
                }
            }
            7 | 8 => {
                let words: [&[u8]; 8] = [
                    b"NiNode ",
                    b"BSTriShape ",
                    b"Skyrim ",
                    b"texture ",
                    b"0.000000 ",
                    b"1.0 ",
                    b"\x00\x00\x80\x3f",
                    b"Whiterun ",
                ];
                while v.len() < len {
                    v.extend_from_slice(words[(xorshift(&mut seed) % 8) as usize]);
                }
            }
            _ => {
                while v.len() < len {
                    v.extend_from_slice(&xorshift(&mut seed).to_le_bytes());
                }
            }
        }
        v.truncate(len);
        total += len;
        out.push(v);
    }
    out
}

fn config(name: &str) -> StoreConfig {
    let base = StoreConfig {
        block_size: BS as u32,
        ..StoreConfig::default()
    };
    if let Some(l) = name.strip_prefix("zstd:") {
        let l: i32 = l.parse().expect("zstd level");
        StoreConfig {
            zstd_level: l,
            bulk: BulkCompression::Foreground,
            ..base
        }
    } else if let Some(p) = name.strip_prefix("gpu:") {
        let level = GpuLevel::from_name(p).expect("gpu preset");
        let vram = std::env::var("GPU_BENCH_VRAM_MIB")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(GpuConfig::default().vram_budget_mib);
        StoreConfig {
            bulk: BulkCompression::Gpu(GpuConfig {
                level,
                vram_budget_mib: vram,
                ..GpuConfig::default()
            }),
            ..base
        }
    } else {
        panic!("unknown config {name}");
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1).cloned())
    };
    let max: usize = arg("--max-bytes").map_or(2 << 30, |v| v.parse().unwrap());
    let writers: usize = arg("--writers").map_or(16, |v| v.parse().unwrap());
    let configs = arg("--configs").unwrap_or_else(|| "zstd:6,gpu:opt16p1".into());
    let input = arg("--input");
    let run_blocks: usize = arg("--run-blocks").map_or(64, |v| v.parse().unwrap());
    let run = run_blocks * BS;
    let t = Instant::now();
    let files = match &input {
        Some(d) => load(Path::new(d), max),
        None => synthetic(max),
    };
    let total: usize = files.iter().map(Vec::len).sum();
    println!(
        "{} files, {:.2} GiB ({}), loaded in {:.1}s; {writers} writers, runs of {} KiB",
        files.len(),
        total as f64 / (1u64 << 30) as f64,
        input.as_deref().unwrap_or("synthetic DDS-like/text/random"),
        t.elapsed().as_secs_f64(),
        run / 1024
    );
    let tmp = std::env::temp_dir();
    for name in configs.split(',') {
        let dir = tempfile::Builder::new()
            .prefix("gpu-bench-")
            .tempdir_in(&tmp)
            .unwrap();
        let store = BlockStore::open(dir.path(), config(name)).unwrap();
        // Open the GPU (or not) before the clock: one bulk block.
        let t0 = Instant::now();
        store.set_len(b"warm", 4096).unwrap();
        store
            .write_blocks_as(b"warm", 0, &[7u8; 4096], WriteClass::Bulk)
            .unwrap();
        let open = t0.elapsed();
        let before = store.write_stats();
        for (i, f) in files.iter().enumerate() {
            store
                .set_len(format!("f{i}").as_bytes(), f.len() as u64)
                .unwrap();
        }
        let next = AtomicUsize::new(0);
        let t1 = Instant::now();
        std::thread::scope(|s| {
            for _ in 0..writers {
                s.spawn(|| {
                    with_write_class(WriteClass::Bulk, || {
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some(f) = files.get(i) else { break };
                            let id = format!("f{i}");
                            for (k, part) in f.chunks(run).enumerate() {
                                store
                                    .write_blocks(id.as_bytes(), (k * run / BS) as u64, part)
                                    .unwrap();
                            }
                        }
                    })
                });
            }
        });
        let secs = t1.elapsed().as_secs_f64();
        let st = store.write_stats();
        let d = st.bulk.since(&before.bulk);
        println!(
            "{name:>14}: {:>8.0} MB/s  ratio {:.4} (raw/stored, new blocks {:.2} GiB -> {:.2} GiB; {} raw)  {:.1}s  [{}; opening {:.2}s]",
            d.logical_bytes as f64 / secs / 1e6,
            d.new_raw_bytes as f64 / d.stored_bytes as f64,
            d.new_raw_bytes as f64 / (1u64 << 30) as f64,
            d.stored_bytes as f64 / (1u64 << 30) as f64,
            d.uncompressed_blocks,
            secs,
            store.compression(WriteClass::Bulk),
            open.as_secs_f64(),
        );
        if let Some(g) = st.gpu {
            println!(
                "{:>14}  GPU batches {}, blocks {}, CPU fallback blocks {}{}",
                "",
                g.batches,
                g.blocks,
                g.cpu_fallback_blocks,
                g.adapter.map(|a| format!(", {a}")).unwrap_or_default()
            );
        }
        // Spot-check: every file reads back.
        let mut buf = Vec::new();
        for (i, f) in files.iter().enumerate().step_by(97) {
            buf.resize(f.len(), 0);
            let r = store.read(format!("f{i}").as_bytes(), 0, &mut buf).unwrap();
            assert!(r.missing.is_empty());
            assert!(buf == *f, "file {i} reads back different");
        }
        store.close().unwrap();
    }
}

//! What `cached` costs per read, with no host language anywhere in the picture.
//!
//! The Node spike measured a JS provider going ~60x *slower* once
//! `CachingProvider` was put in front of it at the default 1 MiB block size.
//! That is a claim about `vfs-cache`, so it must be provable without N-API,
//! without a JS engine, and without the spike's own bridge. This binary does
//! that: the leaf is a trivial Rust provider that memcpys from a static buffer.

use std::sync::Arc;
use std::time::Instant;

use vfs_embed::{
    BlockCache, CacheConfig, Capabilities, CachingProvider, DirEntry, Handle, Provider, Stat, VPath,
    KIND_FILE,
};

const FILE_SIZE: u64 = 64 * 1024 * 1024;

struct MemLeaf {
    src: Vec<u8>,
}

impl Provider for MemLeaf {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            immutable: true,
            slow: true,
            ..Capabilities::read_only()
        }
    }
    fn getattr(&self, _p: VPath) -> Result<Option<Stat>, i32> {
        Ok(Some(Stat {
            kind: KIND_FILE,
            size: FILE_SIZE,
            mtime: 0,
        }))
    }
    fn readdir(&self, _p: VPath) -> Result<Vec<DirEntry>, i32> {
        Ok(Vec::new())
    }
    fn open(&self, _p: VPath, _f: u32) -> Result<(Handle, u64, bool), i32> {
        Ok((1, FILE_SIZE, false))
    }
    fn close(&self, _h: Handle) -> Result<(), i32> {
        Ok(())
    }
    fn read_at(&self, _h: Handle, _offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let n = buf.len().min(self.src.len());
        buf[..n].copy_from_slice(&self.src[..n]);
        Ok(n)
    }
}

fn sweep(label: &str, top: Arc<dyn Provider>, read_size: usize) {
    let h = top.open(VPath::at_default("bench.bin"), 0).expect("open").0;
    let mut buf = vec![0u8; read_size];
    let mut lat = Vec::new();
    let mut off = 0u64;
    let t0 = Instant::now();
    while off < FILE_SIZE {
        let t = Instant::now();
        let n = top.read_at(h, off, &mut buf).expect("read");
        lat.push(t.elapsed().as_nanos() as u64);
        if n == 0 {
            break;
        }
        off += n as u64;
    }
    let elapsed = t0.elapsed();
    top.close(h).expect("close");
    lat.sort_unstable();
    let mib = (FILE_SIZE as f64) / (1024.0 * 1024.0) / elapsed.as_secs_f64();
    println!(
        "{label:<34} {mib:>9.1} MiB/s   p50 {:>8.2} us   p99 {:>8.2} us   reads {}",
        lat[lat.len() / 2] as f64 / 1000.0,
        lat[lat.len() * 99 / 100] as f64 / 1000.0,
        lat.len()
    );
}

/// Aggregate throughput with `threads` readers sharing one `BlockCache`, each
/// sweeping the whole file through its own handle. This is the measurement for
/// defect 3 (one process-wide mutex): if readers serialise, aggregate MiB/s
/// stays flat as `threads` rises while p50 grows in proportion.
fn sweep_threads(label: &str, top: Arc<dyn Provider>, read_size: usize, threads: usize) {
    // Warm the cache once, single-threaded, so the measured region is hits.
    {
        let h = top.open(VPath::at_default("bench.bin"), 0).expect("open").0;
        let mut buf = vec![0u8; read_size];
        let mut off = 0u64;
        while off < FILE_SIZE {
            let n = top.read_at(h, off, &mut buf).expect("read");
            if n == 0 {
                break;
            }
            off += n as u64;
        }
        top.close(h).expect("close");
    }
    let t0 = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..threads {
        let top = Arc::clone(&top);
        handles.push(std::thread::spawn(move || {
            let h = top.open(VPath::at_default("bench.bin"), 0).expect("open").0;
            let mut buf = vec![0u8; read_size];
            let mut lat = Vec::new();
            let mut off = 0u64;
            while off < FILE_SIZE {
                let t = Instant::now();
                let n = top.read_at(h, off, &mut buf).expect("read");
                lat.push(t.elapsed().as_nanos() as u64);
                if n == 0 {
                    break;
                }
                off += n as u64;
            }
            top.close(h).expect("close");
            lat
        }));
    }
    let mut lat: Vec<u64> = Vec::new();
    for h in handles {
        lat.extend(h.join().expect("thread"));
    }
    let elapsed = t0.elapsed();
    lat.sort_unstable();
    let total = (FILE_SIZE as f64) * (threads as f64);
    let mib = total / (1024.0 * 1024.0) / elapsed.as_secs_f64();
    println!(
        "{label:<34} {mib:>9.1} MiB/s   p50 {:>8.2} us   p99 {:>8.2} us   reads {}",
        lat[lat.len() / 2] as f64 / 1000.0,
        lat[lat.len() * 99 / 100] as f64 / 1000.0,
        lat.len()
    );
}

/// Hit latency against the number of blocks resident in the cache. This is the
/// measurement for defect 2 (O(n) LRU scan per hit): a fixed read count over a
/// working set of `window` bytes at a 4 KiB block size leaves `window / 4096`
/// blocks resident, so p50 rising with `window` is the scan and nothing else.
fn sweep_residency(label: &str, top: Arc<dyn Provider>, window: u64, reads: usize) {
    let read_size = 4096usize;
    let h = top.open(VPath::at_default("bench.bin"), 0).expect("open").0;
    let mut buf = vec![0u8; read_size];
    // One warm pass over the window, then the measured passes are all hits.
    let mut off = 0u64;
    while off < window {
        let n = top.read_at(h, off, &mut buf).expect("read");
        if n == 0 {
            break;
        }
        off += n as u64;
    }
    let mut lat = Vec::with_capacity(reads);
    let mut off = 0u64;
    let t0 = Instant::now();
    for _ in 0..reads {
        let t = Instant::now();
        top.read_at(h, off, &mut buf).expect("read");
        lat.push(t.elapsed().as_nanos() as u64);
        off += read_size as u64;
        if off >= window {
            off = 0;
        }
    }
    let elapsed = t0.elapsed();
    top.close(h).expect("close");
    lat.sort_unstable();
    let mib = (reads as f64) * (read_size as f64) / (1024.0 * 1024.0) / elapsed.as_secs_f64();
    println!(
        "{label:<34} {mib:>9.1} MiB/s   p50 {:>8.2} us   p99 {:>8.2} us   blocks {}",
        lat[lat.len() / 2] as f64 / 1000.0,
        lat[lat.len() * 99 / 100] as f64 / 1000.0,
        window / 4096
    );
}

/// Hit latency on **one hot block** with `window / 4096` other blocks resident.
///
/// This is the honest measurement for defect 2, and `sweep_residency` above is
/// not: an LRU deque scanned front-to-back finds a *cyclically* swept block at
/// index 0 every time, because the block you are about to touch is exactly the
/// least recently used one. The scan is accidentally O(1) for that pattern. Re-
/// reading a single block is the worst case and an ordinary one (a header re-
/// read while the rest of the file stays resident): after the first hit the
/// block sits at the *back* of the deque, so every later hit walks the whole
/// thing. p50 rising with `window` here is the scan and cannot be anything else
/// — the working set touched per read is one 4 KiB block regardless of `window`.
fn sweep_hot_block(label: &str, top: Arc<dyn Provider>, window: u64, reads: usize) {
    let read_size = 4096usize;
    let h = top.open(VPath::at_default("bench.bin"), 0).expect("open").0;
    let mut buf = vec![0u8; read_size];
    let mut off = 0u64;
    while off < window {
        let n = top.read_at(h, off, &mut buf).expect("read");
        if n == 0 {
            break;
        }
        off += n as u64;
    }
    let mut lat = Vec::with_capacity(reads);
    for _ in 0..reads {
        let t = Instant::now();
        top.read_at(h, 0, &mut buf).expect("read");
        lat.push(t.elapsed().as_nanos() as u64);
    }
    top.close(h).expect("close");
    lat.sort_unstable();
    // Padding keeps the p50/p99 columns aligned with the other sweeps, which
    // print a MiB/s figure here. This sweep deliberately does not: it re-reads
    // one block, so a throughput number would say nothing.
    println!(
        "{label:<34} {:>21}   p50 {:>8.2} us   p99 {:>8.2} us   blocks {}",
        " ",
        lat[lat.len() / 2] as f64 / 1000.0,
        lat[lat.len() * 99 / 100] as f64 / 1000.0,
        window / 4096
    );
}

fn cached(leaf: &Arc<dyn Provider>, bs: u64) -> Arc<dyn Provider> {
    let cache = Arc::new(BlockCache::new(CacheConfig {
        block_size: bs,
        ram_budget: FILE_SIZE * 2,
        disk_dir: None,
    }));
    Arc::new(CachingProvider::new(Arc::clone(leaf), cache, 1))
}

fn main() {
    // 1 MiB of source, enough for the largest block fetched below.
    let leaf: Arc<dyn Provider> = Arc::new(MemLeaf {
        src: vec![0xABu8; 1024 * 1024],
    });

    for read_size in [4096usize, 65536] {
        let k = if read_size == 4096 { "4K" } else { "64K" };
        sweep(&format!("rust leaf {k} raw"), Arc::clone(&leaf), read_size);
        for bs in [4096u64, 16384, 65536, 262144, 1048576] {
            sweep(
                &format!("rust leaf {k} cached blk={}K", bs / 1024),
                cached(&leaf, bs),
                read_size,
            );
        }
    }

    println!();
    for threads in [1usize, 2, 4, 8] {
        sweep_threads(
            &format!("4K cached blk=64K threads={threads}"),
            cached(&leaf, 65536),
            4096,
            threads,
        );
    }

    println!();
    for window in [256u64 * 1024, 4 * 1024 * 1024, FILE_SIZE] {
        sweep_residency(
            &format!("4K hits blk=4K resident={}K", window / 1024),
            cached(&leaf, 4096),
            window,
            200_000,
        );
    }

    println!();
    for window in [256u64 * 1024, 4 * 1024 * 1024, FILE_SIZE] {
        sweep_hot_block(
            &format!("4K hot block, resident={}K", window / 1024),
            cached(&leaf, 4096),
            window,
            50_000,
        );
    }
}

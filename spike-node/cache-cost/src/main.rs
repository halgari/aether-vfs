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

fn main() {
    // 1 MiB of source, enough for the largest block fetched below.
    let leaf: Arc<dyn Provider> = Arc::new(MemLeaf {
        src: vec![0xABu8; 1024 * 1024],
    });

    for read_size in [4096usize, 65536] {
        let k = if read_size == 4096 { "4K" } else { "64K" };
        sweep(&format!("rust leaf {k} raw"), Arc::clone(&leaf), read_size);
        for bs in [4096u64, 16384, 65536, 262144, 1048576] {
            let cache = Arc::new(BlockCache::new(CacheConfig {
                block_size: bs,
                ram_budget: FILE_SIZE * 2,
                disk_dir: None,
            }));
            let top: Arc<dyn Provider> =
                Arc::new(CachingProvider::new(Arc::clone(&leaf), cache, 1));
            sweep(
                &format!("rust leaf {k} cached blk={}K", bs / 1024),
                top,
                read_size,
            );
        }
    }
}

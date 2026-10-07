//! Opt-in scale test. Writes `BLOCK_STORE_SCALE_GB` GiB (default 100) of synthetic, partly
//! duplicated data into `BLOCK_STORE_SCALE_DIR` (default: a temp dir) and checks index size and
//! resident memory against the design estimates. `BLOCK_STORE_SCALE_CACHE_MB` sets the redb
//! cache (default 64).
//! Run with: cargo test --release --test scale -- --ignored --nocapture

use std::time::Instant;

use vfs_block_store::{BlockStore, StoreConfig};

const BS: usize = 64 * 1024;

fn env_or(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Half random, half zero: compresses to about 50%.
fn block(seed: u64) -> Vec<u8> {
    let mut out = vec![0u8; BS];
    blake3::Hasher::new()
        .update(&seed.to_le_bytes())
        .finalize_xof()
        .fill(&mut out[..BS / 2]);
    out
}

fn rss_mib() -> u64 {
    memory_stats::memory_stats().map_or(0, |m| m.physical_mem as u64 >> 20)
}

#[test]
#[ignore]
fn hundred_gigabytes() {
    let gib = env_or("BLOCK_STORE_SCALE_GB", 100);
    let cache_mib = env_or("BLOCK_STORE_SCALE_CACHE_MB", 64);
    let tmp;
    let dir = match std::env::var("BLOCK_STORE_SCALE_DIR") {
        Ok(d) => std::path::PathBuf::from(d),
        Err(_) => {
            tmp = vfs_testkit::tempdir().unwrap();
            tmp.path().to_path_buf()
        }
    };
    let cfg = StoreConfig {
        index_cache_bytes: (cache_mib as usize) << 20,
        ..StoreConfig::default()
    };
    let store = BlockStore::open(&dir, cfg).unwrap();
    let file_blocks = 256u64; // 16 MiB files
    let files = gib * 1024 / 16;
    let start = Instant::now();
    let mut peak_rss = 0;
    for f in 0..files {
        let id = format!("file-{f:08}");
        let data: Vec<u8> = (0..file_blocks)
            // One block in eight repeats an earlier one, so dedup is exercised.
            .flat_map(|b| {
                let n = f * file_blocks + b;
                block(if n % 8 == 7 { n / 2 } else { n })
            })
            .collect();
        store.set_len(id.as_bytes(), data.len() as u64).unwrap();
        store.write_blocks(id.as_bytes(), 0, &data).unwrap();
        if f % 128 == 0 {
            peak_rss = peak_rss.max(rss_mib());
            eprintln!(
                "{} GiB written, {:.0}s, rss {} MiB",
                f * 16 / 1024,
                start.elapsed().as_secs_f64(),
                rss_mib()
            );
        }
    }
    store.flush().unwrap();
    let elapsed = start.elapsed();

    let blocks = files * file_blocks;
    let index = store.index_size().unwrap();
    eprintln!(
        "{gib} GiB in {:.0}s ({:.0} MiB/s); index pages {} MiB ({:.1} bytes/block, {:.1} stored); \
         peak rss {peak_rss} MiB; packs {}",
        elapsed.as_secs_f64(),
        (gib * 1024) as f64 / elapsed.as_secs_f64(),
        index.page_bytes >> 20,
        index.page_bytes as f64 / blocks as f64,
        index.stored_bytes as f64 / blocks as f64,
        store.stats().unwrap().packs.len(),
    );
    // Measured about 103 bytes/block of pages (76 stored); fail well above that.
    assert!(
        index.page_bytes / blocks < 160,
        "index uses {} bytes per block",
        index.page_bytes / blocks
    );
    // Measured: about 35 MiB baseline plus the redb cache. Allow 100 MiB of headroom.
    assert!(
        peak_rss < cache_mib + 135,
        "peak resident memory {peak_rss} MiB"
    );
}

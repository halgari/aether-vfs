//! Times `Storage::open` and `Storage::close` on an existing storage
//! directory: `cargo run --release -p aether-storage --example open_time -- DIR [ROUNDS]`.
//!
//! **It writes to `DIR`** (opening a store is a write: reconciliation, the
//! clean-close marker), so point it at a copy, never at a live store.
//!
//! `INDEX_CACHE_MIB` and `CATALOG_CACHE_MIB` set the redb page caches
//! (defaults 1024 and 256).

use std::time::Instant;

use aether_storage::{Storage, StorageConfig};

fn mib(var: &str, default: usize) -> usize {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
        << 20
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: open_time DIR [ROUNDS]");
    let rounds: usize = args.next().map_or(2, |r| r.parse().expect("ROUNDS"));
    let mut cfg = StorageConfig::default();
    cfg.store.index_cache_bytes = mib("INDEX_CACHE_MIB", 1024);
    cfg.catalog_cache_bytes = mib("CATALOG_CACHE_MIB", 256);
    for round in 1..=rounds {
        let t = Instant::now();
        let s = Storage::open(&dir, cfg.clone()).expect("open");
        let opened = t.elapsed();
        let r = s.last_reconcile().clone();
        let cached = s.cache_stats().cached_logical_bytes;
        let t = Instant::now();
        let outcome = s.close().expect("close");
        println!(
            "round {round}: open {:.3}s, close {:.3}s ({outcome:?}); cached {cached} B; \
             reconcile: {r:?}",
            opened.as_secs_f64(),
            t.elapsed().as_secs_f64()
        );
    }
}

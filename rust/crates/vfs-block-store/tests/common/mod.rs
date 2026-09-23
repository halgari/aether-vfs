#![allow(dead_code)]

use block_store::{BlockStore, StoreConfig};

pub const BS: usize = 4096;

/// Small blocks and packs so tests exercise rotation and segments quickly.
pub fn test_config() -> StoreConfig {
    StoreConfig {
        block_size: BS as u32,
        max_pack_size: 64 * 1024,
        index_cache_bytes: 4 << 20,
        write_txn_bytes: 8 * BS,
        ..StoreConfig::default()
    }
}

pub fn open(dir: &std::path::Path) -> BlockStore {
    BlockStore::open(dir, test_config()).unwrap()
}

/// Deterministic incompressible bytes.
pub fn random_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    blake3::Hasher::new()
        .update(&seed.to_le_bytes())
        .finalize_xof()
        .fill(&mut out);
    out
}

/// Compressible bytes: a repeated seed-dependent pattern.
pub fn pattern_bytes(seed: u64, len: usize) -> Vec<u8> {
    (0..len).map(|i| (seed as usize + i / 64) as u8).collect()
}

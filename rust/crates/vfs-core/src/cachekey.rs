//! Cache-key computation.

use crate::model::{CacheKey, SourceId};

/// Cache key = blake3 over the winning source identity plus its size and mtime.
/// Identical resolved inputs dedupe; a changed size/mtime yields a new key.
pub fn compute_cache_key(source: &SourceId, size: u64, mtime: i64) -> CacheKey {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&source.0);
    hasher.update(&size.to_le_bytes());
    hasher.update(&mtime.to_le_bytes());
    CacheKey(*hasher.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_for_same_inputs() {
        let s = SourceId::from("root/data/a.esp");
        assert_eq!(compute_cache_key(&s, 100, 5), compute_cache_key(&s, 100, 5));
    }

    #[test]
    fn changes_on_size_or_mtime() {
        let s = SourceId::from("root/data/a.esp");
        let base = compute_cache_key(&s, 100, 5);
        assert_ne!(base, compute_cache_key(&s, 101, 5));
        assert_ne!(base, compute_cache_key(&s, 100, 6));
    }

    #[test]
    fn dedupes_identical_sources() {
        // Two vpaths resolving to the same source+size+mtime → same key.
        let s = SourceId::from("root/shared/tex.dds");
        assert_eq!(compute_cache_key(&s, 2048, 9), compute_cache_key(&s, 2048, 9));
    }
}

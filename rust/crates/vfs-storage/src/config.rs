//! [`StorageConfig`]: the block store's own configuration plus the two budgets
//! `vfs-storage` adds on top of it.

use vfs_block_store::StoreConfig;

/// Configuration for [`crate::Storage::open`].
#[derive(Debug, Clone)]
pub struct StorageConfig {
    /// The block store's configuration. Its `block_size` is fixed when the
    /// store is created; reopening with a different one fails.
    pub store: StoreConfig,
    /// Budget for pull-through cache files, in **logical** bytes (file lengths,
    /// before dedup and compression). Layer data is never counted against it.
    pub cache_max_bytes: u64,
    /// Budget for the RAM tier of decompressed blocks.
    pub ram_tier_bytes: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            store: StoreConfig::default(),
            cache_max_bytes: 32 << 30,
            ram_tier_bytes: 256 << 20,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_documented_budgets() {
        let c = StorageConfig::default();
        assert_eq!(c.cache_max_bytes, 32 << 30);
        assert_eq!(c.ram_tier_bytes, 256 << 20);
        assert_eq!(c.store.block_size, StoreConfig::default().block_size);
    }
}

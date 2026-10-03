use crate::error::{Error, Result};

/// Store configuration. `block_size` is fixed when a store is created; the rest may change between opens.
#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// Block size in bytes. Fixed at creation and saved in the index.
    pub block_size: u32,
    /// zstd compression level for new writes of [`crate::WriteClass::Foreground`] (and of
    /// bulk writes under [`BulkCompression::Foreground`]).
    pub zstd_level: i32,
    /// How new blocks of [`crate::WriteClass::Bulk`] writes are compressed.
    pub bulk: BulkCompression,
    /// A pack is sealed once appending another record would exceed this size.
    pub max_pack_size: u64,
    /// redb page cache size.
    pub index_cache_bytes: usize,
    /// Large writes are split into index transactions of about this many bytes.
    pub write_txn_bytes: usize,
    /// A durable flush happens automatically after this many bytes are appended. Bounds redb memory
    /// use, since non-durable redb commits hold memory until the next durable commit.
    pub auto_flush_bytes: u64,
    /// A durable flush also happens automatically after this many non-durable index commits
    /// (writes, `set_len`, `delete`, heals, compaction batches), which bounds redb memory when
    /// few bytes are appended. Must be at least 1.
    pub auto_flush_commits: u64,
    /// Maximum length of a file id in bytes.
    pub max_file_id_len: usize,
    /// Threads for hashing and compression. `None` uses rayon's global pool.
    pub compression_threads: Option<usize>,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            block_size: 64 * 1024,
            zstd_level: 6,
            max_pack_size: 4 << 30,
            index_cache_bytes: 64 << 20,
            write_txn_bytes: 16 << 20,
            auto_flush_bytes: 1 << 30,
            auto_flush_commits: 10_000,
            max_file_id_len: 256,
            compression_threads: None,
            bulk: BulkCompression::Foreground,
        }
    }
}

/// How the new blocks of bulk writes ([`crate::WriteClass::Bulk`]) are compressed. Whatever is
/// chosen, every block is stored as one zstd frame (or raw, when the frame would not be
/// smaller) and read back the same way.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum BulkCompression {
    /// As foreground writes: CPU zstd at [`StoreConfig::zstd_level`].
    #[default]
    Foreground,
    /// CPU zstd at this level.
    Zstd(i32),
    /// On the GPU, in batches shared by every concurrent bulk writer (see
    /// [`crate::GpuConfig`]). Falls back to CPU zstd at [`StoreConfig::zstd_level`] when the
    /// GPU cannot be used. Needs blocks of at most 64 KiB.
    #[cfg(feature = "gpu-zstd")]
    Gpu(crate::gpu::GpuConfig),
}

impl StoreConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if !(4096..=(16 << 20)).contains(&self.block_size) {
            return Err(Error::Config(
                "block_size must be between 4 KiB and 16 MiB".into(),
            ));
        }
        if !zstd::compression_level_range().contains(&self.zstd_level) {
            return Err(Error::Config("zstd_level out of range".into()));
        }
        match &self.bulk {
            BulkCompression::Foreground => {}
            BulkCompression::Zstd(l) => {
                if !zstd::compression_level_range().contains(l) {
                    return Err(Error::Config("bulk zstd level out of range".into()));
                }
            }
            #[cfg(feature = "gpu-zstd")]
            BulkCompression::Gpu(g) => g.validate(self.block_size).map_err(Error::Config)?,
        }
        if self.max_pack_size < self.block_size as u64 * 2 {
            return Err(Error::Config(
                "max_pack_size must be at least two blocks".into(),
            ));
        }
        if self.auto_flush_commits == 0 {
            return Err(Error::Config(
                "auto_flush_commits must be at least 1".into(),
            ));
        }
        if self.max_file_id_len == 0 || self.max_file_id_len > 4096 {
            return Err(Error::Config(
                "max_file_id_len must be between 1 and 4096".into(),
            ));
        }
        Ok(())
    }
}

/// Options for [`crate::BlockStore::compact`].
#[derive(Debug, Clone)]
pub struct CompactOptions {
    /// Only sealed packs whose garbage fraction is at least this are compacted.
    pub min_garbage_ratio: f64,
    /// Stop starting new packs once this many pack bytes have been processed.
    pub max_bytes: u64,
}

impl Default for CompactOptions {
    fn default() -> Self {
        Self {
            min_garbage_ratio: 0.5,
            max_bytes: u64::MAX,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        let cfg = StoreConfig::default();
        cfg.validate().unwrap();
        assert_eq!(cfg.block_size, 64 * 1024);
        assert_eq!(cfg.zstd_level, 6);
        assert_eq!(cfg.max_pack_size, 4 << 30);
    }

    #[test]
    fn rejects_out_of_range_values() {
        let bad = [
            StoreConfig {
                block_size: 1000,
                ..StoreConfig::default()
            },
            StoreConfig {
                zstd_level: 99,
                ..StoreConfig::default()
            },
            StoreConfig {
                max_pack_size: 1,
                ..StoreConfig::default()
            },
            StoreConfig {
                max_file_id_len: 0,
                ..StoreConfig::default()
            },
            StoreConfig {
                auto_flush_commits: 0,
                ..StoreConfig::default()
            },
            StoreConfig {
                bulk: BulkCompression::Zstd(99),
                ..StoreConfig::default()
            },
        ];
        for cfg in bad {
            assert!(matches!(cfg.validate(), Err(Error::Config(_))), "{cfg:?}");
        }
    }
}

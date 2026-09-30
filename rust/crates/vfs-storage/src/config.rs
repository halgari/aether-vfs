//! [`StorageConfig`]: the block store's own configuration plus the budgets and
//! the durability policy `vfs-storage` adds on top of it.

use std::time::Duration;

use vfs_block_store::StoreConfig;

/// When layer writes become durable: how often a [`crate::Storage`] pays for
/// a **durable point** (`BlockStore::flush()`, which fsyncs pack data and
/// commits the store's index durably, then the catalog's durable commit).
///
/// Whatever the policy, the store is never left inconsistent: every durable
/// catalog row references durable store data, and a file whose row was
/// removed or replaced is deleted from the store only after a durable point
/// made the removal durable. The policy only decides how much recent work a
/// crash (process kill, power loss) can take with it.
///
/// Durable points happen, in both modes, at [`crate::Storage::sync`],
/// [`crate::Storage::close`], when a layer's last provider drops, when a layer
/// is created, imported or deleted, and when reconciliation runs at open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// A handle's `close` (of a handle that wrote), its `flush`, and every
    /// namespace change (`mkdir`, `remove`, `rename`, a size change by
    /// `set_attr`) commit without fsyncs; a real durable point piggybacks on
    /// such an operation only once the last one is at least `max_interval`
    /// old. No background thread runs, so a store that stops changing stays
    /// non-durable until the next change, [`crate::Storage::sync`],
    /// [`crate::Storage::close`] or a layer provider's drop: a host that
    /// wants a batch durable calls `sync` when the batch is done.
    ///
    /// **After a crash** the store reopens as of its last durable point, and
    /// reconciliation at open repairs the rest (spec §6):
    ///
    /// - a file created since then is gone whole (its row was never durable,
    ///   so its store data is an orphan and is deleted); it is never visible
    ///   under its name with part of its data. Writing a temporary file and
    ///   renaming it over the real one therefore leaves either the old file
    ///   or the new one;
    /// - a removal, rename or `mkdir` since then is undone: a removed or
    ///   replaced file comes back with its data, since its store data is
    ///   deleted only after a durable point;
    /// - a file that existed at the last durable point and was rewritten in
    ///   place since can come back with its old bytes or its new ones; if the
    ///   block store's own auto-flush landed in the middle of that rewrite, a
    ///   mix, which reconciliation reports like a file that was open at a
    ///   crash under [`Durability::OnEveryClose`].
    Deferred {
        /// The longest a change waits for a durable point, provided anything
        /// changes after it (see above).
        max_interval: Duration,
    },
    /// Every `close` of a handle that wrote, every `flush`, and every
    /// namespace change is a durable point before it returns: a crash loses
    /// only writes on handles still open. Costs a pack fsync and two durable
    /// redb commits per operation, serialized across all writers.
    OnEveryClose,
}

impl Durability {
    /// The default policy: [`Durability::Deferred`] with a five-minute
    /// `max_interval`.
    pub const DEFAULT: Durability = Durability::Deferred {
        max_interval: Duration::from_secs(5 * 60),
    };
}

impl Default for Durability {
    fn default() -> Self {
        Durability::DEFAULT
    }
}

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
    /// When layer writes become durable; [`Durability::DEFAULT`] (deferred,
    /// at most five minutes) unless set.
    pub durability: Durability,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            store: StoreConfig::default(),
            cache_max_bytes: 32 << 30,
            ram_tier_bytes: 256 << 20,
            durability: Durability::default(),
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
        assert_eq!(
            c.durability,
            Durability::Deferred {
                max_interval: Duration::from_secs(300)
            }
        );
    }
}

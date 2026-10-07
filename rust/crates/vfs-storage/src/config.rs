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
/// is created, imported or deleted, and when reconciliation runs at open. One
/// that finds nothing non-durable skips the fsyncs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// A handle's `close` (of a handle that wrote), its `flush`, and every
    /// namespace change (`mkdir`, `remove`, `rename`, a size change by
    /// `set_attr`) commit without fsyncs; a real durable point piggybacks
    /// on such an operation once the last one is at least `max_interval`
    /// old, or once the catalog holds [`StorageConfig::max_deferred_commits`]
    /// non-durable commits (default 10,000; redb keeps their bookkeeping in
    /// memory until a durable commit).
    ///
    /// One exception: the `close`, `flush` or `set_attr` size change after
    /// writing to a file that **already existed at the last durable point**
    /// (a rewrite in place) makes a durable point at once, as
    /// [`Durability::OnEveryClose`] does. Files created since the last
    /// durable point, and namespace changes, stay deferred; so the cheap path
    /// is creating files, or writing a temporary file and renaming it over
    /// the real one.
    ///
    /// No background thread runs, so a store that stops changing stays
    /// non-durable until the next change, [`crate::Storage::sync`],
    /// [`crate::Storage::close`] or a layer provider's drop: a host that
    /// wants a batch durable calls `sync` when the batch is done.
    ///
    /// **After a crash** — a process kill as much as a power loss: the
    /// catalog's non-durable commits live only in the process — the store
    /// reopens as of its last durable point, and reconciliation at open
    /// repairs the rest (spec §6):
    ///
    /// - a file created since then is gone whole (its row was never durable,
    ///   so its store data is an orphan and is deleted); it is never visible
    ///   under its name with part of its data. Writing a temporary file and
    ///   renaming it over the real one therefore leaves either the old file
    ///   or the new one;
    /// - a removal, rename or `mkdir` since then is undone: a removed or
    ///   replaced file comes back with its data, since its store data is
    ///   deleted only after a durable point. Until then that data takes
    ///   space: a rename over a file holds both versions (about twice the
    ///   file) until the next durable point;
    /// - a rewrite in place of an older file was made durable when its
    ///   handle closed; one still open at the crash can come back old, new
    ///   or mixed, exactly as under [`Durability::OnEveryClose`].
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

/// A top-level directory of one layer whose files are temporaries (see
/// [`StorageConfig::scratch_dirs`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchDir {
    /// The layer's name, exactly as it is opened.
    pub layer: String,
    /// The directory, a single path component; it compares as paths do
    /// (case-insensitively).
    pub dir: String,
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
    /// Under [`Durability::Deferred`], a durable point also comes due once
    /// the catalog holds this many non-durable commits (redb keeps their
    /// bookkeeping in memory until a durable commit). A bulk writer that
    /// creates many small files (three catalog commits each) and makes its
    /// own durable points ([`crate::Storage::sync`]) raises it. At least 1.
    pub max_deferred_commits: u64,
    /// The catalog's redb page cache, bytes. Half of it may hold pages
    /// changed by non-durable commits; past that redb writes pages out one
    /// at a time, so a catalog much larger than this (hundreds of thousands
    /// of files) costs many small writes and reads.
    pub catalog_cache_bytes: usize,
    /// Directories whose files are temporaries, each a top-level directory
    /// of one named layer ([`ScratchDir`]): the host removes them after a
    /// crash, before it uses the layer. Under [`Durability::Deferred`] a
    /// file there never makes a durable point of its own when it is closed
    /// or flushed after one passed while it was being written (the rewrite
    /// rule above): its partial content after a crash is harmless, since
    /// the host deletes it. Every other layer, and every other directory of
    /// that layer, keeps the rule.
    ///
    /// Without this, a host that writes many large files at once into a
    /// temporary directory and renames them (as an installer does) pays a
    /// chain of durable points: every file open across one makes another at
    /// its close, which every other open file then spans.
    pub scratch_dirs: Vec<ScratchDir>,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            store: StoreConfig::default(),
            cache_max_bytes: 32 << 30,
            ram_tier_bytes: 256 << 20,
            durability: Durability::default(),
            max_deferred_commits: crate::durable::DEFERRED_MAX_COMMITS,
            catalog_cache_bytes: crate::catalog::CACHE_BYTES,
            scratch_dirs: Vec::new(),
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
        assert_eq!(c.max_deferred_commits, 10_000);
        assert_eq!(c.store.block_size, StoreConfig::default().block_size);
        assert_eq!(
            c.durability,
            Durability::Deferred {
                max_interval: Duration::from_secs(300)
            }
        );
    }
}

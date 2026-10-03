//! Space accounting.

use std::sync::atomic::Ordering;

use crate::error::Result;
use crate::index::PackState;
use crate::pack::pack_path;
use crate::store::BlockStore;

/// Space accounting for one pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackStats {
    pub id: u32,
    /// False for the pack currently being appended to.
    pub sealed: bool,
    pub file_bytes: u64,
    pub live_bytes: u64,
}

impl PackStats {
    /// Fraction of the pack file that is garbage.
    pub fn garbage_ratio(&self) -> f64 {
        if self.file_bytes == 0 {
            return 0.0;
        }
        self.file_bytes.saturating_sub(self.live_bytes) as f64 / self.file_bytes as f64
    }
}

#[derive(Debug, Clone)]
pub struct Stats {
    /// Packs that are active or sealed (retired packs awaiting deletion are excluded).
    pub packs: Vec<PackStats>,
    /// Corrupt blocks dropped from the index since the store was opened.
    pub healed_blocks: u64,
    /// Bytes appended since the last durable flush.
    pub unflushed_bytes: u64,
    /// Non-durable index commits since the last durable flush.
    pub unflushed_commits: u64,
}

impl BlockStore {
    pub fn stats(&self) -> Result<Stats> {
        let r = self.index.read()?;
        let mut packs = Vec::new();
        for (id, info) in r.packs()? {
            if info.state == PackState::Retired {
                continue;
            }
            let file_bytes = std::fs::metadata(pack_path(&self.pack_dir, id))
                .map(|m| m.len())
                .unwrap_or(0);
            packs.push(PackStats {
                id,
                sealed: info.state == PackState::Sealed,
                file_bytes,
                live_bytes: info.live_bytes,
            });
        }
        Ok(Stats {
            packs,
            healed_blocks: self.healed.load(Ordering::Relaxed),
            unflushed_bytes: self.unflushed.load(Ordering::Relaxed),
            unflushed_commits: self.unflushed_commits.load(Ordering::Relaxed),
        })
    }

    /// Size of the metadata index. Briefly takes the index write lock.
    pub fn index_size(&self) -> Result<IndexSize> {
        let (page_bytes, stored_bytes) = self.index.size()?;
        Ok(IndexSize {
            page_bytes,
            stored_bytes,
        })
    }
}

/// What the writes of one [`crate::WriteClass`] stored since the store opened. Take two and
/// [`ClassWriteStats::since`] for a phase.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClassWriteStats {
    /// Bytes written (new blocks and deduplicated ones).
    pub logical_bytes: u64,
    /// Blocks that were new to the store, and so compressed and appended.
    pub new_blocks: u64,
    /// Their uncompressed bytes.
    pub new_raw_bytes: u64,
    /// Bytes they take in packs (record headers included).
    pub stored_bytes: u64,
    /// New blocks stored raw because compression did not shrink them.
    pub uncompressed_blocks: u64,
}

impl ClassWriteStats {
    /// What was written between `earlier` and `self`.
    pub fn since(&self, earlier: &ClassWriteStats) -> ClassWriteStats {
        ClassWriteStats {
            logical_bytes: self.logical_bytes.saturating_sub(earlier.logical_bytes),
            new_blocks: self.new_blocks.saturating_sub(earlier.new_blocks),
            new_raw_bytes: self.new_raw_bytes.saturating_sub(earlier.new_raw_bytes),
            stored_bytes: self.stored_bytes.saturating_sub(earlier.stored_bytes),
            uncompressed_blocks: self
                .uncompressed_blocks
                .saturating_sub(earlier.uncompressed_blocks),
        }
    }

    /// Both together.
    pub fn plus(&self, other: &ClassWriteStats) -> ClassWriteStats {
        ClassWriteStats {
            logical_bytes: self.logical_bytes + other.logical_bytes,
            new_blocks: self.new_blocks + other.new_blocks,
            new_raw_bytes: self.new_raw_bytes + other.new_raw_bytes,
            stored_bytes: self.stored_bytes + other.stored_bytes,
            uncompressed_blocks: self.uncompressed_blocks + other.uncompressed_blocks,
        }
    }

    /// Logical bytes written that were already stored (deduplicated).
    pub fn dedup_bytes(&self) -> u64 {
        self.logical_bytes.saturating_sub(self.new_raw_bytes)
    }
}

/// Result of [`BlockStore::write_stats`]: what writes stored since the store opened, per class.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteStats {
    pub foreground: ClassWriteStats,
    pub bulk: ClassWriteStats,
    /// The GPU compressor's counters, when bulk writes use it.
    #[cfg(feature = "gpu-zstd")]
    pub gpu: Option<crate::gpu::GpuStats>,
}

/// Space of one class of files, as [`BlockStore::usage_by`] sums it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub files: u64,
    /// Bytes of the files' stored blocks, uncompressed (blocks not stored yet do not count).
    pub logical_bytes: u64,
    /// Distinct blocks the files reference.
    pub blocks: u64,
    /// Bytes those blocks take in packs, record headers included. A block shared by files of
    /// two classes counts in both.
    pub stored_bytes: u64,
}

impl BlockStore {
    /// What writes stored since the store opened, per write class.
    pub fn write_stats(&self) -> WriteStats {
        self.codec.stats()
    }

    /// What compresses `class` writes, for logs: `zstd:6`, `GPU opt16p1`, or
    /// `GPU opt16p1 (off: zstd:6)` once the GPU was turned off.
    pub fn compression(&self, class: crate::WriteClass) -> String {
        self.codec.label(class)
    }

    /// Sums [`Usage`] per class of file: `classify` names a file id's class, or `None` to skip
    /// it. Reads every manifest and every referenced block's index row, so it takes about a
    /// second per few million blocks.
    pub fn usage_by<K: Eq + std::hash::Hash + Clone>(
        &self,
        classify: impl Fn(&[u8]) -> Option<K>,
    ) -> Result<std::collections::HashMap<K, Usage>> {
        use crate::manifest::{MISSING, decode_ids};
        let _guard = self.tracker.enter();
        let r = self.index.read()?;
        let mut seen: std::collections::HashMap<K, std::collections::HashSet<u64>> =
            std::collections::HashMap::new();
        let mut out: std::collections::HashMap<K, Usage> = std::collections::HashMap::new();
        r.for_each_segment(|id, seg, value| {
            let Some(k) = classify(id) else {
                return Ok(());
            };
            let seen = seen.entry(k.clone()).or_default();
            let u = out.entry(k).or_default();
            if seg == 0 {
                u.files += 1;
            }
            for bid in decode_ids(value, seg).into_iter().filter(|&b| b != MISSING) {
                let Some(loc) = r.block(bid)? else { continue };
                u.logical_bytes += loc.raw_len as u64;
                if seen.insert(bid) {
                    u.blocks += 1;
                    u.stored_bytes += loc.record_len();
                }
            }
            Ok(())
        })?;
        Ok(out)
    }
}

/// Result of [`BlockStore::index_size`]. The index file itself grows in large steps and is
/// usually bigger than `page_bytes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexSize {
    /// Bytes of b-tree pages in use.
    pub page_bytes: u64,
    /// Bytes of keys and values, without b-tree overhead.
    pub stored_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn garbage_ratio() {
        let p = PackStats {
            id: 1,
            sealed: true,
            file_bytes: 100,
            live_bytes: 25,
        };
        assert_eq!(p.garbage_ratio(), 0.75);
        let empty = PackStats {
            id: 2,
            sealed: true,
            file_bytes: 0,
            live_bytes: 0,
        };
        assert_eq!(empty.garbage_ratio(), 0.0);
    }
}

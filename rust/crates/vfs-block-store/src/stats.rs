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

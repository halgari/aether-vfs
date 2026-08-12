//! Process-wide block store (RAM LRU + optional disk files).

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Cache key: source instance + stable file identity + block index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockKey {
    pub source_id: u64,
    pub file_id: u64,
    pub block_index: u64,
}

#[derive(Clone, Debug)]
pub struct CacheConfig {
    pub block_size: u64,
    /// Max RAM bytes for block payloads (not counting index overhead).
    pub ram_budget: u64,
    /// Optional directory for on-disk block files.
    pub disk_dir: Option<PathBuf>,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            block_size: crate::DEFAULT_BLOCK_SIZE,
            ram_budget: 64 * 1024 * 1024,
            disk_dir: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub ram_evicts: u64,
    pub disk_hits: u64,
    pub disk_writes: u64,
    pub bytes_from_cache: u64,
    pub bytes_from_source: u64,
    pub ram_bytes: u64,
    pub ram_blocks: u64,
}

struct RamEntry {
    data: Vec<u8>,
}

struct RamState {
    map: HashMap<BlockKey, RamEntry>,
    order: VecDeque<BlockKey>,
    bytes: u64,
}

/// Thread-safe block cache.
pub struct BlockCache {
    cfg: CacheConfig,
    ram: Mutex<RamState>,
    hits: AtomicU64,
    misses: AtomicU64,
    ram_evicts: AtomicU64,
    disk_hits: AtomicU64,
    disk_writes: AtomicU64,
    bytes_from_cache: AtomicU64,
    bytes_from_source: AtomicU64,
}

impl BlockCache {
    pub fn new(cfg: CacheConfig) -> Self {
        if let Some(dir) = &cfg.disk_dir {
            let _ = std::fs::create_dir_all(dir);
        }
        Self {
            cfg,
            ram: Mutex::new(RamState {
                map: HashMap::new(),
                order: VecDeque::new(),
                bytes: 0,
            }),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            ram_evicts: AtomicU64::new(0),
            disk_hits: AtomicU64::new(0),
            disk_writes: AtomicU64::new(0),
            bytes_from_cache: AtomicU64::new(0),
            bytes_from_source: AtomicU64::new(0),
        }
    }

    pub fn block_size(&self) -> u64 {
        self.cfg.block_size.max(1)
    }

    pub fn stats(&self) -> CacheStats {
        let ram = self.ram.lock().ok();
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            ram_evicts: self.ram_evicts.load(Ordering::Relaxed),
            disk_hits: self.disk_hits.load(Ordering::Relaxed),
            disk_writes: self.disk_writes.load(Ordering::Relaxed),
            bytes_from_cache: self.bytes_from_cache.load(Ordering::Relaxed),
            bytes_from_source: self.bytes_from_source.load(Ordering::Relaxed),
            ram_bytes: ram.as_ref().map(|r| r.bytes).unwrap_or(0),
            ram_blocks: ram.as_ref().map(|r| r.map.len() as u64).unwrap_or(0),
        }
    }

    /// Look up a block. Returns a clone of the payload on hit.
    pub fn get(&self, key: &BlockKey) -> Option<Vec<u8>> {
        if let Ok(mut g) = self.ram.lock() {
            if g.map.contains_key(key) {
                if let Some(i) = g.order.iter().position(|k| k == key) {
                    g.order.remove(i);
                    g.order.push_back(*key);
                }
                let data = g.map.get(key).unwrap().data.clone();
                self.hits.fetch_add(1, Ordering::Relaxed);
                self.bytes_from_cache
                    .fetch_add(data.len() as u64, Ordering::Relaxed);
                return Some(data);
            }
        }
        if let Some(path) = self.disk_path(key) {
            if let Ok(data) = std::fs::read(&path) {
                self.disk_hits.fetch_add(1, Ordering::Relaxed);
                self.hits.fetch_add(1, Ordering::Relaxed);
                self.bytes_from_cache
                    .fetch_add(data.len() as u64, Ordering::Relaxed);
                self.insert_ram(*key, data.clone());
                return Some(data);
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        None
    }

    /// Insert a filled block (from source).
    pub fn put(&self, key: BlockKey, data: Vec<u8>) {
        self.bytes_from_source
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        if let Some(path) = self.disk_path(&key) {
            if std::fs::write(&path, &data).is_ok() {
                self.disk_writes.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.insert_ram(key, data);
    }

    /// Drop all blocks for a file (invalidation).
    pub fn invalidate_file(&self, source_id: u64, file_id: u64) {
        if let Ok(mut g) = self.ram.lock() {
            let keys: Vec<BlockKey> = g
                .map
                .keys()
                .filter(|k| k.source_id == source_id && k.file_id == file_id)
                .copied()
                .collect();
            for k in keys {
                if let Some(e) = g.map.remove(&k) {
                    g.bytes = g.bytes.saturating_sub(e.data.len() as u64);
                }
                g.order.retain(|x| x != &k);
                if let Some(path) = self.disk_path(&k) {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }

    fn insert_ram(&self, key: BlockKey, data: Vec<u8>) {
        let Ok(mut g) = self.ram.lock() else {
            return;
        };
        if let Some(old) = g.map.remove(&key) {
            g.bytes = g.bytes.saturating_sub(old.data.len() as u64);
            g.order.retain(|k| k != &key);
        }
        let len = data.len() as u64;
        while g.bytes + len > self.cfg.ram_budget && !g.order.is_empty() {
            if let Some(victim) = g.order.pop_front() {
                if let Some(e) = g.map.remove(&victim) {
                    g.bytes = g.bytes.saturating_sub(e.data.len() as u64);
                    self.ram_evicts.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        if len <= self.cfg.ram_budget {
            g.bytes += len;
            g.map.insert(key, RamEntry { data });
            g.order.push_back(key);
        }
    }

    fn disk_path(&self, key: &BlockKey) -> Option<PathBuf> {
        let dir = self.cfg.disk_dir.as_ref()?;
        Some(dir.join(format!(
            "{:x}_{:x}_{:x}.blk",
            key.source_id, key.file_id, key.block_index
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ram_hit_and_miss() {
        let c = BlockCache::new(CacheConfig {
            block_size: 64,
            ram_budget: 1024,
            disk_dir: None,
        });
        let key = BlockKey {
            source_id: 1,
            file_id: 2,
            block_index: 0,
        };
        assert!(c.get(&key).is_none());
        c.put(key, vec![7u8; 32]);
        assert_eq!(c.get(&key).unwrap(), vec![7u8; 32]);
        let s = c.stats();
        assert_eq!(s.misses, 1);
        assert_eq!(s.hits, 1);
    }

    #[test]
    fn eviction_under_budget() {
        let c = BlockCache::new(CacheConfig {
            block_size: 8,
            ram_budget: 16,
            disk_dir: None,
        });
        c.put(
            BlockKey {
                source_id: 0,
                file_id: 0,
                block_index: 0,
            },
            vec![1u8; 8],
        );
        c.put(
            BlockKey {
                source_id: 0,
                file_id: 0,
                block_index: 1,
            },
            vec![2u8; 8],
        );
        c.put(
            BlockKey {
                source_id: 0,
                file_id: 0,
                block_index: 2,
            },
            vec![3u8; 8],
        );
        assert!(c.stats().ram_evicts >= 1);
        assert!(c.stats().ram_bytes <= 16);
    }

    #[test]
    fn invalidate_file_drops_blocks() {
        let c = BlockCache::new(CacheConfig {
            block_size: 8,
            ram_budget: 1024,
            disk_dir: None,
        });
        let k0 = BlockKey {
            source_id: 1,
            file_id: 9,
            block_index: 0,
        };
        let k1 = BlockKey {
            source_id: 1,
            file_id: 9,
            block_index: 1,
        };
        let other = BlockKey {
            source_id: 1,
            file_id: 10,
            block_index: 0,
        };
        c.put(k0, vec![1u8; 4]);
        c.put(k1, vec![2u8; 4]);
        c.put(other, vec![3u8; 4]);
        c.invalidate_file(1, 9);
        assert!(c.get(&k0).is_none());
        assert!(c.get(&k1).is_none());
        assert_eq!(c.get(&other).unwrap(), vec![3u8; 4]);
    }

    #[test]
    fn disk_tier_roundtrip() {
        let dir = std::env::temp_dir().join(format!("vfs-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let c = BlockCache::new(CacheConfig {
            block_size: 16,
            ram_budget: 0, // force disk-only after put fails RAM
            disk_dir: Some(dir.clone()),
        });
        // With ram_budget 0, put still writes disk then skips RAM.
        let key = BlockKey {
            source_id: 9,
            file_id: 1,
            block_index: 3,
        };
        c.put(key, b"disk-bytes-here!".to_vec());
        // Clear any RAM (none) and read from disk.
        let got = c.get(&key).expect("disk hit");
        assert_eq!(got, b"disk-bytes-here!");
        assert!(c.stats().disk_hits >= 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

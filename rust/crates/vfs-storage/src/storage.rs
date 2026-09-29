//! [`Storage`]: one block store, its catalog and the RAM tier, opened together.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use vfs_block_store::BlockStore;

use crate::catalog::Catalog;
use crate::config::StorageConfig;
use crate::ram::RamTier;

/// Errors from `vfs-storage`.
#[derive(Debug)]
pub enum StorageError {
    /// The block store failed.
    Store(vfs_block_store::Error),
    /// The catalog database failed, or holds a row it cannot decode.
    Catalog(String),
    Io(std::io::Error),
    /// A layer of this name already exists.
    LayerExists(String),
    /// No layer of this name (or id).
    NoSuchLayer(String),
    /// The layer is open and cannot be deleted or replaced.
    LayerInUse(String),
}

impl StorageError {
    /// The `vfs_provider` status a provider should answer with.
    pub fn to_status(&self) -> i32 {
        match self {
            StorageError::NoSuchLayer(_)
            | StorageError::Store(vfs_block_store::Error::NotFound) => vfs_provider::ST_NOT_FOUND,
            StorageError::LayerExists(_) => vfs_provider::ST_EXISTS,
            StorageError::Store(_)
            | StorageError::Catalog(_)
            | StorageError::Io(_)
            | StorageError::LayerInUse(_) => vfs_provider::ST_IO_ERROR,
        }
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::Store(e) => write!(f, "block store: {e}"),
            StorageError::Catalog(e) => write!(f, "catalog: {e}"),
            StorageError::Io(e) => write!(f, "i/o: {e}"),
            StorageError::LayerExists(n) => write!(f, "layer {n:?} already exists"),
            StorageError::NoSuchLayer(n) => write!(f, "no layer {n:?}"),
            StorageError::LayerInUse(n) => write!(f, "layer {n:?} is in use"),
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StorageError::Store(e) => Some(e),
            StorageError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<vfs_block_store::Error> for StorageError {
    fn from(e: vfs_block_store::Error) -> Self {
        StorageError::Store(e)
    }
}

impl From<std::io::Error> for StorageError {
    fn from(e: std::io::Error) -> Self {
        StorageError::Io(e)
    }
}

/// The block store, used as pull-through cache and layer storage: one per
/// directory, and one per process for a directory (the store holds a lock file).
pub struct Storage {
    pub(crate) store: BlockStore,
    pub(crate) catalog: Catalog,
    // Read by the cache and layer providers, which land in later changes.
    #[allow(dead_code)]
    pub(crate) ram: RamTier,
    pub(crate) cfg: StorageConfig,
}

impl Storage {
    /// Opens (creating if needed) the storage in `dir`: the block store in `dir`
    /// itself, the catalog at `dir/catalog.redb`, and an empty RAM tier.
    ///
    /// Fails with `Store(Locked)` while another `Storage` (in any process) has
    /// the directory open.
    pub fn open(dir: impl AsRef<Path>, cfg: StorageConfig) -> Result<Arc<Storage>, StorageError> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)?;
        // The store first: it takes the directory lock, so a second opener
        // fails here, before it opens (and waits on) the catalog database.
        let store = BlockStore::open(dir, cfg.store.clone())?;
        let catalog = Catalog::open(&dir.join("catalog.redb"))?;
        let ram = RamTier::with_geometry(cfg.ram_tier_bytes, u64::from(cfg.store.block_size));
        Ok(Arc::new(Storage {
            store,
            catalog,
            ram,
            cfg,
        }))
    }

    /// Flushes the store, then commits the catalog durably (in that order, so
    /// every durable catalog row references durable store data), then closes the
    /// store and releases the directory.
    ///
    /// If other references to this `Storage` are still alive, everything is
    /// flushed the same way but the directory stays locked until the last one
    /// drops (the store closes itself on drop).
    pub fn close(self: Arc<Self>) -> Result<(), StorageError> {
        self.store.flush()?;
        self.catalog.commit_durable()?;
        match Arc::try_unwrap(self) {
            Ok(s) => {
                let Storage { store, catalog, .. } = s;
                store.close()?;
                drop(catalog);
                Ok(())
            }
            Err(still_shared) => {
                tracing::warn!(
                    refs = Arc::strong_count(&still_shared),
                    "Storage::close with other references alive: flushed, but the \
                     directory stays locked until the last one drops"
                );
                Ok(())
            }
        }
    }

    /// The block store's block size in bytes.
    pub fn block_size(&self) -> u64 {
        u64::from(self.cfg.store.block_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StorageConfig;

    #[test]
    fn storage_opens_twice_in_sequence_but_not_concurrently() {
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
        assert!(
            Storage::open(dir.path(), StorageConfig::default()).is_err(),
            "the block store lock must hold"
        );
        s.close().unwrap();
        Storage::open(dir.path(), StorageConfig::default()).unwrap();
    }

    #[test]
    fn catalog_rows_survive_close_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
        let id = s.catalog.create_layer("prof").unwrap();
        assert_eq!(s.block_size(), 64 * 1024);
        s.close().unwrap();
        let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
        assert_eq!(s.catalog.layer_id("prof").unwrap(), Some(id));
        assert!(dir.path().join("catalog.redb").exists());
    }

    #[test]
    fn close_with_another_reference_flushes_and_keeps_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
        let other = Arc::clone(&s);
        s.close().unwrap();
        assert!(Storage::open(dir.path(), StorageConfig::default()).is_err());
        drop(other);
        Storage::open(dir.path(), StorageConfig::default()).unwrap();
    }

    #[test]
    fn errors_map_to_provider_statuses() {
        assert_eq!(
            StorageError::NoSuchLayer("x".into()).to_status(),
            vfs_provider::ST_NOT_FOUND
        );
        assert_eq!(
            StorageError::LayerExists("x".into()).to_status(),
            vfs_provider::ST_EXISTS
        );
        assert_eq!(
            StorageError::Store(vfs_block_store::Error::NotFound).to_status(),
            vfs_provider::ST_NOT_FOUND
        );
        assert_eq!(
            StorageError::Catalog("x".into()).to_status(),
            vfs_provider::ST_IO_ERROR
        );
        assert_eq!(
            StorageError::LayerInUse("x".into()).to_status(),
            vfs_provider::ST_IO_ERROR
        );
    }
}

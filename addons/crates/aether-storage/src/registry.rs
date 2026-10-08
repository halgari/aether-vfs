//! The [`vfs_embed::RegistrySync`] for a registry layer kept in a [`Storage`].
use std::sync::Arc;

use crate::Storage;

/// The [`vfs_embed::RegistrySync`] for a registry layer taken from `storage`
/// ([`Storage::layer`]): [`Storage::sync`], with its error as the provider
/// status ([`crate::StorageError::to_status`]). Holds the storage weakly, so
/// it never keeps the directory locked; a storage already gone was synced by
/// its own close or drop.
pub fn registry_sync_for(storage: &Arc<Storage>) -> vfs_embed::RegistrySync {
    let weak = Arc::downgrade(storage);
    Arc::new(move || match weak.upgrade() {
        Some(s) => s.sync().map_err(|e| e.to_status()),
        None => Ok(()),
    })
}

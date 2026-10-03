//! [`Storage`]: one block store, its catalog and the RAM tier, opened together.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak};
use std::time::{Duration, Instant};

use vfs_block_store::BlockStore;
use vfs_provider::Provider;

use crate::cached::{lock, CacheState};
use crate::catalog::Catalog;
use crate::config::{Durability, StorageConfig};
use crate::ids::Guid;
use crate::layer::LayerProvider;
use crate::ram::RamTier;
use crate::reconcile::{reconcile, ReconcileReport};

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
    /// No catalog entry at this (folded) path.
    NotFound(String),
    /// A directory still holds entries, so it cannot be removed.
    NotEmpty(String),
    /// The destination of a rename is a directory that holds entries, or an
    /// export's target directory is not empty.
    Exists(String),
    /// A request that can never succeed: a layer root as the target, or a
    /// directory moved into its own subtree.
    BadRequest(String),
}

impl StorageError {
    /// Whether another `Storage` (in this or another process) holds the
    /// directory: what [`Storage::open`] fails with when it is taken.
    pub fn is_locked(&self) -> bool {
        matches!(self, StorageError::Store(vfs_block_store::Error::Locked))
    }

    /// The `vfs_provider` status a provider should answer with.
    ///
    /// The directory refusals match `vfs-compose`'s `MemoryProvider`: removing
    /// a non-empty directory is `ST_IS_DIR` (which the shim already translates
    /// to what `DeleteFileW` gives for a directory), renaming onto an occupied
    /// directory is `ST_EXISTS`, and a move into its own subtree is
    /// `ST_BAD_REQUEST`.
    pub fn to_status(&self) -> i32 {
        match self {
            StorageError::NoSuchLayer(_)
            | StorageError::NotFound(_)
            | StorageError::Store(vfs_block_store::Error::NotFound) => vfs_provider::ST_NOT_FOUND,
            StorageError::LayerExists(_) | StorageError::Exists(_) => vfs_provider::ST_EXISTS,
            StorageError::NotEmpty(_) => vfs_provider::ST_IS_DIR,
            StorageError::BadRequest(_) => vfs_provider::ST_BAD_REQUEST,
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
            StorageError::NotFound(p) => write!(f, "no entry {p:?}"),
            StorageError::NotEmpty(p) => write!(f, "directory {p:?} is not empty"),
            StorageError::Exists(p) => write!(f, "{p:?} is a directory that is not empty"),
            StorageError::BadRequest(m) => write!(f, "bad request: {m}"),
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

/// What [`Storage::close`] managed: whether the directory is released now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseOutcome {
    /// The store is closed and the directory lock released.
    Released,
    /// Everything was flushed, but `refs` references (this one included) were
    /// alive, so the store stays open and the directory locked until the last
    /// of them drops.
    StillShared { refs: usize },
}

/// The block store, used as pull-through cache and layer storage: one per
/// directory, and one per process for a directory (the store holds a lock file).
pub struct Storage {
    pub(crate) store: BlockStore,
    pub(crate) catalog: Catalog,
    pub(crate) ram: RamTier,
    pub(crate) cfg: StorageConfig,
    /// Pull-through cache bookkeeping shared by every cached source.
    pub(crate) cache: CacheState,
    /// The durability gate (spec §6: every durable catalog row references
    /// durable store data). Held **shared** across every "write store data,
    /// then write the catalog row that describes it" pair: a layer commit
    /// (`FileCell::commit` and the row update), a layer file create (row, then
    /// `set_len`) and a cache fetch (for a file's first block, its row and
    /// `set_len`; then `write_blocks`, then the access-log update a later row
    /// commit persists). Held **exclusive** across `store.flush()` +
    /// `catalog.commit_durable()` wherever that pair runs: a layer's durable
    /// point, `delete_layer`, `close` and reconciliation. So no row can land
    /// between a flush and the durable commit that would publish it ahead of
    /// its data. (The store's own auto-flush and compaction commits can still
    /// make a store state durable mid-commit; reconciliation repairs those.)
    ///
    /// **Lock order**, outermost first:
    /// - layers: a file cell's `state` → `gate` → the layer's `ns` → the
    ///   layer's leaf locks (`cells`, `handles`, `fresh`, a cell's `path`
    ///   and `mtime_override`) and the storage's `doomed`;
    /// - the `layers` registry → `gate` (a new layer is made durable while
    ///   the registry is held; nothing holding the gate takes the registry);
    /// - cache: `gate` → `open_counts` → `access`.
    ///
    /// The gate is never taken recursively (shared or exclusive) by a thread
    /// that holds it. The exclusive holder takes nothing else during the
    /// fsyncs (a durable point takes `doomed` only briefly, before them, and
    /// no layer lock at all).
    pub(crate) gate: RwLock<()>,
    /// Every layer with a provider, by name: live, or dropped and still
    /// inside its `Drop` (a last commit and durable point). A provider removes
    /// its own entry at the end of its `Drop` and signals `layers_gone`. One
    /// provider per layer, so all of a layer's handles share one namespace
    /// lock and one file state per GUID.
    pub(crate) layers: Mutex<HashMap<String, Weak<LayerProvider>>>,
    /// Signalled whenever a provider leaves `layers`.
    pub(crate) layers_gone: Condvar,
    /// What reconciliation at open repaired.
    pub(crate) reconciled: ReconcileReport,
    /// When durable points happen, under [`Durability::Deferred`].
    pub(crate) clock: DurableClock,
    /// GUIDs of layer files (any layer's) whose rows are gone, durably or
    /// not, and that no handle has open: deleted from the store by the next
    /// durable point, after its catalog commit. A leaf lock, pushed to under
    /// a layer's `ns` right after the row's removal committed.
    pub(crate) doomed: Mutex<Vec<Guid>>,
    /// Test hook: `import_layer` fails when it reaches this layer path.
    #[cfg(test)]
    pub(crate) fail_import_at: Mutex<Option<String>>,
    /// Test hook: run by the next `LayerProvider::drop`, after its durable
    /// point and before it leaves `layers`.
    #[cfg(test)]
    #[allow(clippy::type_complexity)]
    pub(crate) drop_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Test hook: runs inside every layer file read, while the read holds the
    /// file's state lock (so a test can hold reads there and count them).
    #[cfg(test)]
    #[allow(clippy::type_complexity)]
    pub(crate) layer_read_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Test hook: runs inside a layer file read's RAM-tier fill, after the
    /// block was read from the store and before it is put into the tier
    /// (still under the file's state lock).
    #[cfg(test)]
    #[allow(clippy::type_complexity)]
    pub(crate) layer_fill_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

/// The default for [`DurableClock::max_commits`]
/// ([`StorageConfig::max_deferred_commits`]).
pub(crate) const DEFERRED_MAX_COMMITS: u64 = 10_000;

/// When durable points happen, for [`Durability::Deferred`].
pub(crate) struct DurableClock {
    /// When the last durable point completed (or, while one is claimed as
    /// due, when it was claimed).
    last: Mutex<Instant>,
    /// Durable points completed since open, counting ones that found nothing
    /// to make durable. Read by a layer file create under the shared gate
    /// and bumped under the exclusive gate, so a file whose create saw the
    /// current epoch has a row that no durable point has published yet.
    epoch: AtomicU64,
    /// A deferred change finds a durable point due once the catalog holds
    /// this many non-durable commits (redb keeps their bookkeeping in memory
    /// until a durable commit), whatever `max_interval` says.
    max_commits: AtomicU64,
    /// Test hook: time added to the real clock.
    #[cfg(test)]
    skew: Mutex<Duration>,
    /// Test hook: durable points that fsynced, since open.
    #[cfg(test)]
    points: AtomicU64,
}

impl DurableClock {
    fn new(max_commits: u64) -> Self {
        DurableClock {
            last: Mutex::new(Instant::now()),
            epoch: AtomicU64::new(0),
            max_commits: AtomicU64::new(max_commits.max(1)),
            #[cfg(test)]
            skew: Mutex::new(Duration::ZERO),
            #[cfg(test)]
            points: AtomicU64::new(0),
        }
    }

    fn now(&self) -> Instant {
        #[cfg(test)]
        return Instant::now() + *lock(&self.skew);
        #[cfg(not(test))]
        Instant::now()
    }

    /// The current epoch (see [`DurableClock::epoch`]).
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    /// If the last durable point is at least `max` old, or `commits`
    /// non-durable catalog commits have piled up, claims the next durable
    /// point (restarting the interval) and returns true.
    fn claim_if_due(&self, max: Duration, commits: u64) -> bool {
        let now = self.now();
        let mut last = lock(&self.last);
        if now.saturating_duration_since(*last) >= max
            || commits >= self.max_commits.load(Ordering::Acquire)
        {
            *last = now;
            true
        } else {
            false
        }
    }

    /// A claimed durable point failed: the next change tries again.
    pub(crate) fn retry(&self) {
        let now = self.now();
        let mut last = lock(&self.last);
        // An `Instant` that far back may not exist (early after boot): then
        // the next change waits `max_interval` again, as after open.
        *last = now
            .checked_sub(Duration::from_secs(365 * 24 * 3600))
            .unwrap_or(*last);
    }

    /// A durable point completed (`fsynced`), or found nothing to make
    /// durable. Under the exclusive gate.
    fn reached(&self, fsynced: bool) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        *lock(&self.last) = self.now();
        #[cfg(test)]
        if fsynced {
            self.points.fetch_add(1, Ordering::AcqRel);
        }
        #[cfg(not(test))]
        let _ = fsynced;
    }

    /// Test hook: moves this clock `by` into the future.
    #[cfg(test)]
    pub(crate) fn advance(&self, by: Duration) {
        *lock(&self.skew) += by;
    }

    /// Test hook: durable points that fsynced, since open.
    #[cfg(test)]
    pub(crate) fn points(&self) -> u64 {
        self.points.load(Ordering::Acquire)
    }

    /// Test hook: sets [`DurableClock::max_commits`].
    #[cfg(test)]
    pub(crate) fn set_max_commits(&self, n: u64) {
        self.max_commits.store(n, Ordering::Release);
    }
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
        let catalog_path = dir.join("catalog.redb");
        let catalog = Catalog::open(&catalog_path)?;
        // Spec §6: repair what a crash between the two halves' commits left,
        // before the cache budget is summed and before any provider exists.
        let gate = RwLock::new(());
        let reconciled = reconcile(
            &store,
            &catalog,
            &catalog_path,
            &gate,
            u64::from(cfg.store.block_size),
        )?;
        let ram = RamTier::with_geometry(cfg.ram_tier_bytes, u64::from(cfg.store.block_size));
        let cached_logical = catalog
            .cache_all()?
            .iter()
            .map(|(_, r)| r.logical_bytes)
            .sum();
        let clock = DurableClock::new(cfg.max_deferred_commits);
        Ok(Arc::new(Storage {
            store,
            catalog,
            ram,
            cfg,
            cache: CacheState::new(cached_logical),
            gate,
            layers: Mutex::new(HashMap::new()),
            layers_gone: Condvar::new(),
            reconciled,
            clock,
            doomed: Mutex::new(Vec::new()),
            #[cfg(test)]
            fail_import_at: Mutex::new(None),
            #[cfg(test)]
            drop_hook: Mutex::new(None),
            #[cfg(test)]
            layer_read_hook: Mutex::new(None),
            #[cfg(test)]
            layer_fill_hook: Mutex::new(None),
        }))
    }

    /// Waits for a background eviction, runs [`Storage::sync`] (batched cache
    /// access times, then the store flush, then the catalog's durable commit —
    /// in that order, so every durable catalog row references durable store
    /// data — then live layers' deferred deletions), then closes the store and
    /// releases the directory.
    ///
    /// If other references to this `Storage` are still alive, everything is
    /// flushed the same way and `Ok(StillShared)` is returned, but the store
    /// stays open and the directory stays locked until the last reference
    /// drops (the store closes itself on drop); a warning is logged. Every
    /// layer provider and cached source holds such a reference, so a caller
    /// that must reopen the directory (in this process or another) drops
    /// those first.
    pub fn close(self: Arc<Self>) -> Result<CloseOutcome, StorageError> {
        // A background eviction holds a reference; let it finish.
        self.wait_for_eviction();
        self.sync()?;
        match Arc::try_unwrap(self) {
            Ok(s) => {
                let Storage { store, catalog, .. } = s;
                store.close()?;
                drop(catalog);
                Ok(CloseOutcome::Released)
            }
            Err(still_shared) => {
                let refs = Arc::strong_count(&still_shared);
                tracing::warn!(
                    refs,
                    "Storage::close with other references alive: flushed, but the \
                     directory stays locked until the last one drops"
                );
                Ok(CloseOutcome::StillShared { refs })
            }
        }
    }

    /// The durability gate, shared: see [`Storage::gate`].
    pub(crate) fn gate_shared(&self) -> RwLockReadGuard<'_, ()> {
        self.gate.read().unwrap_or_else(|e| e.into_inner())
    }

    /// The durability gate, exclusive: see [`Storage::gate`].
    pub(crate) fn gate_exclusive(&self) -> RwLockWriteGuard<'_, ()> {
        self.gate.write().unwrap_or_else(|e| e.into_inner())
    }

    /// A durable point: [`Storage::durable_point`]. Safe with the `layers`
    /// registry held.
    pub(crate) fn flush_durably(&self) -> Result<(), StorageError> {
        self.durable_point()
    }

    /// Store flush, then the durable catalog commit, then the store deletes
    /// that commit made safe (every layer's removed or replaced files that no
    /// handle has open, see [`Storage::doomed`]).
    ///
    /// The flush and the commit run under the exclusive durability gate
    /// ([`Storage::gate`]), so no layer's or cache's row can land between
    /// them ahead of its data. Every GUID in the doomed list had its row
    /// removal committed before it was pushed, so the commit makes the
    /// removal durable before the store delete (spec §6). GUIDs doomed later
    /// wait for the next durable point.
    ///
    /// When neither the store nor the catalog holds anything non-durable,
    /// the fsyncs are skipped (the doomed files' removals are then already
    /// durable, and they are deleted all the same).
    pub(crate) fn durable_point(&self) -> Result<(), StorageError> {
        let doomed = {
            let _gate = self.gate_exclusive();
            let doomed = std::mem::take(&mut *lock(&self.doomed));
            let fsync = self.store.has_unflushed() || self.catalog.unflushed_commits() > 0;
            if fsync {
                let flushed = self
                    .store
                    .flush()
                    .map_err(StorageError::from)
                    .and_then(|()| self.catalog.commit_durable());
                if let Err(e) = flushed {
                    lock(&self.doomed).extend(doomed);
                    return Err(e);
                }
            }
            self.clock.reached(fsync);
            doomed
        };
        for g in doomed {
            let id = crate::ids::layer_file_id(&g);
            self.ram.invalidate_file(&id);
            match self.store.delete(&id) {
                Ok(()) | Err(vfs_block_store::Error::NotFound) => {}
                // Left for reconciliation, which deletes unreferenced ids.
                Err(e) => tracing::warn!(error = %e, "layer file delete failed"),
            }
        }
        Ok(())
    }

    /// Makes everything written so far durable: commits the cache's batched
    /// access times, then runs one durable point (store flush, then the
    /// catalog's durable commit), which covers every layer and the cache and
    /// deletes the store data of removed or replaced layer files that were
    /// waiting for it. Skips the fsyncs when nothing changed since the last
    /// durable point. Under [`Durability::Deferred`] this is how a host makes
    /// a batch of writes durable without waiting for `max_interval`; under
    /// either policy it is what [`Storage::close`] does first.
    pub fn sync(&self) -> Result<(), StorageError> {
        self.commit_access()?;
        self.durable_point()
    }

    /// Under [`Durability::Deferred`], called after a layer change that
    /// [`Durability::OnEveryClose`] would make durable at once: whether a
    /// durable point is due (the last one is at least `max_interval` old, or
    /// the catalog holds [`DurableClock::max_commits`] non-durable commits).
    /// A due point is claimed by the caller, so concurrent writers do not
    /// all run one; if it fails, the caller calls [`DurableClock::retry`].
    pub(crate) fn deferred_point_due(&self, max_interval: Duration) -> bool {
        self.clock
            .claim_if_due(max_interval, self.catalog.unflushed_commits())
    }

    /// The block store's block size in bytes.
    pub fn block_size(&self) -> u64 {
        u64::from(self.cfg.store.block_size)
    }

    /// The layer named `name` as a read-write provider, creating the layer if
    /// it does not exist. While a provider for it is alive, every call returns
    /// that same provider; while the last one is still being dropped, the call
    /// waits for that drop to finish rather than build a second one.
    pub fn layer(self: &Arc<Self>, name: &str) -> Result<Arc<dyn Provider>, StorageError> {
        Ok(self.layer_provider(name, true)?)
    }

    /// The provider of layer `name`, shared as [`Storage::layer`] describes.
    /// A missing layer is created when `create`, else `NoSuchLayer`.
    pub(crate) fn layer_provider(
        self: &Arc<Self>,
        name: &str,
        create: bool,
    ) -> Result<Arc<LayerProvider>, StorageError> {
        let mut layers = lock(&self.layers);
        while let Some(w) = layers.get(name) {
            if let Some(live) = w.upgrade() {
                return Ok(live);
            }
            // Its last reference is gone but its `Drop` is still committing.
            layers = self
                .layers_gone
                .wait(layers)
                .unwrap_or_else(|e| e.into_inner());
        }
        let id = match self.catalog.layer_id(name)? {
            Some(id) => id,
            None if create => self.create_layer_durably(name)?,
            None => return Err(StorageError::NoSuchLayer(name.to_owned())),
        };
        let p = Arc::new(LayerProvider::new(Arc::clone(self), name.to_owned(), id));
        layers.insert(name.to_owned(), Arc::downgrade(&p));
        Ok(p)
    }

    /// [`LayerProvider::put_files`] on layer `name` (created if missing):
    /// many whole files in one block-store and one catalog commit. The
    /// status is a `vfs_provider` status, as a layer operation's.
    pub fn put_files(self: &Arc<Self>, name: &str, files: &[(&str, &[u8])]) -> Result<(), i32> {
        let layer = self.layer_provider(name, true).map_err(|e| e.to_status())?;
        layer.put_files(files)
    }

    /// Creates layer `name` and makes it durable before any of its data can
    /// be written, so the store never holds layer data under a catalog that
    /// has no durable layer (which [`Storage::open`] refuses as a lost
    /// catalog). May be called with the `layers` registry lock held.
    pub(crate) fn create_layer_durably(&self, name: &str) -> Result<u64, StorageError> {
        let id = self.catalog.create_layer(name)?;
        self.flush_durably()?;
        Ok(id)
    }

    /// Called at the end of `LayerProvider::drop`: removes `p`'s registry
    /// entry (if it is still `p`'s) and wakes waiters.
    pub(crate) fn layer_dropped(&self, name: &str, p: *const LayerProvider) {
        let mut layers = lock(&self.layers);
        if layers
            .get(name)
            .is_some_and(|w| std::ptr::eq(w.as_ptr(), p))
        {
            layers.remove(name);
        }
        drop(layers);
        self.layers_gone.notify_all();
    }

    /// The durability policy this storage was opened with.
    pub(crate) fn durability(&self) -> Durability {
        self.cfg.durability
    }

    /// The names of the layers with a provider, sorted: live, or dropped but
    /// still finishing its last commit.
    pub fn layers_in_use(&self) -> Vec<String> {
        let mut names: Vec<String> = lock(&self.layers).keys().cloned().collect();
        names.sort();
        names
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
        let e = Storage::open(dir.path(), StorageConfig::default())
            .err()
            .expect("the block store lock must hold");
        assert!(e.is_locked(), "{e}");
        assert!(!StorageError::Catalog("x".into()).is_locked());
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
        assert_eq!(s.close().unwrap(), CloseOutcome::StillShared { refs: 2 });
        assert!(Storage::open(dir.path(), StorageConfig::default()).is_err());
        drop(other);
        let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
        assert_eq!(s.close().unwrap(), CloseOutcome::Released);
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
        assert_eq!(
            StorageError::NotFound("x".into()).to_status(),
            vfs_provider::ST_NOT_FOUND
        );
        assert_eq!(
            StorageError::NotEmpty("x".into()).to_status(),
            vfs_provider::ST_IS_DIR
        );
        assert_eq!(
            StorageError::Exists("x".into()).to_status(),
            vfs_provider::ST_EXISTS
        );
        assert_eq!(
            StorageError::BadRequest("x".into()).to_status(),
            vfs_provider::ST_BAD_REQUEST
        );
    }
}

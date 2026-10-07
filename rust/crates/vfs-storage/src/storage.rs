//! [`Storage`]: one block store, its catalog and the RAM tier, opened together.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock, Weak};

use vfs_block_store::BlockStore;
use vfs_provider::Provider;

use crate::cached::CacheState;
use crate::util::lock;
use crate::catalog::Catalog;
use crate::config::{Durability, StorageConfig};
use crate::durable::{fold_scratch_dirs, DurableClock};
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
    /// The durability gate (spec §6): shared around each "write store data,
    /// then the catalog row that describes it" pair, exclusive around a flush
    /// plus durable commit. What it guards, who takes it how, and the **lock
    /// order** are in `rust/docs/durability.md` (module `crate::durable`).
    pub(crate) gate: RwLock<()>,
    /// Every layer with a provider, by name: live, or dropped and still
    /// inside its `Drop` (a last commit and durable point). A provider removes
    /// its own entry at the end of its `Drop` and signals `layers_gone`. One
    /// provider per layer, so all of a layer's handles share one namespace
    /// lock and one file state per GUID.
    pub(crate) layers: Mutex<HashMap<String, Weak<LayerProvider>>>,
    /// Signalled whenever a provider leaves `layers`.
    pub(crate) layers_gone: Condvar,
    /// What reconciliation at open repaired, or that it was skipped after a
    /// clean close.
    pub(crate) reconciled: ReconcileReport,
    /// Set once a clean close has been tried (by [`Storage::close`] or the
    /// drop), or, in tests, once a crash is simulated: the drop then does
    /// nothing more.
    pub(crate) shut: AtomicBool,
    /// Set by [`Storage::crash_on_drop_for_tests`]: the process is "dead". No
    /// durable point runs from then on, so a layer provider dropped after the
    /// crash does not publish what the crash should have lost.
    #[cfg(any(test, feature = "test-hooks"))]
    pub(crate) crashed: AtomicBool,
    /// Set when this session left something for reconciliation at the next
    /// open (see [`Storage::needs_reconcile`]): the close then leaves no
    /// clean-close mark.
    pub(crate) dirty: AtomicBool,
    /// When durable points happen, under [`Durability::Deferred`].
    pub(crate) clock: DurableClock,
    /// `StorageConfig::scratch_dirs`, folded once at open: `(layer, dir)`.
    pub(crate) scratch: Vec<(String, String)>,
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
    /// Test hook: every [`Storage::store_delete`] fails while set.
    #[cfg(test)]
    pub(crate) fail_deletes: AtomicBool,
    /// Test hook: run by a clean close after the block store's shutdown and
    /// before the catalog's clean-close mark.
    #[cfg(test)]
    #[allow(clippy::type_complexity)]
    pub(crate) before_mark_hook: Mutex<Option<Box<dyn FnOnce(&Storage) + Send>>>,
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
    /// The directory as a crash left it, put back by the drop. **Declared
    /// last, so it drops after every field above has released its files**: see
    /// [`CrashImage`](crate::test_util::CrashImage).
    #[cfg(all(any(test, feature = "test-hooks"), not(windows)))]
    pub(crate) crash_image: Mutex<Option<crate::test_util::CrashImage>>,
    #[cfg(all(any(test, feature = "test-hooks"), not(windows)))]
    pub(crate) dir: std::path::PathBuf,
}

impl Storage {
    /// Opens (creating if needed) the storage in `dir`: the block store in `dir`
    /// itself, the catalog at `dir/catalog.redb`, and an empty RAM tier.
    ///
    /// Fails with `Store(Locked)` while another `Storage` (in any process) has
    /// the directory open.
    ///
    /// After a clean close ([`Storage::close`], or the drop of the last
    /// reference) reconciliation (a lookup per file, slow on a large store) is
    /// skipped and [`Storage::last_reconcile`] says so. The close-token
    /// handshake that makes this safe is in `rust/docs/durability.md`.
    pub fn open(dir: impl AsRef<Path>, cfg: StorageConfig) -> Result<Arc<Storage>, StorageError> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)?;
        // The store first: it takes the directory lock, so a second opener
        // fails here, before it opens (and waits on) the catalog database.
        let store = BlockStore::open(dir, cfg.store.clone())?;
        let catalog_path = dir.join("catalog.redb");
        // A catalog created by this open holds no token, but a skip must
        // never stand in for reconciliation's refusal of a lost catalog.
        let catalog_existed = catalog_path.exists();
        let catalog = Catalog::open_with_cache(&catalog_path, cfg.catalog_cache_bytes)?;
        // The first write: from here on, a crash leaves no mark.
        let marked = catalog.take_clean_close()?;
        // The store's clean-shutdown token is the one its last close left
        // (any open since, by any build, replaced it), so a match means the
        // catalog and the store were closed together and not opened since.
        let clean = catalog_existed && marked.is_some() && marked == store.clean_shutdown_token();
        let gate = RwLock::new(());
        let reconciled = if clean {
            ReconcileReport {
                skipped_after_clean_close: true,
                ..ReconcileReport::default()
            }
        } else {
            if marked.is_some() {
                tracing::warn!(
                    store_clean = store.opened_after_clean_shutdown(),
                    "the catalog's clean-close mark does not match the block store's; reconciling"
                );
            }
            // Spec §6: repair what a crash between the two halves' commits
            // left, before the cache budget is summed and before any
            // provider exists.
            reconcile(
                &store,
                &catalog,
                &catalog_path,
                &gate,
                u64::from(cfg.store.block_size),
            )?
        };
        let ram = RamTier::with_geometry(cfg.ram_tier_bytes, u64::from(cfg.store.block_size));
        let cached_logical = catalog
            .cache_all()?
            .iter()
            .map(|(_, r)| r.logical_bytes)
            .sum();
        let clock = DurableClock::new(cfg.max_deferred_commits);
        let scratch = fold_scratch_dirs(&cfg.scratch_dirs);
        // Corruption found, or a repair that failed: the next open reports
        // and retries it, as before.
        let dirty = !reconciled.corrupt_files.is_empty() || !reconciled.failed_repairs.is_empty();
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
            shut: AtomicBool::new(false),
            #[cfg(any(test, feature = "test-hooks"))]
            crashed: AtomicBool::new(false),
            dirty: AtomicBool::new(dirty),
            clock,
            scratch,
            doomed: Mutex::new(Vec::new()),
            #[cfg(test)]
            fail_import_at: Mutex::new(None),
            #[cfg(test)]
            drop_hook: Mutex::new(None),
            #[cfg(test)]
            fail_deletes: AtomicBool::new(false),
            #[cfg(test)]
            before_mark_hook: Mutex::new(None),
            #[cfg(test)]
            layer_read_hook: Mutex::new(None),
            #[cfg(test)]
            layer_fill_hook: Mutex::new(None),
            #[cfg(all(any(test, feature = "test-hooks"), not(windows)))]
            crash_image: Mutex::new(None),
            #[cfg(all(any(test, feature = "test-hooks"), not(windows)))]
            dir: dir.to_path_buf(),
        }))
    }

    /// Waits for a background eviction, runs [`Storage::sync`] (batched cache
    /// access times, then the store flush, then the catalog's durable commit —
    /// in that order, so every durable catalog row references durable store
    /// data — then live layers' deferred deletions), then closes the store and
    /// releases the directory.
    ///
    /// When this is the last reference, the close is **clean**: after the
    /// block store's final durable commit, the catalog records durably that
    /// the two agree, and the next [`Storage::open`] skips reconciliation —
    /// unless this session left something for it ([`Storage::needs_reconcile`]).
    /// Dropping the last reference does the same (logging errors), unless the
    /// thread is panicking. A process that exits without either (say through
    /// `std::process::exit`, which runs no destructors) leaves no mark, and
    /// its next open reconciles, as after a crash.
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
                s.close_cleanly()?;
                // The drop finds `shut` set: it only closes the files.
                drop(s);
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

    /// The block store's delete of a removed layer file, with the test hook
    /// applied.
    pub(crate) fn store_delete(&self, id: &[u8]) -> Result<(), vfs_block_store::Error> {
        #[cfg(test)]
        if self.fail_deletes.load(Ordering::SeqCst) {
            return Err(vfs_block_store::Error::Io(std::io::Error::other(
                "injected delete failure",
            )));
        }
        self.store.delete(id)
    }

    /// Test hook: what a crash right after a [`Storage::sync`] leaves. The
    /// sync runs, then this reference is dropped without the clean-close
    /// mark, so the next open reconciles (the block store still shuts down
    /// cleanly, as a drop does).
    #[cfg(test)]
    pub(crate) fn close_unclean(self: Arc<Self>) {
        self.wait_for_eviction();
        self.sync().unwrap();
        self.shut.store(true, Ordering::Release);
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
        self.durable_point()?;
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

impl Drop for Storage {
    /// The last reference is gone, so nothing else can write: a clean close
    /// (see [`Storage::close`]), unless one was already tried or this thread
    /// is panicking (whatever it was doing may be half done: the next open
    /// reconciles). It runs a sync and up to three fsyncs on whichever thread
    /// drops the last reference (possibly a background eviction's); a process
    /// that exits before it finishes leaves no mark, which is safe. It never
    /// panics out: a panic inside is caught and logged.
    fn drop(&mut self) {
        if self.shut.load(Ordering::Acquire) || std::thread::panicking() {
            return;
        }
        let this = std::panic::AssertUnwindSafe(&*self);
        match std::panic::catch_unwind(move || this.close_cleanly()) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(
                error = %e,
                "closing the storage cleanly failed; the next open reconciles"
            ),
            Err(_) => tracing::error!("closing the storage panicked; the next open reconciles"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StorageConfig;

    /// One closed file in layer `l`, written under the default (deferred)
    /// policy, so no durable point has run for it; then `end` ends the session.
    /// Whether the file is there after a reopen.
    fn survives(end: impl FnOnce(Arc<Storage>, Arc<dyn vfs_provider::Provider>)) -> bool {
        use vfs_provider::{VPath, OPEN_CREATE, OPEN_WRITE};
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
        let p = s.layer("l").unwrap();
        let (h, _, _) = p
            .open(VPath::at_default("f.bin"), OPEN_WRITE | OPEN_CREATE)
            .unwrap();
        p.write_at(h, 0, b"unsynced").unwrap();
        p.close(h).unwrap();
        end(s, p);
        let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
        let p = s.layer("l").unwrap();
        p.getattr(VPath::at_default("f.bin")).unwrap().is_some()
    }

    #[test]
    fn dropping_the_layer_provider_after_a_crash_makes_nothing_durable() {
        // Control: the same drops without the crash publish the write.
        assert!(survives(|s, p| {
            drop(p);
            drop(s);
        }));
        // The crash comes first, then the drops that would have published it.
        assert!(!survives(|s, p| {
            s.crash_on_drop_for_tests();
            drop(p);
            drop(s);
        }));
    }

    #[test]
    fn a_sync_after_a_crash_makes_nothing_durable() {
        assert!(!survives(|s, p| {
            s.crash_on_drop_for_tests();
            s.sync().unwrap();
            drop(p);
            drop(s);
        }));
    }

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

    /// A store with one layer file, closed by `close`.
    fn closed_store_with_a_file(dir: &Path) {
        let s = Storage::open(dir, StorageConfig::default()).unwrap();
        s.put_files("l", &[("a/b.txt", b"kept")]).unwrap();
        assert_eq!(s.close().unwrap(), CloseOutcome::Released);
    }

    fn read_b(s: &Arc<Storage>) -> Vec<u8> {
        let p = s.layer("l").unwrap();
        let (h, size, _) = p
            .open(
                vfs_provider::VPath::at_default("a/b.txt"),
                vfs_provider::OPEN_READ,
            )
            .unwrap();
        let mut buf = vec![0u8; size as usize];
        assert_eq!(p.read_at(h, 0, &mut buf).unwrap(), buf.len());
        p.close(h).unwrap();
        buf
    }

    /// An orphan store file: what reconciliation would delete.
    fn plant_orphan(s: &Storage) -> [u8; 17] {
        let id = crate::ids::layer_file_id(&crate::ids::new_guid());
        s.store.set_len(&id, 10).unwrap();
        id
    }

    #[test]
    fn a_clean_close_skips_reconciliation_at_the_next_open() {
        let dir = tempfile::tempdir().unwrap();
        let first = Storage::open(dir.path(), StorageConfig::default()).unwrap();
        assert!(
            !first.last_reconcile().skipped_after_clean_close,
            "a new store reconciles"
        );
        first.close().unwrap();
        closed_store_with_a_file(dir.path());

        let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
        let r = s.last_reconcile().clone();
        assert_eq!(
            r,
            ReconcileReport {
                skipped_after_clean_close: true,
                ..Default::default()
            }
        );
        assert_eq!(read_b(&s), b"kept");
        // Proof that no pass ran: an orphan planted before a clean close
        // (which no write path makes) is still there after the open.
        let orphan = plant_orphan(&s);
        s.close().unwrap();
        let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
        assert!(s.last_reconcile().skipped_after_clean_close);
        assert!(s.store.stat(&orphan).unwrap().is_some());
        // Dropping the last reference is a clean close too.
        drop(s);
        let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
        assert!(s.last_reconcile().skipped_after_clean_close);
        assert_eq!(read_b(&s), b"kept");
    }

    /// A crash (no clean close) reconciles at the next open, and so does a
    /// crash of a session that itself opened after a clean close: the open
    /// removed the mark durably before anything else.
    #[test]
    fn a_crash_reconciles_even_after_a_clean_open() {
        let dir = tempfile::tempdir().unwrap();
        closed_store_with_a_file(dir.path());
        let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
        assert!(s.last_reconcile().skipped_after_clean_close);
        assert!(s.catalog.take_clean_close().unwrap().is_none(), "cleared");

        // Killed while open, after its writes reached the store but before
        // the catalog's durable point (as reconcile's kill tests).
        #[cfg(not(windows))]
        {
            let p = s.layer("l").unwrap();
            let (h, _, _) = p
                .open(
                    vfs_provider::VPath::at_default("new.bin"),
                    vfs_provider::OPEN_WRITE | vfs_provider::OPEN_CREATE,
                )
                .unwrap();
            p.write_at(h, 0, &[7u8; 100_000]).unwrap();
            s.store.flush().unwrap();
            let killed = tempfile::tempdir().unwrap();
            crate::test_util::snapshot_as_killed(dir.path(), killed.path()).unwrap();
            p.close(h).unwrap();
            drop(p);
            let k = Storage::open(killed.path(), StorageConfig::default()).unwrap();
            let r = k.last_reconcile();
            assert!(!r.skipped_after_clean_close, "{r:?}");
            assert!(r.orphans_deleted >= 1, "{r:?}");
            assert_eq!(read_b(&k), b"kept");
        }

        // A crash right after a sync: the next open reconciles.
        let orphan = plant_orphan(&s);
        s.close_unclean();
        let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
        let r = s.last_reconcile();
        assert!(!r.skipped_after_clean_close);
        assert_eq!(r.orphans_deleted, 1);
        assert!(s.store.stat(&orphan).unwrap().is_none());
        assert_eq!(read_b(&s), b"kept");
    }

    /// A store from before the mark existed (a catalog without it)
    /// reconciles, and its clean close then lets the next open skip.
    #[test]
    fn a_store_without_the_mark_reconciles() {
        let dir = tempfile::tempdir().unwrap();
        closed_store_with_a_file(dir.path());
        {
            let c = Catalog::open(&dir.path().join("catalog.redb")).unwrap();
            assert!(c.take_clean_close().unwrap().is_some());
        }
        let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
        assert_eq!(*s.last_reconcile(), ReconcileReport::default());
        assert_eq!(read_b(&s), b"kept");
        s.close().unwrap();
        let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
        assert!(s.last_reconcile().skipped_after_clean_close);
    }

    /// The mark is the token the block store's clean shutdown also holds:
    /// if anything opened the store after the clean close (any open replaces
    /// the store's token), or the catalog is an older copy, the open
    /// reconciles.
    #[test]
    fn a_mark_that_does_not_match_the_store_reconciles() {
        let dir = tempfile::tempdir().unwrap();
        closed_store_with_a_file(dir.path());
        let cat = dir.path().join("catalog.redb");
        let old_catalog = dir.path().join("catalog.old");
        std::fs::copy(&cat, &old_catalog).unwrap();
        {
            // Another program opens the store alone and writes to it.
            let store = BlockStore::open(dir.path(), StorageConfig::default().store).unwrap();
            store
                .set_len(&crate::ids::layer_file_id(&crate::ids::new_guid()), 5)
                .unwrap();
            store.close().unwrap();
        }
        let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
        let r = s.last_reconcile();
        assert!(!r.skipped_after_clean_close);
        assert_eq!(r.orphans_deleted, 1);
        s.close().unwrap();

        // A catalog restored from before that close.
        std::fs::rename(&old_catalog, &cat).unwrap();
        let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
        assert!(!s.last_reconcile().skipped_after_clean_close);
        assert_eq!(read_b(&s), b"kept");
    }

    /// A catalog mark with a token other than the store's reconciles.
    #[test]
    fn a_mismatched_token_reconciles() {
        let dir = tempfile::tempdir().unwrap();
        closed_store_with_a_file(dir.path());
        {
            let c = Catalog::open(&dir.path().join("catalog.redb")).unwrap();
            let t = c.take_clean_close().unwrap().unwrap();
            assert!(t > 1, "a token, never 0 or 1: {t}");
            c.mark_clean_close(t ^ 0x10).unwrap();
        }
        let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
        assert!(!s.last_reconcile().skipped_after_clean_close);
        assert_eq!(read_b(&s), b"kept");
    }

    /// What a build from before the token does with the store after a clean
    /// close: its block store open writes 0 over the token, and its clean
    /// shutdown (a `close`, or the drop of a storage it never closed, which
    /// loses the catalog's non-durable rows) writes 1. Either way the next
    /// open reconciles.
    #[test]
    fn a_session_by_an_older_build_reconciles() {
        for via_drop in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            closed_store_with_a_file(dir.path());
            let orphan = crate::ids::layer_file_id(&crate::ids::new_guid());
            {
                // The old `Storage`: a block store and a catalog, with the
                // old block store's clean shutdown (`close`, or its drop:
                // both write 1).
                let store = BlockStore::open(dir.path(), StorageConfig::default().store).unwrap();
                assert!(store.clean_shutdown_token().is_some());
                let catalog = Catalog::open(&dir.path().join("catalog.redb")).unwrap();
                // A file created, its row never made durable.
                store.set_len(&orphan, 5).unwrap();
                if via_drop {
                    drop(catalog);
                    drop(store);
                } else {
                    store.flush().unwrap();
                    catalog.commit_durable().unwrap();
                    store.close().unwrap();
                }
            }
            let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
            let r = s.last_reconcile();
            assert!(!r.skipped_after_clean_close, "via drop: {via_drop}");
            assert_eq!(r.orphans_deleted, 1, "via drop: {via_drop}");
            assert!(s.store.stat(&orphan).unwrap().is_none());
            assert_eq!(read_b(&s), b"kept");
        }
    }

    /// A write that panics while holding the durability gate shared (every
    /// write pair does; a reader's panic does not poison the lock) may be
    /// half done: the close leaves no mark. So does a panic in a durable
    /// point (exclusive, which poisons it).
    #[test]
    fn a_panic_under_the_gate_leaves_no_mark() {
        for exclusive in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            closed_store_with_a_file(dir.path());
            let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
            let orphan = plant_orphan(&s); // what the half-done pair left
            let s2 = Arc::clone(&s);
            std::thread::spawn(move || {
                if exclusive {
                    let _gate = s2.gate_exclusive();
                    panic!("mid-durable-point");
                }
                let _gate = s2.gate_shared();
                panic!("mid-write");
            })
            .join()
            .unwrap_err();
            assert_eq!(s.close().unwrap(), CloseOutcome::Released);
            let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
            let r = s.last_reconcile();
            assert!(!r.skipped_after_clean_close, "exclusive: {exclusive}");
            assert_eq!(r.orphans_deleted, 1, "exclusive: {exclusive}");
            assert!(s.store.stat(&orphan).unwrap().is_none());
            assert_eq!(read_b(&s), b"kept");
        }
    }

    /// A store delete that fails in a session (a removed layer file's, or a
    /// deleted layer's) leaves an orphan for reconciliation: the clean close
    /// leaves no mark, and the next open deletes it.
    #[test]
    fn a_failed_delete_leaves_no_mark() {
        for whole_layer in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            closed_store_with_a_file(dir.path());
            let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
            s.put_files("gone", &[("x.bin", b"doomed")]).unwrap();
            let lid = s.catalog.layer_id("gone").unwrap().unwrap();
            let id = crate::ids::layer_file_id(&s.catalog.get(lid, "x.bin").unwrap().unwrap().guid);
            s.sync().unwrap();
            s.fail_deletes.store(true, Ordering::SeqCst);
            if whole_layer {
                s.delete_layer("gone").unwrap();
            } else {
                let p = s.layer("gone").unwrap();
                p.remove(vfs_provider::VPath::at_default("x.bin")).unwrap();
                drop(p);
                s.sync().unwrap();
            }
            s.fail_deletes.store(false, Ordering::SeqCst);
            assert!(s.store.stat(&id).unwrap().is_some(), "the delete failed");
            assert_eq!(s.close().unwrap(), CloseOutcome::Released);
            let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
            let r = s.last_reconcile();
            assert!(!r.skipped_after_clean_close, "whole layer: {whole_layer}");
            assert_eq!(r.orphans_deleted, 1, "whole layer: {whole_layer}");
            assert!(s.store.stat(&id).unwrap().is_none());
            assert_eq!(read_b(&s), b"kept");
            // That open's close is clean again.
            s.close().unwrap();
            let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
            assert!(s.last_reconcile().skipped_after_clean_close);
        }
    }

    /// Killed after the block store's clean shutdown recorded the token and
    /// before the catalog's mark: the next open reconciles.
    #[cfg(not(windows))]
    #[test]
    fn a_kill_between_the_store_shutdown_and_the_mark_reconciles() {
        let dir = tempfile::tempdir().unwrap();
        closed_store_with_a_file(dir.path());
        let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
        let killed = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().to_owned(), killed.path().to_owned());
        *lock(&s.before_mark_hook) = Some(Box::new(move |_: &Storage| {
            crate::test_util::snapshot_as_killed(&from, &to).unwrap();
        }));
        s.close().unwrap();
        let k = Storage::open(killed.path(), StorageConfig::default()).unwrap();
        assert!(!k.last_reconcile().skipped_after_clean_close);
        assert_eq!(read_b(&k), b"kept");
        // The original completed its close.
        let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
        assert!(s.last_reconcile().skipped_after_clean_close);
    }

    /// An open that fails after the block store opened (here: an unreadable
    /// catalog) consumed the store's token, so the next open reconciles even
    /// with the catalog's mark back in place.
    #[test]
    fn an_open_that_fails_midway_makes_the_next_reconcile() {
        let dir = tempfile::tempdir().unwrap();
        closed_store_with_a_file(dir.path());
        let cat = dir.path().join("catalog.redb");
        let aside = dir.path().join("catalog.aside");
        std::fs::rename(&cat, &aside).unwrap();
        std::fs::write(&cat, b"not a redb database").unwrap();
        assert!(Storage::open(dir.path(), StorageConfig::default()).is_err());
        std::fs::rename(&aside, &cat).unwrap();
        let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
        assert!(!s.last_reconcile().skipped_after_clean_close);
        assert_eq!(read_b(&s), b"kept");
    }

    /// A missing catalog never skips: reconciliation refuses it as before,
    /// even right after a clean close.
    #[test]
    fn a_missing_catalog_after_a_clean_close_still_refuses() {
        let dir = tempfile::tempdir().unwrap();
        closed_store_with_a_file(dir.path());
        std::fs::remove_file(dir.path().join("catalog.redb")).unwrap();
        let e = Storage::open(dir.path(), StorageConfig::default())
            .err()
            .expect("must refuse");
        assert!(e.to_string().contains("catalog.redb"), "{e}");
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

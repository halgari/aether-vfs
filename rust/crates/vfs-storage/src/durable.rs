#![doc = include_str!("../../../docs/durability.md")]

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use vfs_core::fold;
use vfs_provider::map_io_err;

use crate::config::{Durability, ScratchDir};
use crate::ids::Guid;
use crate::layer_io::FileCell;
use crate::storage::{Storage, StorageError};
use crate::util::lock;

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
    pub(crate) fn new(max_commits: u64) -> Self {
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
    /// The clean close, by the holder of the only reference (so no provider,
    /// cached source or eviction can write any more): a last
    /// [`Storage::sync`] (cheap when [`Storage::close`] just ran one), the
    /// block store's own clean shutdown recording a fresh random token (its
    /// final durable commit, which also covers the deletions that sync made),
    /// then the same token as the catalog's clean-close mark, in one durable
    /// commit. Each step durable before the next, so a crash anywhere in
    /// between leaves no matching mark.
    ///
    /// Tried once: `shut` is set first. No mark is left (and the reason is
    /// logged) when the session is dirty ([`Storage::needs_reconcile`]: a
    /// repair left for the next open, a write that panicked, corruption found
    /// at open), when the durability gate is poisoned, or when the block
    /// store's writer is (then nothing is attempted at all: the next open
    /// reconciles).
    pub(crate) fn close_cleanly(&self) -> Result<(), StorageError> {
        if self.shut.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        if self.store.is_poisoned() {
            self.needs_reconcile("the block store's writer lock is poisoned (a write panicked)");
            tracing::warn!("storage not closed cleanly: the next open reconciles");
            return Ok(());
        }
        self.sync()?;
        if self.gate.is_poisoned() {
            self.needs_reconcile("a durable point panicked");
        }
        if self.dirty.load(Ordering::Acquire) {
            self.store.shutdown()?;
            tracing::warn!(
                "storage closed without a clean-close mark: this session left repairs \
                 for reconciliation, which the next open runs"
            );
            return Ok(());
        }
        let token = clean_close_token();
        self.store.shutdown_with_token(token)?;
        #[cfg(test)]
        if let Some(hook) = lock(&self.before_mark_hook).take() {
            hook(self);
        }
        self.catalog.mark_clean_close(token)
    }

    /// Records that this session left something only reconciliation repairs
    /// (an orphan store file, a row whose length disagrees with the store, a
    /// cache row that counts no bytes...), so the close leaves no clean-close
    /// mark and the next open reconciles. Cheap; logged once per session.
    pub(crate) fn needs_reconcile(&self, why: &str) {
        if !self.dirty.swap(true, Ordering::AcqRel) {
            tracing::warn!(
                reason = why,
                "storage left a repair for reconciliation at the next open"
            );
        }
    }

    /// Test hook for other crates (feature `test-hooks`): the process "dies"
    /// here. From now on no durable point runs, whoever asks: not a layer
    /// provider's drop, not [`Storage::sync`], not the storage's own drop, which
    /// also leaves no clean-close mark. What no durable point made durable
    /// before this call is lost and the next open reconciles.
    ///
    /// **Unix only.** The directory the storage is dropped over is put back as
    /// it was at this call, because redb publishes its non-durable commits as
    /// its database closes. On Windows the directory cannot be copied while it
    /// is open, so there only the "no durable point" part holds, and redb's
    /// drop may still publish the catalog's non-durable commits: a test of
    /// crash loss must not rely on this hook there.
    ///
    /// Panics if the copy cannot be made (the test asked for a crash and did not
    /// get one). Quiesce writers first: see
    /// [`snapshot_as_killed`](crate::snapshot_as_killed), which is also how to
    /// look at a crash while everything is still open.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn crash_on_drop_for_tests(&self) {
        // The image first: nothing after this call may reach it. The copy is
        // taken outside the lock, so a failure cannot poison it, and a
        // failure is reported here: a test that asked for a crash must not
        // go on believing it got one.
        #[cfg(not(windows))]
        if lock(&self.crash_image).is_none() {
            let image = crate::test_util::CrashImage::take(&self.dir)
                .unwrap_or_else(|e| panic!("copying the storage directory for the crash: {e}"));
            *lock(&self.crash_image) = Some(image);
        }
        self.crashed.store(true, Ordering::Release);
        self.shut.store(true, Ordering::Release);
    }

    /// The durability gate, shared: see [`Storage::gate`]. A panic while it
    /// is held may leave a write pair half done (and a reader's panic does
    /// not poison an `RwLock`), so the guard marks the session dirty then.
    pub(crate) fn gate_shared(&self) -> SharedGate<'_> {
        SharedGate {
            _guard: self.gate.read().unwrap_or_else(|e| e.into_inner()),
            storage: self,
        }
    }

    /// The durability gate, exclusive: see [`Storage::gate`].
    pub(crate) fn gate_exclusive(&self) -> RwLockWriteGuard<'_, ()> {
        self.gate.write().unwrap_or_else(|e| e.into_inner())
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
        #[cfg(any(test, feature = "test-hooks"))]
        if self.crashed.load(Ordering::Acquire) {
            return Ok(());
        }
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
            match self.store_delete(&id) {
                Ok(()) | Err(vfs_block_store::Error::NotFound) => {}
                // Left for reconciliation, which deletes unreferenced ids.
                Err(e) => {
                    tracing::warn!(error = %e, "layer file delete failed");
                    self.needs_reconcile("a removed layer file's store delete failed");
                }
            }
        }
        Ok(())
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
}

/// A shared hold of the durability gate: see [`Storage::gate_shared`].
pub(crate) struct SharedGate<'a> {
    _guard: RwLockReadGuard<'a, ()>,
    storage: &'a Storage,
}

impl Drop for SharedGate<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.storage
                .needs_reconcile("a write panicked while holding the durability gate");
        }
    }
}

/// A random clean-close token: never 0 (not clean) or 1 (a plain clean
/// shutdown, which builds before the token write).
fn clean_close_token() -> u64 {
    loop {
        let t = uuid::Uuid::new_v4().as_u64_pair().0;
        if t > 1 {
            return t;
        }
    }
}

/// A layer's files created since the last durable point (the "fresh file"
/// rule): the durability epoch ([`DurableClock::epoch`]) their creates saw,
/// and their GUIDs. A set whose epoch is not the current one is stale (a
/// durable point has published those rows since) and counts as empty.
pub(crate) struct FreshFiles(Mutex<(u64, HashSet<Guid>)>);

impl FreshFiles {
    pub(crate) fn new() -> Self {
        FreshFiles(Mutex::new((0, HashSet::new())))
    }

    /// Records that the file `guid` was created in the current durability
    /// epoch. Under the shared gate (a create's), so no durable point runs
    /// between the row's put and this.
    pub(crate) fn created(&self, clock: &DurableClock, guid: Guid) -> Result<(), i32> {
        let epoch = clock.epoch();
        let mut fresh = self.0.lock().map_err(|_| map_io_err())?;
        if fresh.0 != epoch {
            *fresh = (epoch, HashSet::new());
        }
        fresh.1.insert(guid);
        Ok(())
    }

    /// Whether no durable point has published the row of `guid` since its
    /// create (so its whole content is still non-durable). A race with a
    /// durable point answers false, which only costs an extra one.
    fn contains(&self, clock: &DurableClock, guid: &Guid) -> bool {
        let epoch = clock.epoch();
        self.0
            .lock()
            .is_ok_and(|f| f.0 == epoch && f.1.contains(guid))
    }
}

/// [`StorageConfig::scratch_dirs`](crate::StorageConfig::scratch_dirs) with
/// each directory folded once, at open: `(layer, folded dir)`.
pub(crate) fn fold_scratch_dirs(dirs: &[ScratchDir]) -> Vec<(String, String)> {
    dirs.iter()
        .map(|d| (d.layer.clone(), fold(&d.dir)))
        .collect()
}

impl Storage {
    /// Whether `cell`'s file is in one of `layer`'s scratch directories
    /// ([`crate::StorageConfig::scratch_dirs`]): a temporary its host
    /// deletes after a crash, so a rewrite of it never needs a durable point.
    fn is_scratch(&self, layer: &str, cell: &FileCell) -> bool {
        let mut dirs = self.scratch.iter().filter(|(l, _)| l == layer).peekable();
        if dirs.peek().is_none() {
            return false;
        }
        let path = cell.path.lock().unwrap_or_else(|e| e.into_inner());
        path.as_deref()
            .and_then(|p| p.split_once('/'))
            .is_some_and(|(top, _)| dirs.any(|(_, dir)| dir == top))
    }

    /// The policy: called by a layer provider after a change that
    /// [`Durability::OnEveryClose`] makes durable before it returns. Under
    /// that policy, a [`Storage::durable_point`]. Under
    /// [`Durability::Deferred`] the change stays non-durable (and a removed
    /// file's store data stays, doomed) unless
    ///
    /// - `rewrote` names a file that was not created since the last durable
    ///   point (not in `fresh`) and is not in a scratch directory: a rewrite
    ///   in place of a file whose row is already durable runs one at once; or
    /// - a durable point is due ([`Storage::deferred_point_due`]), which is
    ///   claimed first so concurrent writers do not all run one; if it
    ///   fails, the claim is undone ([`DurableClock::retry`]).
    ///
    /// Called with no lock held.
    pub(crate) fn after_change(
        &self,
        layer: &str,
        fresh: &FreshFiles,
        rewrote: Option<&FileCell>,
    ) -> Result<(), StorageError> {
        let max_interval = match self.durability() {
            Durability::OnEveryClose => return self.durable_point(),
            Durability::Deferred { max_interval } => max_interval,
        };
        if rewrote
            .is_some_and(|c| !fresh.contains(&self.clock, &c.guid) && !self.is_scratch(layer, c))
        {
            return self.durable_point();
        }
        if !self.deferred_point_due(max_interval) {
            return Ok(());
        }
        self.durable_point().inspect_err(|_| self.clock.retry())
    }
}

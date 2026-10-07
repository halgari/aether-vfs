//! The path cache, and the two thread-local scopes that decide what may be cached: the
//! caller's [`UncachedScope`] and the OS-consult re-entry guard.

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::sync::RwLock;

use crate::RootHit;

thread_local! {
    /// Depth counter behind [`UncachedScope`]. A counter rather than a bare
    /// flag so nested or overlapping guards on the same thread compose
    /// correctly: an inner guard's `Drop` must not re-enable caching while an
    /// outer guard (from a caller further up the same call chain) is still
    /// held.
    static SUPPRESS_CACHE_DEPTH: Cell<u32> = const { Cell::new(0) };
}

pub(crate) fn cache_suppressed() -> bool {
    SUPPRESS_CACHE_DEPTH.with(|c| c.get() > 0)
}

thread_local! {
    /// Guards [`RootMap::compute_under_root`]'s OS-consult branch
    /// (`expand_short_name`) against re-entering itself.
    ///
    /// `expand_short_name` (via `vfs_win::final_path_for_open`) opens a real
    /// `CreateFileW` handle on the candidate path to ask the OS what it
    /// actually names. When this crate is consulted from inside an injected
    /// process whose own `NtCreateFile`/`NtOpenFile` are hooked (`vfs-shim`'s
    /// whole reason for existing), that `CreateFileW` call is itself
    /// intercepted and fed back through the very same decision path —
    /// `create_hook` -> `decision_for` -> `RootMap::under_root` ->
    /// `compute_under_root` — for the identical `~`-bearing path, which hits
    /// this same OS-consult branch again, which calls `expand_short_name`
    /// again, without bound. Verified by reproduction: an escape-matrix
    /// vector building an 8.3 short-name spelling of a path under a session's
    /// managed root (any temp-directory session-base name longer than 8.3,
    /// which every real session has: `vfs-daemon-<pid>-<seq>-<id>`) recursed
    /// until the injected process's stack overflowed (`STATUS_STACK_OVERFLOW`,
    /// `0xC00000FD`) — a real crash, not a misclassification, and one none of
    /// this crate's own unit tests can see, since a plain test process has no
    /// hook on `CreateFileW` for the recursion to loop through.
    ///
    /// The break: a re-entrant call finds the guard already held and skips
    /// the OS consult, answering "not recognised here" instead
    /// (`Resolution::OsConsulted(None)`). That does not lose the answer — it
    /// only refuses to ask the OS *again* for the same fact the outer call is
    /// already in the middle of asking. The re-entrant `CreateFileW`'s own
    /// hook invocation then falls through to the
    /// *real* trampoline, which is the actual, unhooked `NtCreateFile` this
    /// whole call chain was trying to reach — so `final_path_for_open`'s
    /// handle open still succeeds against the real filesystem, and the outer
    /// call's `expand_short_name` still returns the resolved long path
    /// exactly as it would have without the nested detour. Nothing is
    /// answered incorrectly; the second and every further attempt to
    /// re-derive the same fact from inside itself is simply skipped.
    static OS_CONSULT_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// RAII guard for [`OS_CONSULT_DEPTH`]. `enter()` returns `None` when the
/// guard is already held on this thread — the caller's signal to skip the OS
/// consult rather than recurse into it.
pub(crate) struct OsConsultGuard(());

impl OsConsultGuard {
    pub(crate) fn enter() -> Option<Self> {
        OS_CONSULT_DEPTH.with(|c| {
            if c.get() > 0 {
                None
            } else {
                c.set(1);
                Some(OsConsultGuard(()))
            }
        })
    }
}

impl Drop for OsConsultGuard {
    fn drop(&mut self) {
        OS_CONSULT_DEPTH.with(|c| c.set(c.get().saturating_sub(1)));
    }
}

/// While one or more of these guards is alive on the current thread, every
/// [`RootMap::under_root`] lookup made on that thread is ineligible for
/// caching — including a lookup `compute_under_root` would otherwise classify
/// as [`Resolution::Deterministic`].
///
/// This exists for one situation `compute_under_root` cannot detect on its
/// own: a caller assembling `nt_path` from something that is itself a
/// snapshot of live, mutable state — for instance `GetFinalPathNameByHandleW`
/// on a directory handle, whose current target is a fact about the
/// filesystem *now*, not a property of the resulting string's bytes.
/// `compute_under_root`'s own `~`-gated OS-consulted tracking (see
/// [`Resolution`]) only catches paths *this crate* sent to the OS itself —
/// a path a caller already resolved via its own OS query before ever handing
/// it to `RootMap` looks, from here, exactly like an ordinary literal path.
/// Caching it under its own bytes would resurrect the same staleness bug
/// `Resolution::OsConsulted` exists to prevent (an 8.3 slot reused, a
/// junction retargeted — here, a handle's target renamed or replaced mid-
/// session), just arriving from outside this crate instead of from inside
/// `compute_under_root`. The caller who knows the provenance must say so
/// explicitly, by holding this guard for the duration of every
/// `RootMap`-backed decision it makes with such a path. See `vfs-shim`'s
/// `parent_dir_of_handle` for the concrete caller-side case.
#[must_use = "the suppression ends as soon as this guard is dropped"]
pub struct UncachedScope(());

impl UncachedScope {
    pub fn enter() -> Self {
        SUPPRESS_CACHE_DEPTH.with(|c| c.set(c.get() + 1));
        UncachedScope(())
    }
}

impl Drop for UncachedScope {
    fn drop(&mut self) {
        SUPPRESS_CACHE_DEPTH.with(|c| c.set(c.get().saturating_sub(1)));
    }
}

/// Default bound on [`PathCache`]'s entry count. Generous enough to hold every
/// distinct path a game session opens (the instrumentation this gate is
/// measured against shows opens repeat heavily, not that the *distinct* set is
/// huge), while still being a hard cap so a session that runs for hours cannot
/// grow the cache without limit.
pub(crate) const DEFAULT_CACHE_CAPACITY: usize = 4096;

/// A bounded cache from a raw NT open-path string to the [`RootMap::under_root`]
/// answer for it (`None` meaning "outside/malformed").
///
/// Keyed on the exact raw string a caller spelled — not on any normalized or
/// canonical form — because the point is to avoid *repeating* the work
/// (including a possible Win32 call; see `RootMap::compute_under_root`) that
/// turns a raw spelling into that answer, and the caller's own instrumentation
/// shows the same raw spelling opened over and over during a game's load.
///
/// # Thread safety
///
/// The shim is a DLL hooking calls inside a game process: many threads call
/// `under_root` concurrently, and this cache must never become a single point
/// where every open — including a cache *hit*, the overwhelmingly common case
/// once warm — is forced to wait for every other thread's open. A `Mutex`
/// guarding one shared map would do exactly that: hits and misses alike take
/// the same exclusive lock.
///
/// Instead this uses a `RwLock`: a hit only needs a *read* lock, so any number
/// of threads can look up a cached answer at the same time without blocking
/// each other. Only a genuine miss — the first time a raw spelling is seen, or
/// one that fell out of the bound — takes the brief exclusive write lock
/// needed to insert it.
///
/// Eviction is FIFO (oldest inserted, not least-recently-used), which is a
/// deliberate trade against a "smarter" LRU: LRU needs to bump an entry's
/// recency on every *hit*, which would force hits through the write lock too
/// and defeat the entire point of using a read lock for them. FIFO needs no
/// mutation on a hit at all, at the cost of being a worse eviction policy
/// under adversarial access patterns — an acceptable trade for a cache sized
/// to comfortably hold a real session's distinct paths.
pub(crate) struct PathCache {
    capacity: usize,
    state: RwLock<PathCacheState>,
}

#[derive(Default)]
pub(crate) struct PathCacheState {
    map: HashMap<String, Option<RootHit>>,
    order: VecDeque<String>,
}

impl PathCache {
    pub(crate) fn new(capacity: usize) -> Self {
        PathCache {
            capacity: capacity.max(1),
            state: RwLock::new(PathCacheState::default()),
        }
    }

    /// A lock-poisoning thread (one that panicked while holding the lock) must
    /// not wedge every future open in a long-running game session — recover
    /// the guard rather than propagating the poison.
    pub(crate) fn get(&self, key: &str) -> Option<Option<RootHit>> {
        let guard = self.state.read().unwrap_or_else(|e| e.into_inner());
        guard.map.get(key).cloned()
    }

    pub(crate) fn insert(&self, key: String, value: Option<RootHit>) {
        let mut guard = self.state.write().unwrap_or_else(|e| e.into_inner());
        // Another thread may have raced this one to compute and insert the
        // same key; the first writer wins rather than double-counting it in
        // `order` (which would let the same key be evicted, then re-added,
        // silently exceeding the intended capacity accounting).
        if guard.map.contains_key(&key) {
            return;
        }
        if guard.order.len() >= self.capacity {
            if let Some(oldest) = guard.order.pop_front() {
                guard.map.remove(&oldest);
            }
        }
        guard.order.push_back(key.clone());
        guard.map.insert(key, value);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .len()
    }
}

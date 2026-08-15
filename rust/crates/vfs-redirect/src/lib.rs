#![forbid(unsafe_code)]

//! The injected shim's pure redirect-decision core: map an incoming NT open path
//! + a published snapshot to pass-through vs redirect-to-backing-file.

mod canon;
mod volumes;

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use vfs_core::{fold, normalize_vpath, PathError};
pub use vfs_provider::RootId;
use vfs_shared::{SnapResolution, SnapshotReader};

pub use canon::{canonicalise, split_stream_suffix, VolumeMap};
pub use volumes::{expand_short_name, resolve_volume_map, resolve_volume_map_for};

/// What a [`RootMap`] lookup answers with: which declared root the path fell
/// under, and its folded remainder components beneath that root.
pub type RootHit = (RootId, Vec<String>);

thread_local! {
    /// Depth counter behind [`UncachedScope`]. A counter rather than a bare
    /// flag so nested or overlapping guards on the same thread compose
    /// correctly: an inner guard's `Drop` must not re-enable caching while an
    /// outer guard (from a caller further up the same call chain) is still
    /// held.
    static SUPPRESS_CACHE_DEPTH: Cell<u32> = const { Cell::new(0) };
}

fn cache_suppressed() -> bool {
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
    /// hook invocation then takes `Decision::PassThrough` and calls the
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
struct OsConsultGuard(());

impl OsConsultGuard {
    fn enter() -> Option<Self> {
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
const DEFAULT_CACHE_CAPACITY: usize = 4096;

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
struct PathCache {
    capacity: usize,
    state: RwLock<PathCacheState>,
}

#[derive(Default)]
struct PathCacheState {
    map: HashMap<String, Option<RootHit>>,
    order: VecDeque<String>,
}

impl PathCache {
    fn new(capacity: usize) -> Self {
        PathCache { capacity: capacity.max(1), state: RwLock::new(PathCacheState::default()) }
    }

    /// A lock-poisoning thread (one that panicked while holding the lock) must
    /// not wedge every future open in a long-running game session — recover
    /// the guard rather than propagating the poison.
    fn get(&self, key: &str) -> Option<Option<RootHit>> {
        let guard = self.state.read().unwrap_or_else(|e| e.into_inner());
        guard.map.get(key).cloned()
    }

    fn insert(&self, key: String, value: Option<RootHit>) {
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
    fn len(&self) -> usize {
        self.state.read().unwrap_or_else(|e| e.into_inner()).map.len()
    }
}

/// One declared root: the [`RootId`] it answers with and its normalized path
/// components in original case, e.g. `["C:", "Games", "Skyrim"]`.
struct Root {
    id: RootId,
    comps: Vec<String>,
}

/// The managed VFS install roots (mount points), as normalized path components.
///
/// **Several roots, not one** (stage 2b task 5). A session virtualizes more
/// than one real filesystem location — the game directory *and*
/// `Documents\My Games\Skyrim` — so the answer to "is this path ours?" is no
/// longer a boolean plus a remainder: it is *which* root, plus the remainder
/// under that root. See [`RootMap::resolve`].
///
/// Two roots may name the same [`RootId`]. That is how an **alias** is
/// expressed — the shim serves the staged launch directory as a second
/// spelling of the game root — and it costs nothing structurally: an alias is
/// just another entry pointing at the same id.
pub struct RootMap {
    /// Declared roots, ordered **longest first** (most path components).
    ///
    /// Order is the whole of the nesting policy: if one root lies under
    /// another (a `Documents\My Games\Skyrim` inside a root someone pointed at
    /// `Documents`, say), the deeper one must win, because the shallower one
    /// would match every path the deeper one does and swallow it. Sorting once
    /// at construction makes that deterministic rather than dependent on
    /// declaration order.
    roots: Vec<Root>,
    /// NT device-name / volume-GUID -> drive-letter table, resolved once from
    /// the live OS at session start (see [`resolve_volume_map`]) and handed in
    /// here — never re-resolved per open, which would be several Win32 calls
    /// per drive on every single open.
    volumes: VolumeMap,
    /// Absorbs the cost of re-deriving the same open path's root membership,
    /// for the raw spellings `compute_under_root` can answer purely from the
    /// string — see [`Resolution`] for why an OS-consulted answer never lands
    /// here.
    cache: PathCache,
    /// Count of lookups that consulted the OS (the `~`-gated fallback branch
    /// in `compute_under_root`). Cheap — one relaxed increment on an already
    /// rare branch — kept so how rarely that branch fires is a measured
    /// claim, not just an asserted one.
    os_consults: AtomicU64,
    /// Count of calls to `compute_under_root` — i.e. cache misses, of either
    /// [`Resolution`] variant. Test-only: lets a test prove a lookup was
    /// actually *recomputed* (this counter moves) rather than merely
    /// inferring it from the cache staying empty, which a bug elsewhere could
    /// also produce for the wrong reason. See
    /// `uncached_scope_suppresses_caching_of_an_otherwise_deterministic_path`.
    #[cfg(test)]
    computes: AtomicU64,
}

impl RootMap {
    /// A single root, answering as [`RootId::DEFAULT`] — the shape tests and
    /// one-root callers want. Both production callers (`vfs-shim`'s `Engine`
    /// and its `FuseClient`) now declare every session root through
    /// [`RootMap::with_roots`] instead.
    ///
    /// `root` may be NT (`\??\C:\Games\Skyrim`) or Win32 (`C:\Games\Skyrim`).
    /// `volumes` is the OS's current device-name/volume-GUID table, resolved
    /// once per session (see [`resolve_volume_map`]) — never resolved here.
    pub fn new(root: &str, volumes: VolumeMap) -> Result<Self, PathError> {
        Self::with_roots(&[(RootId::DEFAULT, root)], volumes)
    }

    /// Several roots at once. Each entry is `(id, path)`; two entries may
    /// share an `id` to declare an alias (see the struct doc).
    ///
    /// Fails on the first path that will not normalize, rather than silently
    /// dropping it — a root that quietly failed to register would make every
    /// path under it look like it belongs to no one, which is precisely the
    /// "content simply missing" failure this project keeps rediscovering.
    pub fn with_roots(roots: &[(RootId, &str)], volumes: VolumeMap) -> Result<Self, PathError> {
        Self::with_capacity(roots, volumes, DEFAULT_CACHE_CAPACITY)
    }

    /// Test-only hook to exercise the cache's bound with a small capacity
    /// instead of [`DEFAULT_CACHE_CAPACITY`].
    #[cfg(test)]
    fn new_with_cache_capacity(
        root: &str,
        volumes: VolumeMap,
        capacity: usize,
    ) -> Result<Self, PathError> {
        Self::with_capacity(&[(RootId::DEFAULT, root)], volumes, capacity)
    }

    fn with_capacity(
        roots: &[(RootId, &str)],
        volumes: VolumeMap,
        capacity: usize,
    ) -> Result<Self, PathError> {
        let mut parsed = Vec::with_capacity(roots.len());
        for (id, path) in roots {
            let norm = normalize_vpath(path)?;
            // A root that normalizes to zero components would match *every*
            // path in `match_canonical` (nothing left to fold-compare), with
            // the whole path handed back as the remainder — silently sealing
            // everything under this root rather than routing it. Fail
            // closed at construction instead of at every lookup: reachable
            // today via `VFS_VIRTUAL_DIR=""` (checked for unset, not empty,
            // at `fuse_client.rs`'s env entry point) and newly plausible now
            // that a second declared root can be malformed independently of
            // the first.
            if norm.is_empty() {
                return Err(PathError::EmptyRoot);
            }
            let comps: Vec<String> = norm.split('/').map(str::to_string).collect();
            parsed.push(Root { id: *id, comps });
        }
        // Longest first — see the `roots` field doc. `sort_by` is stable, so
        // equal-depth roots keep declaration order and the answer stays
        // reproducible.
        parsed.sort_by_key(|r| std::cmp::Reverse(r.comps.len()));
        Ok(RootMap {
            roots: parsed,
            volumes,
            cache: PathCache::new(capacity),
            os_consults: AtomicU64::new(0),
            #[cfg(test)]
            computes: AtomicU64::new(0),
        })
    }

    /// The number of entries currently cached. Test-only, to verify the bound
    /// and the cache/no-cache boundary.
    #[cfg(test)]
    fn cache_len(&self) -> usize {
        self.cache.len()
    }

    /// The number of lookups so far that consulted the OS. Test-only, to
    /// prove an OS-consulted answer was recomputed rather than served from
    /// the cache.
    #[cfg(test)]
    fn os_consult_count(&self) -> u64 {
        self.os_consults.load(Ordering::Relaxed)
    }

    /// The number of times `compute_under_root` actually ran (i.e. cache
    /// misses, of either `Resolution` variant). Test-only, to prove a lookup
    /// was recomputed rather than served from the cache — see this struct's
    /// `computes` field doc comment.
    #[cfg(test)]
    fn compute_count(&self) -> u64 {
        self.computes.load(Ordering::Relaxed)
    }

    /// The normalized components of the deepest declared root (original case).
    /// For tests/diagnostics.
    pub fn root_components(&self) -> &[String] {
        self.roots.first().map(|r| r.comps.as_slice()).unwrap_or(&[])
    }

    /// Which declared root `nt_path` falls under, and its folded remainder
    /// components beneath that root — or `None` if it is outside every root,
    /// malformed, or escaping.
    ///
    /// **This is the predicate.** `contains`/`remainder` are conveniences over
    /// it for callers that already know there is only one root; anything that
    /// has to *route* a request must ask this one, because the id is the half
    /// the ring needs and the remainder alone cannot supply.
    pub fn resolve(&self, nt_path: &str) -> Option<RootHit> {
        self.under_root(nt_path)
    }

    /// Whether `nt_path` lies under any managed root (well-formed, not escaping).
    pub fn contains(&self, nt_path: &str) -> bool {
        self.under_root(nt_path).is_some()
    }

    /// The folded remainder components of `nt_path` under whichever root it
    /// matched, or `None` if it is outside/malformed. Exposed so the overlay
    /// layer can build overlay paths from the same normalized components the
    /// snapshot uses. Callers that need to know *which* root want
    /// [`Self::resolve`].
    pub fn remainder(&self, nt_path: &str) -> Option<Vec<String>> {
        self.under_root(nt_path).map(|(_, rest)| rest)
    }

    /// Decide how to handle an incoming NT open path.
    ///
    /// Fail-safe only for paths this crate has no business deciding for at
    /// all: `Located::Outside` (malformed, escaping, or genuinely outside the
    /// managed root) still yields `PassThrough` — nothing here ever touches
    /// traffic that never named the managed root in the first place.
    ///
    /// Everything *under* the root is decided here now, with no real-
    /// filesystem escape hatch (gate 3's own reason for existing): a
    /// virtualized file still redirects/serves as before, and a tombstone
    /// still denies — those two are unchanged. What changes is the other two
    /// arms, which used to fail open:
    ///
    /// - `NotFound` (a real, on-disk file/directory under the root that no
    ///   provider serves) now denies too, rather than falling through to
    ///   whatever is physically on disk. This is the change the whole gate
    ///   exists for: before, a real file the provider graph had never heard
    ///   of still opened, because "not virtualized" fell all the way through
    ///   to the real filesystem underneath the mount. After, the provider
    ///   graph is the sole authority for what exists under the root — if it
    ///   does not know about a path, that path does not exist, full stop.
    /// - `Dir` (a directory node the snapshot genuinely has — i.e. the
    ///   provider graph considers it real) also denies *here*, which sounds
    ///   backwards for something the brief calls "director-served" until the
    ///   two-path structure is spelled out: this pure, snapshot-only
    ///   function has no ring, no FUSE client, no way to literally open
    ///   anything — it cannot serve a directory handle itself under any
    ///   circumstances, virtualized or not. The actual "director-served
    ///   handle" for a real virtual directory comes from
    ///   `vfs-shim::hook::try_fuse_create`'s live round-trip to the director,
    ///   which runs *before* this function is ever consulted and succeeds
    ///   for every directory the provider graph actually knows about. This
    ///   fallback is reached only when that live path did not classify the
    ///   open at all (no director, or the FUSE client's own root notion
    ///   disagreed with this crate's) — and in that situation there is no
    ///   live director connection here to serve the directory from, so
    ///   failing closed is the only safe answer, not a regression from some
    ///   case that used to work through this function.
    ///
    /// See `rust/docs/escape-matrix.md` for the concrete, predicted
    /// consequence of the `NotFound` half of this change (an MO2-style
    /// junction inside the managed root, previously reachable only via the
    /// passthrough this removes) and the configuration that restores it.
    pub fn decide(&self, nt_path: &str, snap: &SnapshotReader) -> Decision {
        match self.locate(nt_path, snap) {
            Located::Resolved(SnapResolution::File { source, size, .. }) => {
                match vfs_core::decode(&source) {
                    vfs_core::Source::ZipWindow { offset, container } => Decision::Serve {
                        container_nt: render_nt(container),
                        offset,
                        length: size,
                    },
                    vfs_core::Source::Disk(bytes) => {
                        Decision::Redirect { target_nt: render_nt(bytes) }
                    }
                }
            }
            Located::Resolved(SnapResolution::Tombstone)
            | Located::Resolved(SnapResolution::Dir)
            | Located::Resolved(SnapResolution::NotFound) => Decision::Deny,
            Located::Outside => Decision::PassThrough,
        }
    }

    /// Folded remainder components if `nt_path` is under the managed root, else
    /// `None` (out of root, malformed, or escaping). Cached on the raw `nt_path`
    /// string for the [`Resolution::Deterministic`] case only — see
    /// [`Resolution`] and the comment at the `cache.insert` call below for why
    /// an OS-consulted answer is deliberately excluded. Also skipped, for
    /// *either* variant, while an [`UncachedScope`] is held on this thread —
    /// see its doc comment for the caller-side half of this same rule.
    fn under_root(&self, nt_path: &str) -> Option<RootHit> {
        let suppressed = cache_suppressed();
        if !suppressed {
            if let Some(cached) = self.cache.get(nt_path) {
                return cached;
            }
        }
        match self.compute_under_root(nt_path) {
            Resolution::Deterministic(result) => {
                // Safe to cache: a pure function of `nt_path` and the
                // session-frozen `self.volumes`, so it can never go stale --
                // unless the caller has told us (via `UncachedScope`) that
                // `nt_path` itself is not such a pure function, e.g. it was
                // assembled from a live OS query of a handle's current
                // target. `compute_under_root` has no way to see that on its
                // own; the `suppressed` check here is what honors it.
                if !suppressed {
                    self.cache.insert(nt_path.to_string(), result.clone());
                }
                result
            }
            Resolution::OsConsulted(result) => {
                // Never cache this. An OS-resolved identity (an 8.3
                // short-name slot, a junction target) is a fact about the
                // filesystem *now*, not a fact about the string — the slot
                // can be reused after a delete-and-recreate, or a junction
                // retargeted, mid-session. A stale POSITIVE is the dangerous
                // direction: an in-root short-name alias cached as "inside"
                // would keep being treated as inside after the real target
                // is swapped for something outside the root, which is
                // exactly the over-eager failure class this gate exists to
                // avoid (the same class Task 2 already found and fixed once
                // in `VolumeMap`). Recomputing this branch on every call is
                // the deliberate cost of staying correct; do not "fix" this
                // by caching it — see `os_consulted_resolution_is_never_cached`.
                result
            }
        }
    }

    /// The actual (possibly Win32-calling) resolution behind [`Self::under_root`],
    /// run only on a cache miss.
    ///
    /// Two passes, and the return type keeps them distinguishable to the
    /// caller so only the first can ever be cached:
    ///
    /// 1. Pure syntactic canonicalisation ([`canonicalise`]): resolves a
    ///    device or volume-GUID prefix via `self.volumes`, strips NT/DOS
    ///    prefixes, refuses a drive-relative spelling, clamps `..` at a drive
    ///    root. No Win32 call, and a deterministic function of `nt_path` (and
    ///    `self.volumes`, itself frozen for the session) — this alone closes
    ///    the device-path and volume-GUID vectors, and every syntactic escape
    ///    vector Task 1 closed in `canonicalise` itself. Returned as
    ///    [`Resolution::Deterministic`].
    /// 2. Only if that syntactic form does not already place the path under
    ///    the root, and only if it contains `~` — the character every
    ///    OS-generated 8.3 short name contains, and the only shape this pass
    ///    exists to catch — ask the OS what the path actually names right now
    ///    ([`expand_short_name`]), then canonicalise *that* and match again.
    ///    A short-name spelling of a component of the root itself (`GAMES~1`
    ///    for `Games`) cannot be recognised any other way: it is an on-disk
    ///    fact, not something derivable from the string alone. Returned as
    ///    [`Resolution::OsConsulted`] regardless of outcome (including a
    ///    negative one — `expand_short_name` returning `None`), because
    ///    "nothing exists there yet" can also stop being true mid-session.
    ///
    /// The `~` gate matters for cost, not correctness: without it, every
    /// single open that does not syntactically match the root — the common
    /// case for anything outside the VFS, e.g. every system DLL a game
    /// loads — would pay a Win32 round trip. Every real 8.3 short name
    /// contains `~` by construction, so nothing this gate is responsible for
    /// closing is missed by requiring it. In practice this makes the
    /// OS-consulted branch rare: see `plainly_outside_path_never_consults_the_os`
    /// and the cost discussion in the task report.
    fn compute_under_root(&self, nt_path: &str) -> Resolution {
        #[cfg(test)]
        self.computes.fetch_add(1, Ordering::Relaxed);
        let Ok(canon) = canonicalise(nt_path, &self.volumes) else {
            return Resolution::Deterministic(None);
        };
        if let Some(folded) = self.match_canonical(&canon) {
            return Resolution::Deterministic(Some(folded));
        }
        if !canon.contains('~') {
            return Resolution::Deterministic(None);
        }
        self.os_consults.fetch_add(1, Ordering::Relaxed);
        // See `OS_CONSULT_DEPTH`'s doc comment: `expand_short_name` below
        // makes a real `CreateFileW` call, which — when this crate is being
        // consulted from inside a process whose own file APIs are hooked —
        // can feed straight back into this same branch for the same path.
        // Skip the OS consult on a re-entrant call rather than recursing into
        // it without bound.
        let Some(_guard) = OsConsultGuard::enter() else {
            return Resolution::OsConsulted(None);
        };
        // `canon` is already an absolute, NT/DOS-prefix-free, drive-letter
        // form (e.g. `C:/Games~1/Data/a.esp`); backslashes make it a path
        // `CreateFileW` (behind `expand_short_name`) accepts directly.
        let win32_candidate = canon.replace('/', "\\");
        let Some(resolved) = expand_short_name(&win32_candidate) else {
            return Resolution::OsConsulted(None);
        };
        // The OS's answer may itself carry an NT/DOS prefix (`final_path_for_open`
        // returns VOLUME_NAME_DOS, `\\?\`-prefixed) — canonicalise strips
        // whatever recognised prefix is present rather than requiring the
        // caller to know which one, so this is not a second special case.
        let Ok(canon2) = canonicalise(&resolved, &self.volumes) else {
            return Resolution::OsConsulted(None);
        };
        Resolution::OsConsulted(self.match_canonical(&canon2))
    }

    /// Fold-compare an already-canonicalised path's components against every
    /// declared root, returning the first match's id and folded remainder.
    ///
    /// `self.roots` is sorted longest-first at construction, so "first match"
    /// is "deepest match" — a nested root wins over the root it sits inside,
    /// which is the only ordering that does not let a shallow root swallow a
    /// deep one.
    fn match_canonical(&self, canon: &str) -> Option<RootHit> {
        let comps: Vec<&str> =
            if canon.is_empty() { Vec::new() } else { canon.split('/').collect() };
        'roots: for root in &self.roots {
            if comps.len() < root.comps.len() {
                continue;
            }
            for (r, c) in root.comps.iter().zip(comps.iter()) {
                if fold(r) != fold(c) {
                    continue 'roots;
                }
            }
            return Some((
                root.id,
                comps[root.comps.len()..].iter().map(|c| fold(c)).collect(),
            ));
        }
        None
    }

    fn locate(&self, nt_path: &str, snap: &SnapshotReader) -> Located {
        match self.under_root(nt_path) {
            None => Located::Outside,
            Some((_, folded)) => {
                let refs: Vec<&str> = folded.iter().map(String::as_str).collect();
                Located::Resolved(snap.resolve(&refs))
            }
        }
    }
}

/// The outcome of [`RootMap::compute_under_root`], tagged by whether it
/// consulted the OS — the boundary `RootMap::under_root` uses to decide what
/// may be cached. See the doc comment on `RootMap::under_root`'s `cache.insert`
/// call for why [`Resolution::OsConsulted`] must never reach the cache: it is
/// an answer about the filesystem *now*, not a pure function of the input
/// string, and a stale positive here is the over-eager failure class this
/// gate exists to avoid.
enum Resolution {
    /// A pure function of the raw input string (and the session-frozen
    /// `VolumeMap`) — safe to cache indefinitely.
    Deterministic(Option<RootHit>),
    /// Reached by asking the OS what the path currently names (8.3 short-name
    /// / junction resolution). Never cached.
    OsConsulted(Option<RootHit>),
}

/// Where an NT path lands relative to the managed root.
enum Located {
    /// Not under the root, or malformed/escaping — never virtualized.
    Outside,
    /// Under the root; here is the snapshot's answer for the remainder.
    Resolved(SnapResolution),
}

/// Render a backing `source` (a UTF-8 absolute Win32 path, per the director's
/// contract) as an NT DOS-device path. A `source` already carrying an NT/DOS
/// long-path prefix is returned unchanged rather than double-prefixed.
fn render_nt(source: &[u8]) -> String {
    let s = String::from_utf8_lossy(source);
    if s.starts_with(r"\??\") || s.starts_with(r"\\?\") {
        s.into_owned()
    } else {
        format!(r"\??\{s}")
    }
}

/// Decode a length-counted UTF-16 buffer (a `UNICODE_STRING` body) to a `String`.
/// Lossy: unpaired surrogates become U+FFFD rather than panicking.
pub fn utf16_to_string(units: &[u16]) -> String {
    String::from_utf16_lossy(units)
}

/// Encode a `&str` as UTF-16 with NO trailing NUL (`UNICODE_STRING` is counted).
pub fn string_to_utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// One entry in a directory listing — used both for the caller's real on-disk
/// entries and for the merged result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirItem {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
}

/// The outcome of inspecting one NT open path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Let the original NT open proceed unchanged.
    PassThrough,
    /// Reissue the open against this NT path (the mod backing file).
    Redirect { target_nt: String },
    /// The path is tombstoned (mod-deleted); the hook must return
    /// STATUS_OBJECT_NAME_NOT_FOUND rather than open or pass through.
    Deny,
    /// Serve the file's bytes from a window inside a container (zip) file.
    /// The shim opens `container_nt`, maps it, and returns a synthetic handle
    /// covering `[offset, offset + length)`.
    Serve { container_nt: String, offset: u64, length: u64 },
}

/// The directory-info `FILE_INFORMATION_CLASS` values the shim marshals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirInfoClass {
    Directory,       // 1  FILE_DIRECTORY_INFORMATION
    FullDirectory,   // 2  FILE_FULL_DIR_INFORMATION
    BothDirectory,   // 3  FILE_BOTH_DIR_INFORMATION
    Names,           // 12 FILE_NAMES_INFORMATION
    IdBothDirectory, // 37 FILE_ID_BOTH_DIR_INFORMATION
    IdFullDirectory, // 38 FILE_ID_FULL_DIR_INFORMATION
}

impl DirInfoClass {
    /// Map a raw `FILE_INFORMATION_CLASS`; `None` for classes we do not marshal.
    pub fn from_u32(v: u32) -> Option<DirInfoClass> {
        Some(match v {
            1 => DirInfoClass::Directory,
            2 => DirInfoClass::FullDirectory,
            3 => DirInfoClass::BothDirectory,
            12 => DirInfoClass::Names,
            37 => DirInfoClass::IdBothDirectory,
            38 => DirInfoClass::IdFullDirectory,
            _ => return None,
        })
    }

    /// Byte offset of the `FileName` field == the fixed header size.
    fn name_offset(self) -> usize {
        match self {
            DirInfoClass::Names => 12,
            DirInfoClass::Directory => 64,
            DirInfoClass::FullDirectory => 68,
            DirInfoClass::IdFullDirectory => 80,
            DirInfoClass::BothDirectory => 94,
            DirInfoClass::IdBothDirectory => 104,
        }
    }

    /// Byte offset of the `FileNameLength` (u32) field.
    fn name_len_offset(self) -> usize {
        match self {
            DirInfoClass::Names => 8,
            _ => 60,
        }
    }

    /// Whether this class carries `EndOfFile`/`AllocationSize`/`FileAttributes`.
    fn has_metadata(self) -> bool {
        !matches!(self, DirInfoClass::Names)
    }
}

/// The NTSTATUS family a directory write resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirStatus {
    Success,
    NoMoreFiles,
    BufferOverflow,
}

/// Result of marshalling directory entries into a caller buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirWriteResult {
    /// Bytes actually used (end offset of the last record's data) —
    /// the value to report as `IoStatusBlock.Information`.
    pub bytes: usize,
    /// Number of entries written.
    pub count: usize,
    pub status: DirStatus,
}

/// Marshal `items` into `buf` in the layout of `class`, chaining
/// `NextEntryOffset`, 8-byte aligning each record, stopping at `single` (one
/// entry) or when the next record would overflow `buf`. Pure: writes only into
/// `buf`.
pub fn write_dir_info(
    class: DirInfoClass,
    items: &[DirItem],
    buf: &mut [u8],
    single: bool,
) -> DirWriteResult {
    let name_off = class.name_offset();
    let name_len_off = class.name_len_offset();
    let cap = buf.len();
    let mut off = 0usize;
    let mut count = 0usize;
    let mut prev: Option<usize> = None;
    let mut last_end = 0usize;

    for it in items {
        let name16: Vec<u16> = it.name.encode_utf16().collect();
        let namelen = name16.len() * 2;
        let rec = name_off + namelen;
        if off + rec > cap {
            break;
        }
        // Zero the fixed header (EaSize/ShortName/FileId fields left zero).
        for b in &mut buf[off..off + name_off] {
            *b = 0;
        }
        if class.has_metadata() {
            let eof = it.size as i64;
            buf[off + 40..off + 48].copy_from_slice(&eof.to_le_bytes());
            buf[off + 48..off + 56].copy_from_slice(&eof.to_le_bytes());
            let attrs: u32 = if it.is_dir { 0x10 } else { 0x80 };
            buf[off + 56..off + 60].copy_from_slice(&attrs.to_le_bytes());
        }
        buf[off + name_len_off..off + name_len_off + 4]
            .copy_from_slice(&(namelen as u32).to_le_bytes());
        let name_bytes: Vec<u8> = name16.iter().flat_map(|u| u.to_le_bytes()).collect();
        buf[off + name_off..off + name_off + namelen].copy_from_slice(&name_bytes);

        if let Some(p) = prev {
            let delta = (off - p) as u32;
            buf[p..p + 4].copy_from_slice(&delta.to_le_bytes());
        }
        prev = Some(off);
        last_end = off + rec;
        count += 1;
        off += (rec + 7) & !7; // 8-byte align next record
        if single {
            break;
        }
    }

    let status = if count == 0 {
        if items.is_empty() {
            DirStatus::NoMoreFiles
        } else {
            DirStatus::BufferOverflow
        }
    } else {
        DirStatus::Success
    };
    DirWriteResult { bytes: last_end, count, status }
}

/// Parse a `FILE_FULL_DIR_INFORMATION` (class 2) chain into items, skipping `.`
/// and `..`. Bounds-checked: a record that would read past `buf` ends the walk
/// (fail-safe, never panics). The shim always *drains the OS in class 2*, so
/// only this one class needs a parser.
pub fn parse_full_dir_info(buf: &[u8]) -> Vec<DirItem> {
    const HDR: usize = 68;
    let mut out = Vec::new();
    let mut o = 0usize;
    loop {
        if o + HDR > buf.len() {
            break;
        }
        let next = u32::from_le_bytes(buf[o..o + 4].try_into().unwrap()) as usize;
        let size = i64::from_le_bytes(buf[o + 40..o + 48].try_into().unwrap());
        let attrs = u32::from_le_bytes(buf[o + 56..o + 60].try_into().unwrap());
        let namelen = u32::from_le_bytes(buf[o + 60..o + 64].try_into().unwrap()) as usize;
        if o + HDR + namelen > buf.len() {
            break;
        }
        let units: Vec<u16> = buf[o + HDR..o + HDR + namelen]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let name = String::from_utf16_lossy(&units);
        if !name.is_empty() && name != "." && name != ".." {
            out.push(DirItem {
                name,
                is_dir: attrs & 0x10 != 0,
                size: size.max(0) as u64,
                mtime: 0,
            });
        }
        if next == 0 {
            break;
        }
        o += next;
    }
    out
}

/// NtCreateFile create dispositions.
pub const FILE_SUPERSEDE: u32 = 0;
pub const FILE_OPEN: u32 = 1;
pub const FILE_CREATE: u32 = 2;
pub const FILE_OPEN_IF: u32 = 3;
pub const FILE_OVERWRITE: u32 = 4;
pub const FILE_OVERWRITE_IF: u32 = 5;

/// How an open intends to touch a file's content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteIntent {
    /// The caller can modify content (has write/append/generic-write access).
    pub write: bool,
    /// The disposition keeps existing content (`OPEN`/`OPEN_IF`) rather than
    /// truncating or replacing it — the signal that a copy-on-write materialize
    /// must preserve the current bytes.
    pub preserves: bool,
}

/// Classify an open from its desired-access mask and create disposition.
pub fn classify_open(access: u32, disposition: u32) -> WriteIntent {
    // FILE_WRITE_DATA | FILE_APPEND_DATA | GENERIC_WRITE | GENERIC_ALL.
    const WRITE_MASK: u32 = 0x2 | 0x4 | 0x4000_0000 | 0x1000_0000;
    WriteIntent {
        write: access & WRITE_MASK != 0,
        preserves: matches!(disposition, FILE_OPEN | FILE_OPEN_IF),
    }
}

/// The whiteout marker suffix appended to a deleted file's name in the overlay.
pub const WHITEOUT_SUFFIX: &str = ".__vfs_wh__";

/// The overlay marker filename that hides `name` (a deletion tombstone on disk).
pub fn whiteout_marker(name: &str) -> String {
    format!("{name}{WHITEOUT_SUFFIX}")
}

/// If `name` is a whiteout marker, the base name it hides; else `None`.
pub fn is_whiteout(name: &str) -> Option<&str> {
    name.strip_suffix(WHITEOUT_SUFFIX)
}

/// Wrap a Win32 absolute path as an NT DOS-device path (`\??\...`). A path that
/// already carries an NT/DOS long prefix is returned unchanged.
pub fn to_nt(path: &str) -> String {
    if path.starts_with(r"\??\") || path.starts_with(r"\\?\") {
        path.to_string()
    } else {
        format!(r"\??\{path}")
    }
}

/// Strip a `\??\` / `\\?\` prefix and a leading `X:` drive, yielding the
/// volume-relative path (`\...`, no drive) that `FILE_NAME_INFORMATION` carries.
/// Idempotent on already-relative input.
pub fn nt_to_volume_relative(nt_path: &str) -> String {
    let s = nt_path
        .strip_prefix(r"\??\")
        .or_else(|| nt_path.strip_prefix(r"\\?\"))
        .unwrap_or(nt_path);
    let b = s.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        s[2..].to_string()
    } else {
        s.to_string()
    }
}

/// Result of marshalling a `FILE_NAME_INFORMATION` record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NameWriteResult {
    pub bytes: usize,
    pub status: DirStatus,
}

/// Marshal a `FILE_NAME_INFORMATION` / `FILE_NORMALIZED_NAME_INFORMATION`:
/// `FileNameLength` (u32 bytes) @0, UTF-16LE `FileName` (no NUL) @4. On overflow
/// writes only `FileNameLength` (documented behavior).
pub fn write_file_name_info(name: &str, buf: &mut [u8]) -> NameWriteResult {
    let name16: Vec<u16> = name.encode_utf16().collect();
    let namelen = name16.len() * 2;
    if buf.len() < 4 {
        return NameWriteResult { bytes: 0, status: DirStatus::BufferOverflow };
    }
    buf[0..4].copy_from_slice(&(namelen as u32).to_le_bytes());
    if buf.len() < 4 + namelen {
        return NameWriteResult { bytes: 4, status: DirStatus::BufferOverflow };
    }
    let nb: Vec<u8> = name16.iter().flat_map(|u| u.to_le_bytes()).collect();
    buf[4..4 + namelen].copy_from_slice(&nb);
    NameWriteResult { bytes: 4 + namelen, status: DirStatus::Success }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `OsConsultGuard`'s own reentrancy mechanics, independent of any real
    /// OS call or hook — the crash this guard fixes (see its doc comment)
    /// can only be reproduced from inside a real injected session, but the
    /// guard's own held/released bookkeeping is a pure, unit-testable fact:
    /// a nested `enter()` while one is already held must refuse (`None`),
    /// and the slot must become available again once the outer guard drops.
    #[test]
    fn os_consult_guard_refuses_reentry_and_releases_on_drop() {
        let outer = OsConsultGuard::enter().expect("first enter must succeed");
        assert!(OsConsultGuard::enter().is_none(), "a nested enter while held must refuse");
        drop(outer);
        assert!(
            OsConsultGuard::enter().is_some(),
            "the slot must be free again once the outer guard was dropped"
        );
    }

    #[test]
    fn new_normalizes_nt_and_win32_roots() {
        // Both forms normalize to the same component vector.
        let nt = RootMap::new(r"\??\C:\Games\Skyrim", VolumeMap::empty()).unwrap();
        let win32 = RootMap::new(r"C:\Games\Skyrim", VolumeMap::empty()).unwrap();
        assert_eq!(nt.root_components(), win32.root_components());
        assert_eq!(nt.root_components(), vec!["C:", "Games", "Skyrim"]);
    }

    /// A root that normalizes to zero components (`""`, `"."`, `"/"`, or an
    /// NT/DOS prefix with nothing after it) must be rejected at construction,
    /// not accepted and left to `match_canonical` — which, given zero
    /// components to fold-compare, would match *every* path with the whole
    /// path as the remainder. Reachable via `VFS_VIRTUAL_DIR=""` (the shim's
    /// env entry point checks for unset, not empty) and, since stage 2b, via
    /// a second declared root that is malformed independently of the first.
    #[test]
    fn an_empty_root_is_rejected_rather_than_matching_every_path() {
        // `RootMap` has no `Debug` impl (it holds a lookup cache), so
        // `unwrap_err` — which requires `T: Debug` — is not available here;
        // match the `Result` directly instead.
        fn assert_empty_root_err(r: Result<RootMap, PathError>) {
            match r {
                Err(PathError::EmptyRoot) => {}
                Err(other) => panic!("expected PathError::EmptyRoot, got {other:?}"),
                Ok(_) => panic!("expected an error, but the empty root was accepted"),
            }
        }
        assert_empty_root_err(RootMap::new("", VolumeMap::empty()));
        assert_empty_root_err(RootMap::new(".", VolumeMap::empty()));
        assert_empty_root_err(RootMap::new("/", VolumeMap::empty()));
        // A malformed second root must not silently swallow a valid first one.
        assert_empty_root_err(RootMap::with_roots(
            &[(RootId(0), r"C:\Games\Skyrim"), (RootId(1), "")],
            VolumeMap::empty(),
        ));
    }

    /// Stage 2b task 5, step 1: the structural claim of the whole task. A
    /// path under root 1 resolves to `(RootId(1), rel)`, a path under root 0
    /// to `(RootId(0), rel)`, and a path under neither is outside. Before this
    /// task `RootMap` held exactly one root and could answer only "inside" or
    /// "outside", so the shim could not tell the director which root a path
    /// belonged to.
    #[test]
    fn resolve_answers_with_the_matching_root_id_and_remainder() {
        let map = RootMap::with_roots(
            &[
                (RootId(0), r"C:\Games\Skyrim"),
                (RootId(1), r"C:\Users\me\Documents\My Games\Skyrim"),
            ],
            VolumeMap::empty(),
        )
        .unwrap();

        assert_eq!(
            map.resolve(r"\??\C:\Games\Skyrim\Data\Foo.ESP"),
            Some((RootId(0), vec!["data".to_string(), "foo.esp".to_string()]))
        );
        assert_eq!(
            map.resolve(r"\??\C:\Users\me\Documents\My Games\Skyrim\Saves\Save1.ess"),
            Some((RootId(1), vec!["saves".to_string(), "save1.ess".to_string()]))
        );
        assert_eq!(map.resolve(r"\??\C:\Windows\System32\kernel32.dll"), None);
        // The same *relative* path under each root is a different answer —
        // the collision the whole stage exists to make representable.
        assert_eq!(
            map.resolve(r"C:\Games\Skyrim\same.txt").unwrap().0,
            RootId(0)
        );
        assert_eq!(
            map.resolve(r"C:\Users\me\Documents\My Games\Skyrim\same.txt").unwrap().0,
            RootId(1)
        );
    }

    /// A root nested inside another must win, or the shallow one swallows
    /// every path the deep one serves. Declared shallow-first here on purpose:
    /// the ordering must come from `with_roots`' own sort, not from the
    /// caller happening to declare them in a helpful order.
    #[test]
    fn a_nested_root_wins_over_the_root_it_sits_inside() {
        let map = RootMap::with_roots(
            &[
                (RootId(0), r"C:\Users\me\Documents"),
                (RootId(1), r"C:\Users\me\Documents\My Games\Skyrim"),
            ],
            VolumeMap::empty(),
        )
        .unwrap();
        assert_eq!(
            map.resolve(r"C:\Users\me\Documents\My Games\Skyrim\Saves\a.ess"),
            Some((RootId(1), vec!["saves".to_string(), "a.ess".to_string()]))
        );
        assert_eq!(
            map.resolve(r"C:\Users\me\Documents\notes.txt"),
            Some((RootId(0), vec!["notes.txt".to_string()]))
        );
    }

    /// Two declared paths may share one `RootId`: that is how the shim's
    /// staged-launch directory is served as a second spelling of the game
    /// root. Both must resolve, and both must answer with the *same* id, so a
    /// request routed through either spelling reaches the same provider.
    #[test]
    fn two_paths_may_share_one_root_id_as_an_alias() {
        let map = RootMap::with_roots(
            &[
                (RootId(0), r"C:\Games\Skyrim"),
                (RootId(0), r"C:\tmp\vfs-stage-21728"),
            ],
            VolumeMap::empty(),
        )
        .unwrap();
        assert_eq!(
            map.resolve(r"C:\Games\Skyrim\Data\a.esm"),
            Some((RootId(0), vec!["data".to_string(), "a.esm".to_string()]))
        );
        assert_eq!(
            map.resolve(r"C:\tmp\vfs-stage-21728\Data\a.esm"),
            Some((RootId(0), vec!["data".to_string(), "a.esm".to_string()]))
        );
    }

    /// Canonicalisation is per-`RootMap`, not per-root: registering a second
    /// root must not cost the first one its device-path/volume-GUID
    /// resolution, and the second root must get the same treatment rather
    /// than a string-prefix approximation of it. This is the acceptance
    /// criterion "the escape matrix passes against every root, not just the
    /// first", at the unit level where the canonicaliser actually lives.
    #[test]
    fn canonicalisation_applies_to_every_root_not_just_the_first() {
        let mut volumes = VolumeMap::empty();
        volumes.insert(r"\Device\HarddiskVolume3", 'C');
        let map = RootMap::with_roots(
            &[(RootId(0), r"C:\Games\Skyrim"), (RootId(1), r"C:\Docs\Skyrim")],
            volumes,
        )
        .unwrap();
        assert_eq!(
            map.resolve(r"\Device\HarddiskVolume3\Games\Skyrim\Data\a.esp").map(|(r, _)| r),
            Some(RootId(0))
        );
        assert_eq!(
            map.resolve(r"\Device\HarddiskVolume3\Docs\Skyrim\Saves\a.ess").map(|(r, _)| r),
            Some(RootId(1)),
            "the second root must canonicalise exactly like the first"
        );
        // And the over-eager direction still fails closed for both.
        assert!(map.resolve(r"\Device\HarddiskVolume3\Windows\System32\x.dll").is_none());
    }

    #[test]
    fn utf16_round_trips() {
        let s = "C:\\Games\\Skyrim\\Data\\foo.esp";
        assert_eq!(utf16_to_string(&string_to_utf16(s)), s);
        // No trailing NUL is appended.
        assert_eq!(*string_to_utf16("ab").last().unwrap(), b'b' as u16);
    }

    #[test]
    fn utf16_lossy_does_not_panic_on_unpaired_surrogate() {
        let units: [u16; 2] = [0xD800, b'x' as u16]; // lone high surrogate
        let _ = utf16_to_string(&units); // must not panic
    }

    use vfs_shared::SnapshotReader;

    // Build a snapshot with two virtual files under `data/`.
    fn snapshot_bytes() -> Vec<u8> {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let file = |vpath: &str, source: &str| InputEntry {
            vpath: vpath.into(),
            kind: EntryKind::File,
            source: source.into(),
            size: 10,
            mtime: 1,
        };
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![
                file("data/foo.esp", r"D:\Mods\Cool\foo.esp"),
                file("data/sub/bar.dds", r"D:\Mods\Cool\bar.dds"),
                InputEntry {
                    vpath: "data/deleted.esp".into(),
                    kind: EntryKind::Tombstone,
                    source: "".into(),
                    size: 0,
                    mtime: 0,
                },
            ],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    }

    fn root() -> RootMap {
        RootMap::new(r"\??\C:\Games\Skyrim", VolumeMap::empty()).unwrap()
    }

    #[test]
    fn redirects_a_virtual_file() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let d = root().decide(r"\??\C:\Games\Skyrim\Data\foo.esp", &snap);
        assert_eq!(
            d,
            Decision::Redirect { target_nt: r"\??\D:\Mods\Cool\foo.esp".to_string() }
        );
    }

    #[test]
    fn redirect_is_case_insensitive_on_root_and_remainder() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let d = root().decide(r"\??\c:\games\SKYRIM\DATA\Foo.ESP", &snap);
        assert_eq!(
            d,
            Decision::Redirect { target_nt: r"\??\D:\Mods\Cool\foo.esp".to_string() }
        );
    }

    #[test]
    fn passes_through_outside_root() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let d = root().decide(r"\??\C:\Windows\System32\kernel32.dll", &snap);
        assert_eq!(d, Decision::PassThrough);
    }

    /// Gate 3, Task 5 flip (was `passes_through_under_root_but_not_virtualized`,
    /// asserting `Decision::PassThrough`): a real file under the root that no
    /// provider serves is now denied, not passed through to the real
    /// filesystem — this is the negative canary's own logic-level shape (see
    /// `real_on_disk_file_under_root_not_in_snapshot_is_denied` in
    /// `vfs-shim::engine` for the same fact proven against an actual on-disk
    /// file) and the change the whole gate exists for.
    #[test]
    fn denies_under_root_but_not_virtualized() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let d = root().decide(r"\??\C:\Games\Skyrim\Data\notmod.esp", &snap);
        assert_eq!(d, Decision::Deny);
    }

    /// Gate 3, Task 5 flip (was `passes_through_a_virtual_directory`,
    /// asserting `Decision::PassThrough`): this pure, snapshot-only function
    /// cannot itself hand back a director-served handle for a `Dir`
    /// resolution (see `decide`'s own doc comment for why), so it must not
    /// fail open to the real filesystem either — it now denies, matching the
    /// `NotFound` arm. The actual director-served handle for a real virtual
    /// directory comes from `vfs-shim::hook::try_fuse_create`'s live path,
    /// which this crate has no way to reach and which this test cannot
    /// exercise (no ring here).
    #[test]
    fn denies_a_virtual_directory_this_pure_function_cannot_itself_serve() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let d = root().decide(r"\??\C:\Games\Skyrim\Data", &snap);
        assert_eq!(d, Decision::Deny);
    }

    /// Step 1's failing-test-first negative canary, at this crate's own
    /// level: a REAL file physically on disk, under a real root directory,
    /// that the snapshot never mentions at all (not even as a `Dir` node --
    /// nothing above it in the tree references it). Before this task's fix
    /// this returned `PassThrough`, and a caller trampolining on that verdict
    /// would open the real bytes below. `RootMap::decide` never touches the
    /// real filesystem itself (this is exactly why a synthetic file is
    /// enough here, and a real on-disk one is only needed one layer up, at
    /// `vfs-shim::engine`, to prove the acceptance-level claim) -- but
    /// building the real file and root here anyway keeps this test honest
    /// about what "under the managed root" means physically, not just as a
    /// string.
    #[test]
    fn real_on_disk_file_under_root_with_no_snapshot_entry_is_denied() {
        let base = std::env::temp_dir()
            .join(format!("vfs-redirect-negcanary-{}", std::process::id()));
        std::fs::create_dir_all(base.join("Data")).unwrap();
        std::fs::write(base.join("Data").join("negative-canary.bin"), b"real bytes on disk")
            .unwrap();

        let map = RootMap::new(&base.to_string_lossy(), VolumeMap::empty()).unwrap();
        // A snapshot that knows about a completely unrelated file, so `Data`
        // itself is not even a `Dir` node -- the lookup for
        // `data/negative-canary.bin` fails at the very first component and
        // resolves to `SnapResolution::NotFound`.
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let raw = format!(r"{}\Data\negative-canary.bin", base.to_string_lossy());
        assert_eq!(
            map.decide(&raw, &snap),
            Decision::Deny,
            "a real, on-disk file under the root with no provider must be denied"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn passes_through_escaping_path_without_panic() {
        // Four `..` pop past the drive component, so normalize_vpath returns
        // PathError::EscapesRoot; decide must fail safe to PassThrough, not panic.
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let d = root().decide(r"\??\C:\Games\Skyrim\..\..\..\..\evil", &snap);
        assert_eq!(d, Decision::PassThrough);
    }

    #[test]
    fn win32_form_root_matches_nt_form_open() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let win32_root = RootMap::new(r"C:\Games\Skyrim", VolumeMap::empty()).unwrap();
        let d = win32_root.decide(r"\??\C:\Games\Skyrim\Data\foo.esp", &snap);
        assert_eq!(
            d,
            Decision::Redirect { target_nt: r"\??\D:\Mods\Cool\foo.esp".to_string() }
        );
    }

    #[test]
    fn source_already_nt_prefixed_is_not_double_prefixed() {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "data/foo.esp".into(),
                kind: EntryKind::File,
                source: r"\??\D:\Mods\Cool\foo.esp".into(),
                size: 10,
                mtime: 1,
            }],
        }])
        .unwrap();
        let bytes = vfs_shared::bridge::flatten(&tree);
        let snap = SnapshotReader::open(&bytes).unwrap();
        let d = root().decide(r"\??\C:\Games\Skyrim\Data\foo.esp", &snap);
        assert_eq!(
            d,
            Decision::Redirect { target_nt: r"\??\D:\Mods\Cool\foo.esp".to_string() }
        );
    }

    #[test]
    fn decide_denies_a_tombstoned_path() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        assert_eq!(
            root().decide(r"\??\C:\Games\Skyrim\Data\deleted.esp", &snap),
            Decision::Deny
        );
    }

    #[test]
    fn contains_reports_under_root() {
        let r = root(); // \??\C:\Games\Skyrim
        assert!(r.contains(r"\??\C:\Games\Skyrim\Data\foo.esp"));
        assert!(r.contains(r"\??\C:\Games\Skyrim")); // the root itself
        assert!(!r.contains(r"\??\C:\Windows\System32"));
        assert!(!r.contains(r"\??\C:\Games\Skyrim\..\..\..\..\evil")); // escaping
    }

    fn ditem(name: &str, is_dir: bool, size: u64) -> DirItem {
        DirItem { name: name.into(), is_dir, size, mtime: 0 }
    }

    fn ru32(buf: &[u8], rec: usize, off: usize) -> u32 {
        u32::from_le_bytes(buf[rec + off..rec + off + 4].try_into().unwrap())
    }
    fn ri64(buf: &[u8], rec: usize, off: usize) -> i64 {
        i64::from_le_bytes(buf[rec + off..rec + off + 8].try_into().unwrap())
    }
    fn rname(buf: &[u8], rec: usize, name_off: usize, namelen: usize) -> String {
        let units: Vec<u16> = buf[rec + name_off..rec + name_off + namelen]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    }

    #[test]
    fn write_full_dir_two_entries_chained() {
        let items = [ditem("a.esp", false, 5), ditem("sub", true, 0)];
        let mut buf = vec![0u8; 1024];
        let r = write_dir_info(DirInfoClass::FullDirectory, &items, &mut buf, false);
        assert_eq!(r.status, DirStatus::Success);
        assert_eq!(r.count, 2);
        assert_eq!(ru32(&buf, 0, 60), 10); // "a.esp" = 5 chars * 2 bytes
        assert_eq!(ri64(&buf, 0, 40), 5); // EndOfFile
        assert_eq!(ru32(&buf, 0, 56), 0x80); // FILE_ATTRIBUTE_NORMAL
        assert_eq!(rname(&buf, 0, 68, 10), "a.esp");
        let next = ru32(&buf, 0, 0) as usize;
        assert_eq!(next, 80); // (68+10)=78 -> 8-align -> 80
        assert_eq!(ru32(&buf, next, 56), 0x10); // second is a directory
        assert_eq!(rname(&buf, next, 68, 6), "sub");
        assert_eq!(ru32(&buf, next, 0), 0); // last record: NextEntryOffset 0
        assert_eq!(r.bytes, 80 + 68 + 6);
    }

    #[test]
    fn write_both_dir_uses_class3_header() {
        let items = [ditem("x", false, 1)];
        let mut buf = vec![0u8; 512];
        let r = write_dir_info(DirInfoClass::BothDirectory, &items, &mut buf, false);
        assert_eq!(r.status, DirStatus::Success);
        assert_eq!(ru32(&buf, 0, 60), 2);
        assert_eq!(ru32(&buf, 0, 56), 0x80);
        assert_eq!(rname(&buf, 0, 94, 2), "x");
    }

    #[test]
    fn write_names_class_is_name_only() {
        let items = [ditem("only.txt", false, 999)];
        let mut buf = vec![0u8; 256];
        let r = write_dir_info(DirInfoClass::Names, &items, &mut buf, false);
        assert_eq!(r.status, DirStatus::Success);
        assert_eq!(ru32(&buf, 0, 8), 16); // "only.txt" = 8*2
        assert_eq!(rname(&buf, 0, 12, 16), "only.txt");
    }

    #[test]
    fn write_single_entry_stops_after_one() {
        let items = [ditem("a", false, 1), ditem("b", false, 1)];
        let mut buf = vec![0u8; 512];
        let r = write_dir_info(DirInfoClass::FullDirectory, &items, &mut buf, true);
        assert_eq!(r.count, 1);
        assert_eq!(r.status, DirStatus::Success);
        assert_eq!(ru32(&buf, 0, 0), 0); // single -> no chain
    }

    #[test]
    fn write_empty_is_no_more_files() {
        let mut buf = vec![0u8; 128];
        let r = write_dir_info(DirInfoClass::FullDirectory, &[], &mut buf, false);
        assert_eq!(r.count, 0);
        assert_eq!(r.status, DirStatus::NoMoreFiles);
        assert_eq!(r.bytes, 0);
    }

    #[test]
    fn write_too_small_for_first_is_buffer_overflow() {
        let items = [ditem("longname.esp", false, 1)];
        let mut buf = vec![0u8; 8]; // smaller than one class-2 record
        let r = write_dir_info(DirInfoClass::FullDirectory, &items, &mut buf, false);
        assert_eq!(r.count, 0);
        assert_eq!(r.status, DirStatus::BufferOverflow);
    }

    #[test]
    fn dir_info_class_from_u32() {
        assert_eq!(DirInfoClass::from_u32(2), Some(DirInfoClass::FullDirectory));
        assert_eq!(DirInfoClass::from_u32(3), Some(DirInfoClass::BothDirectory));
        assert_eq!(DirInfoClass::from_u32(12), Some(DirInfoClass::Names));
        assert_eq!(DirInfoClass::from_u32(99), None);
    }

    #[test]
    fn parse_full_dir_round_trips_and_skips_dots() {
        let items = [
            ditem(".", true, 0),
            ditem("..", true, 0),
            ditem("keep.esp", false, 42),
            ditem("kids", true, 0),
        ];
        let mut buf = vec![0u8; 4096];
        let w = write_dir_info(DirInfoClass::FullDirectory, &items, &mut buf, false);
        assert_eq!(w.status, DirStatus::Success);
        let parsed = parse_full_dir_info(&buf);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0], DirItem { name: "keep.esp".into(), is_dir: false, size: 42, mtime: 0 });
        assert_eq!(parsed[1], DirItem { name: "kids".into(), is_dir: true, size: 0, mtime: 0 });
    }

    #[test]
    fn parse_full_dir_empty_buffer_is_empty() {
        let buf = vec![0u8; 68];
        let _ = parse_full_dir_info(&buf); // must not panic
    }

    #[test]
    fn volume_relative_strips_prefix_and_drive() {
        assert_eq!(
            nt_to_volume_relative(r"\??\C:\Games\Skyrim\Data\foo.esp"),
            r"\Games\Skyrim\Data\foo.esp"
        );
        assert_eq!(nt_to_volume_relative(r"\\?\D:\Mods\x.esp"), r"\Mods\x.esp");
        assert_eq!(nt_to_volume_relative(r"\Games\already.esp"), r"\Games\already.esp");
    }

    #[test]
    fn write_file_name_info_round_trips() {
        let mut buf = vec![0u8; 128];
        let r = write_file_name_info(r"\Games\Skyrim\Data\foo.esp", &mut buf);
        assert_eq!(r.status, DirStatus::Success);
        let namelen = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        assert_eq!(namelen, r"\Games\Skyrim\Data\foo.esp".encode_utf16().count() * 2);
        let units: Vec<u16> =
            buf[4..4 + namelen].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(String::from_utf16_lossy(&units), r"\Games\Skyrim\Data\foo.esp");
        assert_eq!(r.bytes, 4 + namelen);
    }

    #[test]
    fn write_file_name_info_overflow_writes_length_only() {
        let mut buf = vec![0u8; 6]; // room for u32 len but not the name
        let r = write_file_name_info("abcdef", &mut buf);
        assert_eq!(r.status, DirStatus::BufferOverflow);
        assert_eq!(u32::from_le_bytes(buf[0..4].try_into().unwrap()), 12);
    }

    #[test]
    fn whiteout_marker_round_trips() {
        let m = whiteout_marker("foo.esp");
        assert_eq!(m, "foo.esp.__vfs_wh__");
        assert_eq!(is_whiteout(&m), Some("foo.esp"));
        assert_eq!(is_whiteout("foo.esp"), None);
    }

    #[test]
    fn to_nt_prefixes_and_preserves() {
        assert_eq!(to_nt(r"C:\overlay\foo.esp"), r"\??\C:\overlay\foo.esp");
        assert_eq!(to_nt(r"\??\C:\x"), r"\??\C:\x");
        assert_eq!(to_nt(r"\\?\C:\x"), r"\\?\C:\x");
    }

    #[test]
    fn classify_open_reads_writes_and_preserves() {
        // Read: SYNCHRONIZE|READ_DATA, disp OPEN -> not a write.
        assert_eq!(
            classify_open(0x0010_0001, FILE_OPEN),
            WriteIntent { write: false, preserves: true }
        );
        // GENERIC_WRITE + OPEN_IF -> write, preserves (COW-materialize).
        assert_eq!(
            classify_open(0x4010_0080, FILE_OPEN_IF),
            WriteIntent { write: true, preserves: true }
        );
        // GENERIC_WRITE + OVERWRITE_IF -> write, does not preserve (truncate).
        assert_eq!(
            classify_open(0x4000_0000, FILE_OVERWRITE_IF),
            WriteIntent { write: true, preserves: false }
        );
        // APPEND_DATA + CREATE -> write, create (no preserve).
        assert_eq!(
            classify_open(0x4, FILE_CREATE),
            WriteIntent { write: true, preserves: false }
        );
    }

    #[test]
    fn remainder_returns_folded_components() {
        let r = root(); // \??\C:\Games\Skyrim
        assert_eq!(
            r.remainder(r"\??\C:\Games\Skyrim\Data\Foo.ESP"),
            Some(vec!["data".to_string(), "foo.esp".to_string()])
        );
        assert_eq!(r.remainder(r"\??\C:\Windows"), None);
    }

    #[test]
    fn decide_serves_a_zip_window_source() {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId, SourceId};
        let src = SourceId::new(vfs_core::encode_zip_window(
            0x1_0000_0010,
            r"C:\GameLayers\base.zip",
        ));
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "data/big.bsa".into(),
                kind: EntryKind::File,
                source: src,
                size: 4242,
                mtime: 1,
            }],
        }])
        .unwrap();
        let snap = vfs_shared::bridge::flatten(&tree);
        let reader = vfs_shared::SnapshotReader::open(&snap).unwrap();
        let map = RootMap::new(r"\??\C:\Games\Skyrim", VolumeMap::empty()).unwrap();
        assert_eq!(
            map.decide(r"\??\C:\Games\Skyrim\Data\big.bsa", &reader),
            Decision::Serve {
                container_nt: r"\??\C:\GameLayers\base.zip".to_string(),
                offset: 0x1_0000_0010,
                length: 4242,
            }
        );
    }

    // -- Task 3: canonicalisation wired into `under_root` -----------------

    /// A device-path spelling of a file under the root, which the old
    /// normalize_vpath-only `under_root` classified outside (no device-prefix
    /// resolution at all), must now resolve inside via the `VolumeMap` handed
    /// to `RootMap::new`.
    #[test]
    fn under_root_recognises_a_device_path_spelling() {
        let mut volumes = VolumeMap::empty();
        volumes.insert(r"\Device\HarddiskVolume3", 'C');
        let map = RootMap::new(r"C:\Games\Skyrim", volumes).unwrap();
        assert!(map.contains(r"\Device\HarddiskVolume3\Games\Skyrim\Data\a.esp"));
    }

    /// The failure mode of an over-eager canonicaliser is worse than the one
    /// being fixed: a path genuinely outside the root — spelled either as a
    /// plain drive path or via the very same registered device prefix — must
    /// stay outside. Registering a device prefix must not make the VFS start
    /// swallowing the rest of that volume.
    #[test]
    fn under_root_still_rejects_a_path_genuinely_outside_the_root() {
        let mut volumes = VolumeMap::empty();
        volumes.insert(r"\Device\HarddiskVolume3", 'C');
        let map = RootMap::new(r"C:\Games\Skyrim", volumes).unwrap();
        assert!(!map.contains(r"C:\Windows\System32\kernel32.dll"));
        assert!(!map.contains(r"\Device\HarddiskVolume3\Windows\System32\kernel32.dll"));
    }

    /// An 8.3 short-name spelling of a component of the root ITSELF (not just
    /// of the virtual remainder under it) is a real bypass: syntactic
    /// canonicalisation alone cannot know `GAMES~1` and `Games` name the same
    /// directory, only the OS does. Builds a real temp root (short names are
    /// an on-disk fact, not derivable from the string), so this needs a real
    /// file — skips gracefully if this volume has 8.3 generation disabled
    /// (same convention as the existing vfs-win / vfs-redirect volumes tests).
    #[test]
    #[cfg(windows)]
    fn under_root_recognises_an_8dot3_style_spelling_of_the_root() {
        let base = std::env::temp_dir()
            .join(format!("vfs-redirect-830-under-root-{}", std::process::id()));
        let long_name = "ThisIsALongRootDirectoryNameForShortNameTesting";
        let root_dir = base.join(long_name);
        std::fs::create_dir_all(root_dir.join("Data")).unwrap();
        std::fs::write(root_dir.join("Data").join("a.esp"), b"x").unwrap();
        let root_str = root_dir.to_str().unwrap().to_string();

        let short_root = match vfs_win::short_path_name(&root_str) {
            Some(s) if !s.eq_ignore_ascii_case(&root_str) => s,
            _ => {
                // 8.3 name generation disabled on this volume: nothing to
                // test here (Task 6's `unbuildable` case, not a failure).
                std::fs::remove_dir_all(&base).ok();
                return;
            }
        };

        // The OS's own resolution of the short root must be VOLUME_NAME_DOS
        // (`\\?\`-prefixed) when it goes through `final_path_for_open` --
        // this is exactly the prefix shape flagged in Task 2's review as a
        // silent-fail-closed trap if a consumer assumes a bare drive form.
        let via_final_path = vfs_win::final_path_for_open(&short_root);
        if let Some(p) = &via_final_path {
            assert!(
                p.starts_with(r"\\?\"),
                "expected VOLUME_NAME_DOS (\\\\?\\-prefixed) form: {p}"
            );
        }

        let map = RootMap::new(&root_str, VolumeMap::empty()).unwrap();
        let raw = format!(r"{short_root}\Data\a.esp");
        assert!(map.contains(&raw), "8.3-spelled root was not recognised as inside: {raw}");

        std::fs::remove_dir_all(&base).ok();
    }

    /// A resolution that never left the raw string (pure `canonicalise` +
    /// component match, no OS call) is a deterministic function of its input
    /// and is safe to cache: the second lookup of the same raw spelling must
    /// be served from the cache rather than recomputed.
    #[test]
    fn deterministic_resolution_is_cached() {
        let map = RootMap::new_with_cache_capacity(r"C:\Games\Skyrim", VolumeMap::empty(), 8)
            .unwrap();
        let raw = r"C:\Games\Skyrim\Data\a.esp"; // no `~`: never reaches the OS branch.
        assert!(map.contains(raw));
        assert_eq!(map.cache_len(), 1, "a deterministic resolution was not cached");
        assert!(map.contains(raw));
        assert_eq!(map.cache_len(), 1, "the second lookup added a second entry");
    }

    /// The finding this test guards: an OS-resolved identity (an 8.3
    /// short-name slot, a junction target) is not stable for the life of a
    /// cache entry — the slot can be reused after a delete-and-recreate, or
    /// the junction retargeted, mid-session. A stale POSITIVE is the
    /// dangerous direction: an in-root short-name alias cached as "inside"
    /// would stay "inside" after the real on-disk target is swapped for
    /// something outside the root, which is exactly the over-eager failure
    /// class this gate exists to avoid.
    ///
    /// So: any resolution that consulted the OS (`compute_under_root`'s `~`
    /// fallback) must never be cached, positive or negative. Proven here two
    /// ways: the cache stays empty across two lookups of the same raw 8.3
    /// spelling, and the OS-consult counter increments on BOTH lookups (proof
    /// it was recomputed the second time, not served from a cache miss that
    /// happened to also be empty for some other reason).
    #[test]
    #[cfg(windows)]
    fn os_consulted_resolution_is_never_cached() {
        let base = std::env::temp_dir()
            .join(format!("vfs-redirect-830-no-cache-{}", std::process::id()));
        let long_name = "ThisIsALongRootDirectoryNameForNoCacheTesting";
        let root_dir = base.join(long_name);
        std::fs::create_dir_all(root_dir.join("Data")).unwrap();
        std::fs::write(root_dir.join("Data").join("a.esp"), b"x").unwrap();
        let root_str = root_dir.to_str().unwrap().to_string();

        let short_root = match vfs_win::short_path_name(&root_str) {
            Some(s) if !s.eq_ignore_ascii_case(&root_str) => s,
            _ => {
                // 8.3 disabled on this volume: nothing forces the OS-consulted
                // branch here (Task 6's `unbuildable` case, not a failure).
                std::fs::remove_dir_all(&base).ok();
                return;
            }
        };

        let map = RootMap::new(&root_str, VolumeMap::empty()).unwrap();
        let raw = format!(r"{short_root}\Data\a.esp");

        assert!(map.contains(&raw));
        assert_eq!(map.cache_len(), 0, "an OS-consulted resolution was cached");
        assert_eq!(map.os_consult_count(), 1);

        assert!(map.contains(&raw));
        assert_eq!(map.cache_len(), 0, "an OS-consulted resolution was cached on a second lookup");
        assert_eq!(
            map.os_consult_count(),
            2,
            "the second lookup did not re-consult the OS -- it must have been served from a cache"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    /// Gate-review finding: a path a *caller* assembled from its own OS query
    /// (e.g. `vfs-shim` resolving `OBJECT_ATTRIBUTES.RootDirectory` via
    /// `GetFinalPathNameByHandleW` on a handle it does not own) carries no `~`
    /// and no other marker `compute_under_root` can see — from here it is
    /// indistinguishable from an ordinary literal path, so on its own it
    /// would be classified `Resolution::Deterministic` and cached permanently
    /// even though the caller knows it is a snapshot of live, mutable state.
    /// `UncachedScope` is the caller-side escape hatch for exactly this case.
    ///
    /// Proven the same way `os_consulted_resolution_is_never_cached` proves
    /// its case: a recomputation-counter delta, not merely an empty cache
    /// (which a bug elsewhere could also produce for the wrong reason). Also
    /// confirms the suppression is scoped to the guard's lifetime, not a
    /// permanent regression: caching resumes once it is dropped.
    #[test]
    fn uncached_scope_suppresses_caching_of_an_otherwise_deterministic_path() {
        let map =
            RootMap::new_with_cache_capacity(r"C:\Games\Skyrim", VolumeMap::empty(), 8).unwrap();
        // No `~`: purely deterministic shape by `compute_under_root`'s own
        // rules -- would be cached on the very first lookup without the guard.
        let raw = r"C:\Games\Skyrim\Data\a.esp";

        {
            let _guard = UncachedScope::enter();
            assert!(map.contains(raw));
            assert_eq!(map.compute_count(), 1, "the first guarded lookup did not compute at all");
            assert_eq!(map.cache_len(), 0, "a guarded lookup was cached");

            assert!(map.contains(raw));
            assert_eq!(
                map.compute_count(),
                2,
                "a second lookup of the identical raw string, still under the guard, was served \
                 from the cache instead of being recomputed -- it must recompute every time"
            );
            assert_eq!(map.cache_len(), 0, "a guarded lookup was cached on a second pass");
        }

        // The guard is dropped: this is a genuine first-ever cache miss for
        // `raw` (nothing above ever inserted), so it recomputes once more and
        // this time gets cached.
        assert!(map.contains(raw));
        assert_eq!(map.compute_count(), 3, "the first unguarded lookup did not recompute");
        assert_eq!(map.cache_len(), 1, "caching did not resume once the guard was dropped");

        // A second unguarded lookup is a genuine cache hit: proves the
        // suppression above came specifically from the guard, not from some
        // other reason `compute_count` might have kept moving.
        assert!(map.contains(raw));
        assert_eq!(
            map.compute_count(),
            3,
            "a cache hit outside the guard was recomputed instead of served from the cache"
        );
    }

    /// A path that never contains `~` and never matches the root deterministically
    /// fails closed without ever touching the OS-consult counter — confirms the
    /// `~` gate, not just the cache boundary, is doing its job.
    #[test]
    fn plainly_outside_path_never_consults_the_os() {
        let map = RootMap::new(r"C:\Games\Skyrim", VolumeMap::empty()).unwrap();
        assert!(!map.contains(r"C:\Windows\System32\kernel32.dll"));
        assert_eq!(map.os_consult_count(), 0);
    }

    /// `vfs_win::final_path_for_open` returns `GetFinalPathNameByHandleW`'s
    /// default VOLUME_NAME_DOS form, which is `\\?\`-prefixed -- the Win32
    /// spelling, never the NT `\??\` spelling a real hooked open presents.
    /// Task 2's review found a bug of exactly this shape (a volume-GUID key
    /// registered in the wrong prefix silently matched nothing). Guard the
    /// same trap here: canonicalise must treat `\\?\` the same as any other
    /// recognised NT/DOS prefix, not require the caller to strip it first,
    /// so feeding a real OS-resolved path straight back into canonicalise
    /// (as `under_root`'s fallback does) can never silently fail closed.
    #[test]
    #[cfg(windows)]
    fn os_resolved_dos_prefixed_path_still_canonicalises_correctly() {
        let dir =
            std::env::temp_dir().join(format!("vfs-redirect-dosform-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("plain.txt");
        std::fs::write(&file, b"x").unwrap();

        let resolved = vfs_win::final_path_for_open(file.to_str().unwrap())
            .expect("should resolve an existing file");
        assert!(resolved.starts_with(r"\\?\"), "expected VOLUME_NAME_DOS form: {resolved}");

        let canon = canonicalise(&resolved, &VolumeMap::empty()).unwrap();
        assert!(
            canon.to_ascii_lowercase().ends_with("plain.txt"),
            "lost the file name: {canon}"
        );
        assert!(!canon.contains('?'), "leftover NT/DOS prefix marker in canonical form: {canon}");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The cache is bounded: pushing more distinct raw spellings through
    /// `under_root` than its capacity must not grow it past that capacity —
    /// a game runs for hours and opens a great many distinct paths over a
    /// session, so an unbounded cache would leak memory for the life of the
    /// process.
    #[test]
    fn cache_evicts_rather_than_growing_without_bound() {
        let map = RootMap::new_with_cache_capacity(r"C:\Games\Skyrim", VolumeMap::empty(), 2)
            .unwrap();
        map.contains(r"C:\Games\Skyrim\Data\a.esp");
        map.contains(r"C:\Games\Skyrim\Data\b.esp");
        map.contains(r"C:\Games\Skyrim\Data\c.esp");
        assert!(map.cache_len() <= 2, "cache grew past its capacity: {}", map.cache_len());
    }

    /// The same raw spelling queried twice is a single cache entry, not two —
    /// the whole point of keying on the raw input string.
    #[test]
    fn repeated_raw_spelling_is_one_cache_entry() {
        let map = RootMap::new_with_cache_capacity(r"C:\Games\Skyrim", VolumeMap::empty(), 8)
            .unwrap();
        let raw = r"C:\Games\Skyrim\Data\a.esp";
        map.contains(raw);
        map.contains(raw);
        map.contains(raw);
        assert_eq!(map.cache_len(), 1);
    }

    /// The strongest form of the over-eager check: a path that genuinely
    /// lies OUTSIDE the root, but that also carries a `~` and so specifically
    /// forces `compute_under_root` down its Win32-fallback branch (the same
    /// branch the 8.3-spelling test above exercises for an IN-root path),
    /// must still come back outside once the OS resolves it. A short-name
    /// vector closing false positives (real files start matching the root
    /// when they should not) would be a worse bug than the one this gate
    /// fixes.
    #[test]
    #[cfg(windows)]
    fn under_root_fallback_branch_does_not_pull_in_a_path_outside_the_root() {
        let base = std::env::temp_dir()
            .join(format!("vfs-redirect-830-over-eager-{}", std::process::id()));
        let root_dir = base.join("TheManagedRootDirectory");
        let outside_dir = base.join("ANeighbouringDirectoryNotUnderTheRootAtAll");
        std::fs::create_dir_all(&root_dir).unwrap();
        std::fs::create_dir_all(&outside_dir).unwrap();
        std::fs::write(outside_dir.join("secret.txt"), b"not yours").unwrap();
        let outside_str = outside_dir.to_str().unwrap().to_string();

        let short_outside = match vfs_win::short_path_name(&outside_str) {
            Some(s) if !s.eq_ignore_ascii_case(&outside_str) => s,
            _ => {
                // 8.3 disabled on this volume: the fallback branch can't be
                // forced this way here (Task 6's `unbuildable` case).
                std::fs::remove_dir_all(&base).ok();
                return;
            }
        };

        let root_str = root_dir.to_str().unwrap().to_string();
        let map = RootMap::new(&root_str, VolumeMap::empty()).unwrap();
        let raw = format!(r"{short_outside}\secret.txt");
        assert!(
            !map.contains(&raw),
            "a path outside the root was pulled inside via the 8.3 fallback: {raw}"
        );

        std::fs::remove_dir_all(&base).ok();
    }
}

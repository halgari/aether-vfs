//! [`RootMap`]: which declared root an NT path falls under, and the decision for it.

use std::sync::atomic::{AtomicU64, Ordering};

use vfs_core::{fold, normalize_vpath, PathError};

use crate::cache::{cache_suppressed, OsConsultGuard, PathCache, DEFAULT_CACHE_CAPACITY};
use crate::{canonicalise, expand_short_name, RootHit, RootId, VolumeMap};

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
    pub(crate) fn new_with_cache_capacity(
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
    pub(crate) fn cache_len(&self) -> usize {
        self.cache.len()
    }

    /// The number of lookups so far that consulted the OS. Test-only, to
    /// prove an OS-consulted answer was recomputed rather than served from
    /// the cache.
    #[cfg(test)]
    pub(crate) fn os_consult_count(&self) -> u64 {
        self.os_consults.load(Ordering::Relaxed)
    }

    /// The number of times `compute_under_root` actually ran (i.e. cache
    /// misses, of either `Resolution` variant). Test-only, to prove a lookup
    /// was recomputed rather than served from the cache — see this struct's
    /// `computes` field doc comment.
    #[cfg(test)]
    pub(crate) fn compute_count(&self) -> u64 {
        self.computes.load(Ordering::Relaxed)
    }

    /// The normalized components of the deepest declared root (original case).
    /// For tests/diagnostics.
    pub fn root_components(&self) -> &[String] {
        self.roots
            .first()
            .map(|r| r.comps.as_slice())
            .unwrap_or(&[])
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
    /// matched, or `None` if it is outside/malformed. Callers that need to know *which* root want
    /// [`Self::resolve`].
    pub fn remainder(&self, nt_path: &str) -> Option<Vec<String>> {
        self.under_root(nt_path).map(|(_, rest)| rest)
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
        let comps: Vec<&str> = if canon.is_empty() {
            Vec::new()
        } else {
            canon.split('/').collect()
        };
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

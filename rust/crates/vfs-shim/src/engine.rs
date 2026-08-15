//! The redirect engine: a `RootMap` plus the snapshot bytes it resolves against.

use std::cell::Cell;
use std::io::Write;
use std::path::Path;
use std::sync::OnceLock;

use vfs_redirect::{classify_open, to_nt, Decision, DirItem, RootId, RootMap, VolumeMap};
use vfs_shared::{LayoutError, SnapshotReader};

use crate::hookstats::OverlayFail;
use crate::overlay::{Overlay, OverlayState};

thread_local! {
    /// Guards [`Engine::map`]'s lazy `RootMap` resolution against re-entering
    /// itself on the same thread.
    ///
    /// `resolve_volume_map`'s junction scan makes real Win32 calls
    /// (`vfs_win::reparse_point_target`'s `CreateFileW`, directory listings)
    /// while resolving the alias table. Inside an injected process whose own
    /// file APIs are hooked — this crate's whole reason for existing — those
    /// calls are themselves intercepted and fed back through the very same
    /// chain (hook -> `Engine::decide`/`overlay_state` -> `Engine::map`)
    /// on the *same thread*, before the first call's
    /// `OnceLock::get_or_init` closure has returned. `std::sync::Once`
    /// documents same-thread reentrant `call_once` as unspecified behaviour
    /// ("a panic or a deadlock") — verified by reproduction, not assumed:
    /// this exact shape hung the escape-matrix e2e test's injected fixture
    /// process (high, sustained CPU use, not a clean crash — consistent with
    /// undefined behaviour at the FFI boundary a panic there would cross)
    /// before this guard existed. Same failure class, same fix shape, as
    /// `vfs-redirect`'s own `OS_CONSULT_DEPTH`/`OsConsultGuard` for the
    /// unrelated (but structurally identical) 8.3-short-name OS-consult
    /// reentrancy this project already found and fixed once.
    ///
    /// The break: a reentrant call finds the guard already held and
    /// `Engine::map` answers `None` ("not ready yet") instead of touching
    /// the still-initializing `OnceLock` again — every caller already has a
    /// fail-safe `PassThrough`/`false`/`None` fallback for "not resolved",
    /// the same shape used everywhere else in this file for "outside the
    /// root" or "no overlay". The nested Win32 call's own hook invocation
    /// then takes that fall-through path and reaches the real, unhooked
    /// syscall, so `reparse_point_target`'s handle operations still complete
    /// normally — nothing is answered incorrectly, only the nested attempt
    /// to finish initializing `self.map` a second time is skipped.
    static MAP_INIT_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// RAII guard for [`MAP_INIT_DEPTH`]. `enter()` returns `None` when the guard
/// is already held on this thread — the caller's signal to answer "not ready
/// yet" rather than recurse into the still-initializing `RootMap`.
struct MapInitGuard(());

impl MapInitGuard {
    fn enter() -> Option<Self> {
        MAP_INIT_DEPTH.with(|c| {
            if c.get() > 0 {
                None
            } else {
                c.set(1);
                Some(MapInitGuard(()))
            }
        })
    }
}

impl Drop for MapInitGuard {
    fn drop(&mut self) {
        MAP_INIT_DEPTH.with(|c| c.set(c.get().saturating_sub(1)));
    }
}

/// Bytes pulled from the director per `FuseClient::read_fragmented` call
/// during copy-up. Not the transfer unit: the client fragments this again to
/// fit the ring's payload cap (or an arena bank), so one call here is already
/// several round trips for anything sizeable. It only bounds how much of the
/// file this crate holds in memory at once — a Skyrim BSA is gigabytes and
/// must not be buffered whole. Heap, not stack: game threads reach this from
/// inside a hook, and SkyrimSE's primary thread ships a 1 MiB PE stack.
const SEED_CHUNK: usize = 256 * 1024;

/// Errors constructing an [`Engine`].
#[derive(Debug)]
pub enum EngineError {
    /// The managed root path could not be normalized.
    Root(vfs_core::PathError),
    /// The snapshot bytes failed layout validation.
    Snapshot(LayoutError),
}

/// What [`Engine::rename`] did, and — the reason this is not a `bool` — what
/// the caller is allowed to do next.
///
/// The middle and last variants used to be the same `false`, which is how a
/// cross-root rename ended up being carried out by the real filesystem. See
/// [`Engine::rename`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameOutcome {
    /// The overlay performed the move. Suppress the real rename and report
    /// success.
    Handled,
    /// Not ours: no overlay is configured, or a side of the move does not
    /// resolve cleanly under any managed root. The real rename may proceed.
    Declined,
    /// Both sides resolve under managed roots, but *different* ones. The
    /// caller must fail the call — never trampoline, or the kernel moves the
    /// file across the root boundary for real.
    CrossRoot,
}

/// Owns the redirect policy, the snapshot it resolves against, and an optional
/// write overlay consulted ahead of the snapshot.
pub struct Engine {
    /// Every managed root this session declared, as `(id, path)`.
    ///
    /// **Several, not one** (gate 4, Task 3). The rest of the stack —
    /// `RootMap`, the director, the ring's `root:u32`, the block cache, this
    /// crate's own `Overlay` — has been root-addressed since stage 2b; this
    /// was the last single-root thing, and it is the one that decides what
    /// happens when the director does *not* answer. While it knew only root
    /// 0, a path under root ≥1 was `Outside` to it, so a write that missed
    /// the director landed on real disk with no redirect and no deny.
    roots: Vec<(RootId, String)>,
    /// The volume-aware `RootMap`, built lazily — see [`Engine::map`] for why
    /// this is deferred past `build()` rather than eager.
    map: OnceLock<RootMap>,
    snapshot: Vec<u8>,
    overlay: Option<Overlay>,
}

impl Engine {
    /// `root` may be NT (`\??\C:\Games\Skyrim`) or Win32 (`C:\Games\Skyrim`).
    /// The snapshot is validated eagerly so `decide` can stay infallible.
    /// No write overlay (read-only VFS).
    ///
    /// Single-root shorthand for [`Engine::with_roots`] — the shape most
    /// tests and every pre-stage-2b caller want.
    pub fn new(root: &str, snapshot: Vec<u8>) -> Result<Self, EngineError> {
        Self::build(&[(RootId::DEFAULT, root.to_string())], None, snapshot)
    }

    /// Like [`Engine::new`] but with an on-disk write overlay (create/modify/
    /// delete land there; reads resolve overlay-first).
    pub fn with_overlay(
        root: &str,
        overlay_root: &str,
        snapshot: Vec<u8>,
    ) -> Result<Self, EngineError> {
        Self::build(&[(RootId::DEFAULT, root.to_string())], Some(overlay_root), snapshot)
    }

    /// Every root the session declared, `(id, path)`. Two entries may share an
    /// id to declare an alias, exactly as [`RootMap::with_roots`] defines it.
    ///
    /// `bootstrap.rs` builds this list with the *same* function the FUSE
    /// client uses (`fuse_client::roots_from_env`), so the two halves of the
    /// shim cannot disagree about which roots exist.
    pub fn with_roots(roots: &[(RootId, String)], snapshot: Vec<u8>) -> Result<Self, EngineError> {
        Self::build(roots, None, snapshot)
    }

    /// [`Engine::with_roots`] plus an on-disk write overlay. The overlay is
    /// root-scoped (`overlay_layer_dir`), so each root's writes land in their
    /// own subtree and identical relative paths under different roots never
    /// share a file.
    pub fn with_roots_and_overlay(
        roots: &[(RootId, String)],
        overlay_root: &str,
        snapshot: Vec<u8>,
    ) -> Result<Self, EngineError> {
        Self::build(roots, Some(overlay_root), snapshot)
    }

    fn build(
        roots: &[(RootId, String)],
        overlay_root: Option<&str>,
        snapshot: Vec<u8>,
    ) -> Result<Self, EngineError> {
        // An engine with no roots would classify every path as `Outside` and
        // therefore virtualize nothing at all — the "content simply missing"
        // failure this project keeps rediscovering, but total. `RootMap` will
        // happily build an empty map, so reject it here.
        if roots.is_empty() {
            return Err(EngineError::Root(vfs_core::PathError::EmptyRoot));
        }
        // Validate the root shapes eagerly, so a bad root is still reported
        // from the constructor immediately (unchanged observable behaviour)
        // — but with an empty `VolumeMap`, since this call exists only to
        // surface `PathError`, not to build the real, volume-aware map the
        // engine will actually use. See `Engine::map` for why the real one is
        // deferred. `with_roots` fails on the first path that will not
        // normalize rather than dropping it, so a second root that is
        // malformed is reported here rather than going quietly missing.
        RootMap::with_roots(&Self::refs(roots), VolumeMap::empty()).map_err(EngineError::Root)?;
        SnapshotReader::open(&snapshot).map_err(EngineError::Snapshot)?;
        let overlay = overlay_root.map(Overlay::new);
        Ok(Engine { roots: roots.to_vec(), map: OnceLock::new(), snapshot, overlay })
    }

    /// Borrowed view of `roots` in the shape `RootMap::with_roots` takes.
    fn refs(roots: &[(RootId, String)]) -> Vec<(RootId, &str)> {
        roots.iter().map(|(id, p)| (*id, p.as_str())).collect()
    }

    /// The volume-aware `RootMap`, built on first use and memoized for the
    /// rest of the engine's life — exactly once per session, same cost as
    /// building it eagerly in `build()`, but deliberately *not* eager.
    ///
    /// `resolve_volume_map`'s junction scan (escape-matrix vector 7) reads
    /// the real filesystem at the moment it runs. `build()` runs from the
    /// shim's own DLL bootstrap, which the injector guarantees completes
    /// (and hooks go live) *before* the target's own `main()` executes any
    /// application code — see `vfs-shim-dll`'s module doc. A junction a game
    /// or mod manager already has in place before the game process even
    /// starts is unaffected either way: bootstrap-time and first-decision-
    /// time are the same instant for all practical purposes, both well
    /// before the game does anything with the filesystem. The two moments
    /// only diverge for an artificial case this project's own escape-matrix
    /// fixture happens to construct: a junction created *by the injected
    /// process's own later code*, after bootstrap already ran. Deferring to
    /// first use costs nothing extra in the real-world case (still one
    /// resolution for the session) while no longer silently assuming
    /// injection-time is early enough to see everything that will ever
    /// matter — a strictly more honest reading of "session start" than
    /// "the instant the DLL loads".
    ///
    /// `None` only while the *first-ever* call on this engine is still in
    /// progress and a nested, same-thread call reaches this function again
    /// before that first call returns — see [`MAP_INIT_DEPTH`]. Every
    /// caller treats `None` exactly like "not under any managed root",
    /// which is always a safe answer for an open the shim's own resolution
    /// machinery made about itself, not the game.
    fn map(&self) -> Option<&RootMap> {
        if let Some(m) = self.map.get() {
            return Some(m);
        }
        let _guard = MapInitGuard::enter()?;
        Some(self.map.get_or_init(|| {
            // Scoped to *every* declared root, not just root 0: a junction or
            // volume-GUID spelling under the second root needs the same alias
            // table the first one does. `FuseClient::connect` scans the same
            // way for the same reason.
            let scan: Vec<&str> = self.roots.iter().map(|(_, p)| p.as_str()).collect();
            let volumes = vfs_redirect::resolve_volume_map_for(&scan);
            // The one panic site in this crate's production code, and it is
            // dead: `build()` already ran this exact call over these exact
            // strings, and `RootMap::with_capacity`'s only two error exits
            // (`normalize_vpath`, and the empty-normalisation check) never
            // read `volumes` — so its `VolumeMap::empty()` and this real one
            // cannot disagree. **That is the invariant to preserve:** adding
            // volume-dependent root validation to `vfs-redirect` makes this
            // reachable, and reachable here means an abort inside the game.
            // Deliberately not softened to `None`: `map()` returning `None`
            // sends `decide_open` to `Decision::PassThrough`, trading a dead
            // abort for a live real-disk fall-through. See §1 of
            // `rust/docs/audit-2026-08-13.md`.
            RootMap::with_roots(&Self::refs(&self.roots), volumes)
                .expect("root shapes already validated by build()'s empty-VolumeMap check")
        }))
    }

    /// The overlay resolution for `nt_path`, if an overlay is configured, the
    /// engine's `RootMap` is currently available (see [`Engine::map`]), and
    /// the path is under the root.
    ///
    /// `pub(crate)` (rather than private): since Task 4 deleted
    /// `RootMap::query_attributes` and `AttrDecision` — the local
    /// snapshot-backed attribute answering this crate used to fall back to —
    /// the shim-local write overlay is the only thing left that can still
    /// answer an attribute query without asking the director. `hook.rs`'s
    /// `qattr_hook`/`qfull_hook`/`qibn_hook` call this directly for exactly
    /// that: a file just created/modified through the overlay write
    /// fallback (gate 4's mechanism, untouched by Task 4) must still report
    /// sane attributes even though the director has never heard of it.
    pub(crate) fn overlay_state(&self, nt_path: &str) -> Option<OverlayState> {
        let ov = self.overlay.as_ref()?;
        let (root, comps) = self.resolve(nt_path)?;
        Some(ov.lookup(root, &comps))
    }

    /// Which declared root `nt_path` falls under, plus its folded remainder
    /// beneath that root — `None` when it is outside every root, malformed,
    /// escaping, or the `RootMap` is not yet available (see [`Engine::map`]).
    ///
    /// **Every root-addressed entry point goes through here**, so the id an
    /// overlay call receives is always the one the path actually resolved
    /// under. That matters more than it looks: an overlay call left at
    /// `RootId::DEFAULT` compiles, passes every single-root test, and quietly
    /// files root 1's writes under root 0 — the collision `Overlay` was made
    /// root-scoped to prevent. `RootMap::remainder`, which throws the id away,
    /// is deliberately not used anywhere in this file for that reason.
    fn resolve(&self, nt_path: &str) -> Option<vfs_redirect::RootHit> {
        self.map()?.resolve(nt_path)
    }

    /// Decide how to handle an incoming NT open path. Overlay-first, then
    /// snapshot. Fail-safe: if the snapshot somehow fails to re-open, or the
    /// `RootMap` is not currently available (see [`Engine::map`]), pass
    /// through.
    ///
    /// **The snapshot answers for root 0 only.** It is one flat vpath tree
    /// with no root dimension — the composition published for the managed
    /// game directory — so resolving root 1's remainder against it would
    /// answer `<root1>\Data\foo.esp` with root *0*'s backing file. That is a
    /// worse failure than the pass-through this became multi-root to remove:
    /// serving one root's bytes under another root's name. So a read under
    /// root ≥1 that the overlay does not answer is sealed (`Deny`) — the same
    /// verdict gate 3 gives any under-root path the provider graph cannot
    /// vouch for, and the same one root 0 gets from the empty snapshot a
    /// director session actually ships (`Session::serve` writes
    /// `empty_tree_snapshot()`; the director's ring, not this snapshot, is
    /// what answers reads in a live session).
    pub fn decide(&self, nt_path: &str) -> Decision {
        match self.overlay_state(nt_path) {
            Some(OverlayState::Present { path, .. }) => {
                return Decision::Redirect { target_nt: to_nt(&path.to_string_lossy()) }
            }
            Some(OverlayState::Whiteout) => return Decision::Deny,
            Some(OverlayState::Absent) | None => {}
        }
        let Some(map) = self.map() else { return Decision::PassThrough };
        match map.resolve(nt_path) {
            // Outside every declared root: never ours to touch.
            None => Decision::PassThrough,
            Some((root, _)) if root != RootId::DEFAULT => Decision::Deny,
            Some(_) => match SnapshotReader::open(&self.snapshot) {
                Ok(reader) => map.decide(nt_path, &reader),
                Err(_) => Decision::PassThrough,
            },
        }
    }

    /// Decide how to handle an open given its desired-access mask and create
    /// disposition. Reads use the overlay-first read resolution; writes (with an
    /// overlay configured) redirect to the overlay, copy-on-write materializing
    /// existing content first. Without an overlay, writes pass through.
    pub fn decide_open(&self, nt_path: &str, access: u32, disposition: u32) -> Decision {
        let intent = classify_open(access, disposition);
        if !intent.write {
            return self.decide(nt_path);
        }
        let ov = match &self.overlay {
            Some(o) => o,
            None => return Decision::PassThrough,
        };
        let (root, comps) = match self.resolve(nt_path) {
            Some((r, c)) if !c.is_empty() => (r, c),
            _ => return Decision::PassThrough,
        };
        // Both of these used to be `let _ = …`. See [`Engine::overlay_fs`] for
        // why the discarded results and the missing reentrancy guard were each
        // a defect in their own right. The `Decision` below is deliberately
        // unchanged on failure: a redirect into a directory that could not be
        // created still fails with the honest `STATUS_OBJECT_PATH_NOT_FOUND`,
        // and anything else here (`PassThrough`) would put the write on real
        // disk *under a managed root*, which is the escape this gate closed.
        // What was missing was any record that it happened.
        self.overlay_fs(OverlayFail::EnsureParent, root, &comps, || {
            ov.ensure_parent(root, &comps)
        });
        // Recreating the path: drop any whiteout so it is visible again.
        self.overlay_fs(OverlayFail::ClearWhiteout, root, &comps, || {
            ov.clear_whiteout(root, &comps)
        });
        // Copy-on-write: preserve existing content into the overlay before the
        // caller writes, unless it is truncating/replacing (no copy needed) or a
        // copy already exists.
        if intent.preserves && !ov.has_file(root, &comps) {
            let dest = ov.file_path(root, &comps);
            self.copy_up(root, nt_path, &comps, &dest);
        }
        Decision::Redirect {
            target_nt: to_nt(&ov.file_path(root, &comps).to_string_lossy()),
        }
    }

    /// Run one shim-local overlay filesystem mutation, guarded and counted.
    /// Returns whether it happened.
    ///
    /// **The guard.** These run from inside `create_hook`/`setinfo_hook`,
    /// where no reentrancy guard is held — `create_hook` only takes its
    /// `in_hook_reenter` fast path for calls made *while* one is. So
    /// `create_dir_all`, `remove_file`, `write` and `rename` below all issue
    /// NT calls that our own detours then re-decide: `create_dir_all` in
    /// particular issues a `FILE_DIRECTORY_FILE` create per component, which
    /// `try_fuse_mkdir` will happily route into the director if the overlay
    /// directory happens to sit under a managed root. The overlay is a real
    /// directory and its mutations must reach the real filesystem, so this
    /// holds [`crate::hook::ShimIoGuard`] across them — the same guard, for
    /// the same reason, that `cow_seed` holds while it writes its
    /// destination.
    ///
    /// **The counter.** Every one of these used to be `let _ = …`. A
    /// discarded failure here is invisible from every other vantage point in
    /// the shim: see [`crate::hookstats::OverlayFail`] for what each one
    /// costs, and why a guard alone would have fixed only half the problem.
    fn overlay_fs<F>(&self, fail: OverlayFail, root: RootId, comps: &[String], f: F) -> bool
    where
        F: FnOnce() -> std::io::Result<()>,
    {
        let Some(_io) = crate::hook::ShimIoGuard::enter() else {
            crate::hookstats::note_overlay_fail(
                OverlayFail::DeclinedReentrant,
                root.0,
                &comps.join("/"),
            );
            return false;
        };
        match f() {
            Ok(()) => true,
            Err(_) => {
                crate::hookstats::note_overlay_fail(fail, root.0, &comps.join("/"));
                false
            }
        }
    }

    /// The copy-up policy both call sites share: decline outright for a named
    /// alternate data stream, otherwise seed through the director.
    ///
    /// Copy-up is best-effort by design and the caller ignores the result —
    /// a director that will not hand over the existing content leaves the
    /// overlay file to be created empty by the write itself, which is the same
    /// thing the path *reads* as. What it must never do is fall back to real
    /// disk under the root (see [`Engine::cow_seed`]). But "best-effort" used
    /// to mean "silent": nothing recorded that the content went missing, and
    /// this gate's defects have all been the kind a green test suite cannot
    /// see. Every outcome is now counted and named in the shim's own stats
    /// report ([`crate::hookstats::CopyUp`]), including the successes, so an
    /// empty file in a live session is explainable from the report rather than
    /// by bisection.
    ///
    /// **The stream case.** `Engine::resolve` goes through
    /// `RootMap::canonicalise`, which discards a `:stream` suffix — correctly
    /// for its own purpose, since `f.esp:s` and `f.esp` are spellings of the
    /// same *file*. But they are not the same *content*, and copy-up consumes
    /// the remainder, so seeding a preserving write to `f.esp:s` would fill it
    /// with `f.esp`'s bytes. That is the same mistake as answering a read of
    /// `f.esp:probe` with `f.esp` — a containment bug this project has already
    /// had once, which is why `FuseClient::vpath_under_root` re-attaches the
    /// suffix on the read path. Declining is the honest answer: nothing here
    /// knows what a named stream's prior content is, and inventing stream
    /// support to find out is a different task. Before this change nothing was
    /// seeded either, because the suffixed vpath came back not-found — so this
    /// preserves the old behaviour rather than restoring something.
    fn copy_up(&self, root: RootId, nt_path: &str, rel: &[String], dest: &Path) {
        use crate::hookstats::{note_copy_up, CopyUp};
        if vfs_redirect::split_stream_suffix(nt_path).1.is_some() {
            note_copy_up(CopyUp::DeclinedStream, root.0, &rel.join("/"), 0);
            return;
        }
        let _ = self.cow_seed(root, rel, dest);
    }

    /// Materialise `(root, rel)`'s existing content at `dest` by reading it
    /// **through the director**. Returns true when `dest` now holds the
    /// director's bytes in full.
    ///
    /// **This used to read the real filesystem.** It re-ran the snapshot-only
    /// decision and copied from whatever that yielded — `std::fs::copy` off a
    /// `Decision::Redirect` target, a zip window off the since-removed
    /// `Decision::Serve`, and
    /// failing both, `std::fs::copy` off the raw NT path being opened. None of
    /// those asked the director anything, so copy-up seeded from content under
    /// a managed root that the invariant says is unreachable: a real file the
    /// provider graph does not serve reads as not-found, and then a preserving
    /// *write* to the same path copied it up anyway and handed the game its
    /// bytes. `vfs-directord`'s escape matrix named this exact hole — its
    /// negative-canary assertion was scoped to reads because the write open
    /// still reached the canary "through `Engine::cow_seed`'s last-resort
    /// branch". This is that branch, gone; the matrix now carries a write
    /// half (`escape_matrix_write_access_positive_and_negative_canary`) that
    /// asserts the other side.
    ///
    /// It also takes the resolved [`RootId`] and the folded remainder rather
    /// than an NT path, so there is no second, private re-derivation of which
    /// root a path belongs to: the id and components are the ones the caller
    /// already resolved through [`Engine::resolve`], and the vpath handed to
    /// the ring is `rel.join("/")` — the same shape
    /// `FuseClient::vpath_under_root` builds.
    ///
    /// Three things this must get right, none of them incidental:
    ///
    /// - **The read spans many round trips.** The ring's payload cap is
    ///   ~1 MiB and a bulk arena bank is capped at 1 MiB per RTT, against
    ///   Skyrim assets measured in gigabytes. The loop below runs to the size
    ///   the OPEN reported, and it re-reads at the new offset when a call
    ///   comes back short — a short read is *not* proof of EOF
    ///   (`read_fragmented` also returns short when one fragment in its own
    ///   batch was partial). Only a zero-length read ends the loop, and
    ///   because that case is treated as failure, the loop cannot spin: every
    ///   iteration either advances `done` or returns.
    /// - **A director error fails the copy-up.** No `std::fs::copy` fallback,
    ///   not even on the error path — a fallback there would restore the
    ///   escape at precisely the moment something had already gone wrong. A
    ///   partially written `dest` is removed, so "false" means the same thing
    ///   it always did: nothing was seeded, and the caller's write starts from
    ///   an empty overlay file.
    /// - **It cannot be re-decided by the hook that called it.**
    ///   `decide_open` runs inside `create_hook`, so `File::create(dest)`
    ///   below is an NT open made from inside a hook — and `dest` is a path
    ///   the shim itself chose, which nothing says cannot be under a managed
    ///   root. Unguarded, that open is answered by the VFS instead of the
    ///   filesystem, and copy-up's bytes land somewhere other than where the
    ///   very same `decide_open` call is about to point the game — the failure
    ///   `cow_seed_reentrancy.rs` reproduces. Note it is *misrouting*, not
    ///   stack exhaustion: `File::create` truncates, so the re-entered
    ///   `decide_open` does not take the `intent.preserves` branch and does
    ///   not recurse. Unbounded recursion is the same family (this project has
    ///   lost a process to it twice: `vfs_redirect`'s `OS_CONSULT_DEPTH` and
    ///   this file's own `MAP_INIT_DEPTH`) and would need a preserving
    ///   shim-issued open on an under-root destination — not reachable today,
    ///   and not something to leave depending on which disposition a helper
    ///   happens to use. [`crate::hook::ShimIoGuard`] is the crate's existing
    ///   answer — the same counter `create_hook` tests on entry before
    ///   trampolining straight to the real ntdll — held across the whole seed.
    ///   Held rather than declined-on-conflict for the destination's sake:
    ///   while it is up, our own writes reach the real filesystem instead of
    ///   being re-decided.
    ///
    /// Every exit is counted and named in the shim's stats report — see
    /// [`Engine::copy_up`] for why a silent best-effort was not good enough.
    fn cow_seed(&self, root: RootId, rel: &[String], dest: &Path) -> bool {
        use crate::hookstats::{note_copy_up, CopyUp};
        if rel.is_empty() {
            return false;
        }
        let vpath = rel.join("/");
        // No director, no copy-up. The old code's answer here was to read the
        // disk, which is the whole bug; a shim with no ring has no legitimate
        // source for these bytes.
        let Some(client) = crate::fuse_client::global() else {
            note_copy_up(CopyUp::DeclinedNoDirector, root.0, &vpath, 0);
            return false;
        };
        // Already inside shim-initiated I/O on this thread: this call *is* the
        // recursion the guard exists to stop, so decline rather than deepen it.
        let Some(_io) = crate::hook::ShimIoGuard::enter() else {
            note_copy_up(CopyUp::DeclinedReentrant, root.0, &vpath, 0);
            return false;
        };
        let (outcome, bytes) = seed_from_director(client, root, &vpath, dest);
        note_copy_up(outcome, root.0, &vpath, bytes);
        if outcome == CopyUp::Seeded {
            return true;
        }
        // Failed part-way: a truncated seed is worse than none, because the
        // game would edit it believing it whole. Leave the caller the empty
        // overlay file it gets for any other unseedable path.
        //
        // Runs even when `File::create` was never reached (the OPEN failed, or
        // the vpath is a directory), which is safe only because both callers
        // gate on `!ov.has_file(...)`: `dest` did not exist when copy-up
        // started, so there is nothing here for this to destroy. A future
        // caller that seeds over an existing overlay copy must move this
        // cleanup inside the "we created it" case first.
        let _ = std::fs::remove_file(dest);
        false
    }

    /// Whether `nt_path` lies under **any** declared root. `hook.rs`'s
    /// `path_is_ours` is the only caller; see its doc comment for the one
    /// spelling this still answers `false` for that the FUSE client answers
    /// `true` for (the staged-launch alias).
    pub fn is_under_root(&self, nt_path: &str) -> bool {
        self.map().is_some_and(|m| m.contains(nt_path))
    }

    // `pub fn remainder(&self, nt_path) -> Option<Vec<String>>` used to sit
    // here: the remainder with the `RootId` thrown away. It had no callers
    // left, and now that every overlay operation is addressed by
    // `(RootId, comps)` it is a trap rather than a convenience — a future
    // caller reaching for it would be reaching for exactly the root-blind
    // shape this task removed from the four call sites that had it. Ask
    // `RootMap::resolve` (or this engine's own `resolve`) instead, which
    // hands back the id with the components.

    /// Whiteout `nt_path` in the overlay (mark as deleted). Returns whether an
    /// overlay handled it; `false` means the caller should let the real delete
    /// proceed (read-only VFS or path outside the root).
    pub fn whiteout(&self, nt_path: &str) -> bool {
        match (&self.overlay, self.resolve(nt_path)) {
            (Some(ov), Some((root, comps))) if !comps.is_empty() => {
                // `true` even when the marker could not be written: the
                // caller reads `false` as "let the real delete proceed",
                // which under a managed root is the escape, not a fallback.
                // The failure is recorded instead — a whiteout that did not
                // land means the deleted file is still visible.
                self.overlay_fs(OverlayFail::Whiteout, root, &comps, || {
                    ov.whiteout(root, &comps)
                });
                true
            }
            _ => false,
        }
    }

    /// Rename `from_nt` to `to_nt` within the overlay: materialize the source if
    /// needed, move it, and whiteout the old location.
    ///
    /// [`RenameOutcome::Declined`] (no overlay, or a side that is not cleanly
    /// under any root) means the caller may let the real rename proceed.
    /// [`RenameOutcome::CrossRoot`] does **not**: see below.
    ///
    /// A rename whose two sides land under *different* roots is refused rather
    /// than guessed at, matching what `hook.rs` already does one layer up when
    /// the FUSE client answers the same question (`Some((dst_root, dstv)) if
    /// dst_root == root`): `Overlay::rename` moves within one root's subtree,
    /// and there is no cross-root move in the provider contract either.
    /// Picking one of the two ids would file the result under a root that only
    /// half the operation named.
    ///
    /// **Refusing is not the same as declining, and this used to conflate
    /// them** (gate 4, Task 5). Both answered `false`, and `setinfo_hook` reads
    /// `false` as "let the real `NtSetInformationFile` run" — so a cross-root
    /// rename of an overlay-captured file was performed by the kernel, which
    /// physically moved it out of the overlay and onto real disk under the
    /// destination root. The content escaped the VFS, and then read back as
    /// missing, because that root seals every path the provider graph does not
    /// serve. (Across volumes it instead surfaced as a bare
    /// `STATUS_NOT_SAME_DEVICE`, an error the caller has no way to interpret
    /// here.) The distinct variant is what lets the caller fail closed.
    ///
    /// The cross-root check deliberately runs **before** the overlay check: an
    /// engine with no overlay has no capture to lose, but it has the same two
    /// managed roots, and handing that move to the kernel is the same escape.
    pub fn rename(&self, from_nt: &str, to_nt: &str) -> RenameOutcome {
        let from_hit = self.resolve(from_nt).filter(|(_, c)| !c.is_empty());
        let to_hit = self.resolve(to_nt).filter(|(_, c)| !c.is_empty());
        if let (Some((from_root, _)), Some((to_root, _))) = (&from_hit, &to_hit) {
            if from_root != to_root {
                return RenameOutcome::CrossRoot;
            }
        }
        let ov = match &self.overlay {
            Some(o) => o,
            None => return RenameOutcome::Declined,
        };
        let (from_root, from) = match from_hit {
            Some(hit) => hit,
            None => return RenameOutcome::Declined,
        };
        let (_, to) = match to_hit {
            Some(hit) => hit,
            None => return RenameOutcome::Declined,
        };
        self.overlay_fs(OverlayFail::EnsureParent, from_root, &from, || {
            ov.ensure_parent(from_root, &from)
        });
        if !ov.has_file(from_root, &from) {
            let dest = ov.file_path(from_root, &from);
            // Same policy as `decide_open`'s copy-up, through the same
            // function: a director that declines leaves nothing at `dest`, so
            // the rename moves an absent/empty file rather than one seeded off
            // real disk, and the reason is recorded either way. The rename
            // itself is still handled (`true`) — declining here would hand the
            // operation back to the real filesystem, which is the one outcome
            // an under-root path must never get.
            self.copy_up(from_root, from_nt, &from, &dest);
        }
        // Still `Handled` if the move failed, for the same reason `whiteout`
        // still answers `true`: `Declined` hands the rename to the real
        // filesystem under a managed root. The failure is recorded instead.
        self.overlay_fs(OverlayFail::Rename, from_root, &from, || {
            ov.rename(from_root, &from, &to)
        });
        RenameOutcome::Handled
    }

    /// Apply the shim-local write overlay (adds/overrides win, whiteouts
    /// remove) on top of `base`.
    ///
    /// Task 4 deleted `RootMap::merge_directory`, which used to blend the
    /// published snapshot's virtual children in here — a directory listing
    /// under a managed root comes solely from the director's own `readdir`
    /// (see `hook.rs::serve_dir_query`), never from a local snapshot merge.
    /// This method keeps only the overlay half: the write path still needs a
    /// just-created/modified/deleted overlay entry to show up in a listing
    /// the director cannot itself account for.
    ///
    /// **`base` is always empty at the only call site, and that is the point.**
    /// It used to be the real directory's own drained entries, which is how a
    /// real, unserved file under a managed root got listed — gate 4 task 8b
    /// deleted the drain (see `serve_dir_query`, and `docs/escape-matrix.md`'s
    /// "Gate 4, Task 8b"). The parameter is kept rather than dropped because
    /// this method's contract is "overlay on top of whatever the caller
    /// legitimately has", and the unit tests exercise it with a non-empty
    /// base; a caller passing a real-disk listing here would be the
    /// regression, not the signature.
    ///
    /// Fail-safe: no overlay configured, or the `RootMap` not currently
    /// available (see [`Engine::map`]), returns `base` unchanged.
    pub fn overlay_listing(
        &self,
        dir_nt_path: &str,
        base: &[DirItem],
        wildcard: Option<&str>,
    ) -> Vec<DirItem> {
        match (&self.overlay, self.resolve(dir_nt_path)) {
            (Some(ov), Some((root, comps))) => {
                ov.apply_to_listing(root, &comps, base.to_vec(), wildcard)
            }
            _ => base.to_vec(),
        }
    }
}

/// OPEN → read-to-`dest` → CLOSE against the director. Split out of
/// [`Engine::cow_seed`] so the handle is closed on every path out of the read
/// loop, including the failing ones — a leaked `fh` is a provider-side file
/// the director never releases.
///
/// Returns the outcome to record plus the bytes written (0 unless seeded).
fn seed_from_director(
    client: &crate::fuse_client::FuseClient,
    root: RootId,
    vpath: &str,
    dest: &Path,
) -> (crate::hookstats::CopyUp, u64) {
    use crate::hookstats::CopyUp;
    // The shim's own open, not the game's: no `OpenOutcome::Routed` was ever
    // recorded for it, but the director counts it like any other arrival.
    // Counted so the shim/director reconciliation stays an exact equality —
    // see `hookstats::UNROUTED_DIRECTOR_OPENS`. Noted before the call so a
    // refusal (which the director still counts, in `opens_err`) is included.
    crate::hookstats::note_unrouted_director_open();
    let Ok(opened) = client.open(root, vpath) else {
        return (CopyUp::DirectorRefused, 0);
    };
    let out = if opened.is_dir {
        // A directory is not copy-up material; `File::create` on `dest` would
        // otherwise leave a zero-length file standing in for one. Recorded as
        // a refusal rather than a read failure: the director answered
        // correctly, this path just has nothing to copy up.
        (CopyUp::DirectorRefused, 0)
    } else {
        write_director_file(client, opened.fh, opened.size, dest)
    };
    let _ = client.close(opened.fh);
    out
}

/// Stream `size` bytes of `fh` into a freshly created `dest`.
///
/// See [`Engine::cow_seed`] for why the loop is shaped this way: it reads to
/// the size OPEN reported rather than trusting one call, treats a short read
/// as "read again from the new offset" rather than as EOF, and treats a
/// zero-length read (the director having less than it said) as a failure —
/// which is also what makes the loop provably terminate.
fn write_director_file(
    client: &crate::fuse_client::FuseClient,
    fh: u64,
    size: u64,
    dest: &Path,
) -> (crate::hookstats::CopyUp, u64) {
    use crate::hookstats::CopyUp;
    let Ok(mut f) = std::fs::File::create(dest) else {
        return (CopyUp::DestWriteFailed, 0);
    };
    // A zero-byte file in the provider graph copies up as a zero-byte overlay
    // file — the loop below simply does not run. The lower clamp bound only
    // keeps the buffer allocation legal in that case.
    let mut buf = vec![0u8; (size as usize).clamp(1, SEED_CHUNK)];
    let mut done: u64 = 0;
    while done < size {
        let want = ((size - done) as usize).min(buf.len());
        let Ok(n) = client.read_fragmented(fh, done, &mut buf[..want]) else {
            return (CopyUp::ReadFailed, done);
        };
        if n == 0 {
            return (CopyUp::ReadFailed, done);
        }
        if f.write_all(&buf[..n]).is_err() {
            return (CopyUp::DestWriteFailed, done);
        }
        done += n as u64;
    }
    if f.flush().is_err() {
        return (CopyUp::DestWriteFailed, done);
    }
    (CopyUp::Seeded, done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs_redirect::Decision;

    fn snapshot_bytes() -> Vec<u8> {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "data/foo.esp".into(),
                kind: EntryKind::File,
                source: r"D:\Mods\Cool\foo.esp".into(),
                size: 10,
                mtime: 1,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    }

    #[test]
    fn new_rejects_a_bad_snapshot() {
        // Use `matches!` on the whole Result rather than `.unwrap_err()` — the
        // latter needs `Engine: Debug`, but Engine holds a `Vec<u8>` snapshot we
        // don't want dumped, so Engine intentionally does not derive Debug.
        assert!(matches!(
            Engine::new(r"C:\Games\Skyrim", vec![0u8; 4]),
            Err(EngineError::Snapshot(_))
        ));
    }

    /// A root list that declares nothing would make every path `Outside` and
    /// virtualize precisely nothing — the total form of the "content simply
    /// missing" failure. `RootMap` builds an empty map happily, so `Engine`
    /// rejects it at construction.
    #[test]
    fn with_roots_rejects_an_empty_root_list() {
        assert!(matches!(
            Engine::with_roots(&[], snapshot_bytes()),
            Err(EngineError::Root(vfs_core::PathError::EmptyRoot))
        ));
    }

    #[test]
    fn decide_redirects_a_virtual_file() {
        let engine = Engine::new(r"\??\C:\Games\Skyrim", snapshot_bytes()).unwrap();
        let d = engine.decide(r"\??\C:\Games\Skyrim\Data\foo.esp");
        assert_eq!(d, Decision::Redirect { target_nt: r"\??\D:\Mods\Cool\foo.esp".to_string() });
    }

    #[test]
    fn decide_passes_through_outside_root() {
        let engine = Engine::new(r"\??\C:\Games\Skyrim", snapshot_bytes()).unwrap();
        assert_eq!(engine.decide(r"\??\C:\Windows\notepad.exe"), Decision::PassThrough);
    }

    #[test]
    fn is_under_root_predicate() {
        let engine = Engine::new(r"\??\C:\Games\Skyrim", snapshot_bytes()).unwrap();
        assert!(engine.is_under_root(r"\??\C:\Games\Skyrim\Data\foo.esp"));
        assert!(!engine.is_under_root(r"\??\C:\Windows\notepad.exe"));
    }

    /// Gate 4, Task 5. A rename whose two sides land under *different* managed
    /// roots is [`RenameOutcome::CrossRoot`], never `Declined` — the two used
    /// to be the same `false`, and `setinfo_hook` reads a decline as
    /// permission to run the real `NtSetInformationFile`, which physically
    /// moves the file across the root boundary.
    #[test]
    fn a_cross_root_rename_is_refused_rather_than_declined() {
        let base = std::env::temp_dir()
            .join(format!("vfs-engine-xroot-{}", std::process::id()));
        let root0 = base.join("root0");
        let root1 = base.join("root1");
        let overlay = base.join("overlay");
        std::fs::create_dir_all(&root0).unwrap();
        std::fs::create_dir_all(&root1).unwrap();
        std::fs::create_dir_all(&overlay).unwrap();
        let roots = [
            (RootId::DEFAULT, root0.to_string_lossy().into_owned()),
            (RootId(1), root1.to_string_lossy().into_owned()),
        ];
        let from = format!(r"\??\{}", root1.join("a.txt").display());
        let to = format!(r"\??\{}", root0.join("b.txt").display());

        let with_overlay = Engine::with_roots_and_overlay(
            &roots,
            &overlay.to_string_lossy(),
            snapshot_bytes(),
        )
        .unwrap();
        assert_eq!(with_overlay.rename(&from, &to), RenameOutcome::CrossRoot);

        // And with no overlay at all: an engine with nothing to capture the
        // move still must not hand it to the kernel, because the two roots are
        // just as real. This is why the cross-root check runs *before* the
        // overlay check rather than after it.
        let no_overlay = Engine::with_roots(&roots, snapshot_bytes()).unwrap();
        assert_eq!(
            no_overlay.rename(&from, &to),
            RenameOutcome::CrossRoot,
            "with no overlay the cross-root check was skipped, so the real rename would run"
        );

        // The control: within one root it is still handled, so the assertions
        // above are not just \"rename never works\".
        let same_root = format!(r"\??\{}", root1.join("c.txt").display());
        assert_eq!(with_overlay.rename(&from, &same_root), RenameOutcome::Handled);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Gate 3, Task 5's Step 1: the failing test written first. A REAL file,
    /// physically on disk under a real managed root, that no provider serves
    /// (the snapshot has an entry for a completely different vpath, so this
    /// file is not even a `Dir` node in the tree) must be unreachable through
    /// `Engine::decide` -- before this task's fix this returned
    /// `Decision::PassThrough`, and `hook::create_hook`/`open_hook` trampoline
    /// on exactly that verdict, opening the real bytes below.
    #[test]
    fn real_on_disk_file_under_root_not_in_snapshot_is_denied() {
        let dir = std::env::temp_dir()
            .join(format!("vfs-engine-negcanary-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Data")).unwrap();
        let real_file = dir.join("Data").join("negative-canary.bin");
        std::fs::write(&real_file, b"the real bytes physically on disk").unwrap();
        assert!(real_file.is_file(), "setup: the real file must actually exist");

        let engine = Engine::new(&dir.to_string_lossy(), snapshot_bytes()).unwrap();
        let path = format!(r"\??\{}", real_file.to_string_lossy());
        assert_eq!(
            engine.decide(&path),
            Decision::Deny,
            "a real, on-disk file under the root with no provider must be denied, not opened"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Gate 3's own predicted consequence (gate 2's final review; see
    /// `rust/docs/escape-matrix.md`'s "Mod Organizer exposure" note): a
    /// junction *inside* the managed root pointing at external staging
    /// content is a common real-world Skyrim/MO2 layout. Verified here with a
    /// REAL junction (`mklink /J`), not merely reasoned about: the junction
    /// makes the target's bytes genuinely, transparently reachable by
    /// `std::fs` (proving the content really is there and the OS really does
    /// resolve it), while `Engine::decide` -- operating on the same literal,
    /// unresolved path string a real hooked open would present, exactly as
    /// `RootMap::compute_under_root` does not follow junctions -- now denies
    /// it. Before this task's fix this would have been `PassThrough`, and the
    /// junction's transparency at the kernel level is exactly why that used
    /// to work: the shim never needed to resolve the junction itself, because
    /// passing through let the OS do it. Removing the passthrough seals that
    /// content along with everything else `NotFound` covers, which is the gate
    /// 2 review's prediction confirmed by reproduction rather than assumed.
    #[test]
    #[cfg(windows)]
    fn mo2_style_junction_inside_root_pointing_to_external_staging_is_sealed() {
        let base = std::env::temp_dir()
            .join(format!("vfs-engine-mo2-junction-{}", std::process::id()));
        let root_dir = base.join("root");
        // Deliberately NOT under root, and NOT mounted as a provider anywhere
        // -- the external mod-staging directory an MO2-style setup points at.
        let staging_dir = base.join("staging");
        std::fs::create_dir_all(root_dir.join("Data")).unwrap();
        std::fs::create_dir_all(&staging_dir).unwrap();
        const BYTES: &[u8] = b"the mo2-staged mod's real bytes";
        std::fs::write(staging_dir.join("mo2-mod.esp"), BYTES).unwrap();

        let junction = root_dir.join("Data").join("SomeMod");
        let ok = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &junction.to_string_lossy(),
                &staging_dir.to_string_lossy(),
            ])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            // mklink unavailable/needs a privilege this account lacks: nothing
            // to test here, same convention as this crate's own 8.3 tests.
            std::fs::remove_dir_all(&base).ok();
            return;
        }

        let via_junction = junction.join("mo2-mod.esp");
        // Proves the junction is real and the OS really does resolve it
        // transparently -- the exact mechanism the old passthrough relied on.
        assert_eq!(
            std::fs::read(&via_junction).unwrap(),
            BYTES,
            "setup: the junction must transparently resolve to the staging dir's real bytes"
        );

        let engine = Engine::new(&root_dir.to_string_lossy(), snapshot_bytes()).unwrap();
        let path = format!(r"\??\{}", via_junction.to_string_lossy());
        assert_eq!(
            engine.decide(&path),
            Decision::Deny,
            "an MO2-style junction's real, externally-staged content must now be sealed \
             -- mounting the staging directory as a provider is the required fix, not \
             restoring the passthrough (see rust/docs/escape-matrix.md)"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    // Task 4 removed `merge_directory_adds_virtual_children` and
    // `query_attributes_reports_virtual_file`: both asserted that a plain,
    // no-overlay `Engine` answers a directory listing / attribute query from
    // its local snapshot alone (no director involved). That is exactly the
    // local-answering path Task 4 deletes — `RootMap::merge_directory` and
    // `RootMap::query_attributes` no longer exist for `Engine` to call.
    // A directory listing under a managed root is now the director's
    // `readdir` alone (`hook.rs::serve_dir_query`); an attribute query is the
    // director's `getattr` alone (`hook.rs::fuse_path_attr`). Neither is
    // reachable from this crate's fast, no-director unit tests without a
    // live ring, so there is no in-crate equivalent to port these two to —
    // see the task report for this named gap. `overlay_listing_adds_overlay_children`
    // below replaces the still-relevant half: the shim-local write overlay
    // (gate 4's mechanism, unaffected by Task 4) must still contribute to a
    // listing regardless of what answers the rest of it.
    #[test]
    fn overlay_listing_adds_overlay_children() {
        use vfs_redirect::DirItem;
        let dir = std::env::temp_dir().join(format!("vfs-ovl-listing-{}", std::process::id()));
        // Root-scoped on-disk layout (see `Overlay::root_dir`): this engine
        // declares one root, so everything it resolves is root 0's.
        std::fs::create_dir_all(dir.join("root-0").join("data")).unwrap();
        std::fs::write(dir.join("root-0").join("data").join("overlaid.txt"), b"x").unwrap();
        let engine = overlay_engine(&dir);
        let real = vec![DirItem { name: "real.txt".into(), is_dir: false, size: 1, mtime: 0 }];
        let listed = engine.overlay_listing(r"\??\C:\Games\Skyrim\Data", &real, None);
        let names: Vec<&str> = listed.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"real.txt"), "real entry dropped: {names:?}");
        assert!(names.contains(&"overlaid.txt"), "overlay entry missing: {names:?}");
        // The snapshot's "foo.esp" must NOT appear: no director, no overlay
        // entry for it -- nothing answers for it anymore.
        assert!(!names.contains(&"foo.esp"), "snapshot leaked into the listing: {names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Overlay-aware resolution (W1). Uses a real temp overlay directory.
    fn overlay_engine(overlay_dir: &std::path::Path) -> Engine {
        Engine::with_overlay(
            r"\??\C:\Games\Skyrim",
            overlay_dir.to_str().unwrap(),
            snapshot_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn overlay_file_wins_over_snapshot() {
        let dir = std::env::temp_dir().join(format!("vfs-ovl-win-{}", std::process::id()));
        // Overlay copy of data/foo.esp (folded components on disk), under
        // root 0's subdirectory (see `Overlay::root_dir`) — `overlay_engine`
        // declares one root, so root 0 is the only one it can resolve to.
        std::fs::create_dir_all(dir.join("root-0").join("data")).unwrap();
        std::fs::write(dir.join("root-0").join("data").join("foo.esp"), b"overlaid").unwrap();
        let engine = overlay_engine(&dir);
        let d = engine.decide(r"\??\C:\Games\Skyrim\Data\foo.esp");
        match d {
            Decision::Redirect { target_nt } => {
                assert!(target_nt.to_lowercase().contains("vfs-ovl-win"), "{target_nt}");
                assert!(target_nt.starts_with(r"\??\"));
            }
            other => panic!("expected overlay redirect, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overlay_whiteout_hides_snapshot_file() {
        use crate::overlay::OverlayState;
        let dir = std::env::temp_dir().join(format!("vfs-ovl-wh-{}", std::process::id()));
        // Root 0's subdirectory (see `Overlay::root_dir`) — `overlay_engine`
        // declares one root, so root 0 is the only one it can resolve to.
        std::fs::create_dir_all(dir.join("root-0").join("data")).unwrap();
        // Whiteout marker for data/foo.esp.
        std::fs::write(dir.join("root-0").join("data").join("foo.esp.__vfs_wh__"), b"").unwrap();
        let engine = overlay_engine(&dir);
        assert_eq!(engine.decide(r"\??\C:\Games\Skyrim\Data\foo.esp"), Decision::Deny);
        // `AttrDecision`/`Engine::query_attributes` are gone (Task 4); the
        // overlay's own whiteout state is what a caller now consults for an
        // attribute-query fallback (see `hook.rs`'s qattr/qfull/qibn hooks).
        assert!(matches!(
            engine.overlay_state(r"\??\C:\Games\Skyrim\Data\foo.esp"),
            Some(OverlayState::Whiteout)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The gap `a_write_under_a_second_root_passes_through_to_real_disk_today`
    /// recorded, now closed. That test asserted the opposite of this one on
    /// purpose ("when `Engine` becomes multi-root this fails — that is the
    /// point"), and this is the rewrite it asked for.
    ///
    /// Three things have to hold at once, and the contrast between them is
    /// the whole test — asserting root 1 alone could not tell "both roots
    /// handled correctly" from "both roots broken the same way":
    ///
    /// 1. A write under root 0 is captured by the overlay (unchanged).
    /// 2. The identical write under root 1 is captured too — no `PassThrough`
    ///    to real disk, which is the hole this closes.
    /// 3. The two land in *different* overlay files. A `RootMap` that learned
    ///    root 1 but an overlay call still hardcoding `RootId::DEFAULT` would
    ///    satisfy (1) and (2) and still be wrong: both roots' `Data\w.ini`
    ///    would collide on one file (the collision `Overlay` was made
    ///    root-scoped to prevent).
    ///
    /// The fourth assertion is the control: an engine told about root 0 only
    /// still passes root 1 through, because an *undeclared* root genuinely is
    /// outside — `Session::declare_root`'s "mount without declaring and the
    /// shim never classifies any path into that root at all". Declaring is
    /// what changes the answer here, not guessing.
    #[test]
    fn a_write_under_a_second_root_lands_in_that_root_s_overlay() {
        use crate::overlay_layer_dir;
        use vfs_redirect::{FILE_OPEN_IF, FILE_OVERWRITE_IF};
        // GENERIC_WRITE — `classify_open` reads this as a write intent.
        const WRITE: u32 = 0x4000_0000;

        let base = std::env::temp_dir().join(format!("vfs-engine-2root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root0 = base.join("root0");
        // A second root a multi-root session declares. Deliberately not under
        // `root0`: it is a separate location, the way `Documents\My Games\…`
        // is (`skyrim-live` declares exactly that as root 1).
        let root1 = base.join("root1");
        let overlay_dir = base.join("overlay");
        std::fs::create_dir_all(root0.join("Data")).unwrap();
        std::fs::create_dir_all(root1.join("Data")).unwrap();
        std::fs::create_dir_all(&overlay_dir).unwrap();

        let engine = Engine::with_roots_and_overlay(
            &[
                (RootId::DEFAULT, root0.to_string_lossy().into_owned()),
                (RootId(1), root1.to_string_lossy().into_owned()),
            ],
            overlay_dir.to_str().unwrap(),
            snapshot_bytes(),
        )
        .unwrap();

        // The overlay file each write must land in — from `overlay_layer_dir`,
        // the one place the on-disk naming scheme is defined, not spelled out
        // here a second time.
        let expect_target = |root: RootId| {
            to_nt(
                &overlay_layer_dir(&overlay_dir, root)
                    .join("data")
                    .join("w.ini")
                    .to_string_lossy(),
            )
        };

        let under_root0 = format!(r"\??\{}", root0.join("Data").join("w.ini").to_string_lossy());
        assert_eq!(
            engine.decide_open(&under_root0, WRITE, FILE_OVERWRITE_IF),
            Decision::Redirect { target_nt: expect_target(RootId::DEFAULT) },
            "a write under root 0 must land in root 0's overlay subtree"
        );

        let under_root1 = format!(r"\??\{}", root1.join("Data").join("w.ini").to_string_lossy());
        assert_eq!(
            engine.decide_open(&under_root1, WRITE, FILE_OVERWRITE_IF),
            Decision::Redirect { target_nt: expect_target(RootId(1)) },
            "a write under root 1 must land in ROOT 1's overlay subtree — a \
             PassThrough here is the closed gap reopening (the write reaching \
             real disk); a root-0 target here is the same write colliding with \
             root 0's copy of the same relative path"
        );
        assert_eq!(
            engine.decide_open(&under_root1, WRITE, FILE_OPEN_IF),
            Decision::Redirect { target_nt: expect_target(RootId(1)) },
            "same for the copy-on-write disposition"
        );
        assert_ne!(
            expect_target(RootId::DEFAULT),
            expect_target(RootId(1)),
            "setup: the two roots' overlay targets must be distinct paths for \
             the assertions above to mean anything"
        );

        assert!(engine.is_under_root(&under_root0));
        assert!(
            engine.is_under_root(&under_root1),
            "a declared second root is under root as far as this engine is concerned"
        );

        // Control: the same second-root write against an engine that was never
        // told about root 1 still passes through — being outside every
        // declared root is not the bug, never declaring the root is.
        let single = Engine::with_overlay(
            &root0.to_string_lossy(),
            overlay_dir.to_str().unwrap(),
            snapshot_bytes(),
        )
        .unwrap();
        assert_eq!(
            single.decide_open(&under_root1, WRITE, FILE_OVERWRITE_IF),
            Decision::PassThrough,
            "an undeclared root is genuinely outside; declaring it is what \
             makes the difference above"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Root 1's reads must not be answered out of root 0's snapshot. The
    /// snapshot `Engine` carries has no root dimension at all — it is one flat
    /// vpath tree, published for root 0 — so feeding root 1's remainder into
    /// it would answer `root1\Data\foo.esp` with root 0's backing file. That
    /// would be a *worse* failure than the pass-through this task removed:
    /// silently serving one root's bytes for another root's path.
    ///
    /// So the snapshot half of `decide` is root-0 only, and a root-≥1 read
    /// nothing local can vouch for is sealed (`Deny`) exactly the way gate 3
    /// seals an under-root path the provider graph does not know.
    #[test]
    fn a_read_under_a_second_root_is_not_answered_from_root_zeros_snapshot() {
        let base = std::env::temp_dir()
            .join(format!("vfs-engine-2root-snap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root0 = base.join("root0");
        let root1 = base.join("root1");
        std::fs::create_dir_all(root0.join("Data")).unwrap();
        std::fs::create_dir_all(root1.join("Data")).unwrap();

        let engine = Engine::with_roots(
            &[
                (RootId::DEFAULT, root0.to_string_lossy().into_owned()),
                (RootId(1), root1.to_string_lossy().into_owned()),
            ],
            snapshot_bytes(),
        )
        .unwrap();

        // The snapshot has exactly one entry, `data/foo.esp` -> D:\Mods\Cool.
        let via_root0 = format!(r"\??\{}", root0.join("Data").join("foo.esp").to_string_lossy());
        assert_eq!(
            engine.decide(&via_root0),
            Decision::Redirect { target_nt: r"\??\D:\Mods\Cool\foo.esp".to_string() },
            "root 0 still resolves against the snapshot"
        );

        // The *same relative path* under root 1 must not pick that up.
        let via_root1 = format!(r"\??\{}", root1.join("Data").join("foo.esp").to_string_lossy());
        assert_eq!(
            engine.decide(&via_root1),
            Decision::Deny,
            "root 1's Data\\foo.esp must not resolve to root 0's backing file"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// **The invariant, stated on the write path.** A real file physically on
    /// disk under a managed root, that no provider serves, must not reach the
    /// game through copy-up either.
    ///
    /// This test asserted the exact opposite until gate 4 task 4: that the
    /// overlay copy *was* seeded from root 1's own real file. That was the
    /// best available answer while `cow_seed` had no way to ask the director
    /// anything — it at least ruled out the worse failure of seeding root 1's
    /// copy from root 0's snapshot entry for the same relative path — but it
    /// pinned a hole open. `vfs-directord`'s escape matrix named the same one
    /// from the other side: its negative canary is unreachable by a read, and
    /// its read test's "scoped to reads only" note existed because a *write*
    /// open still reached the canary here. That scope is now covered by the
    /// matrix's own write half, not left open.
    ///
    /// Two roots rather than one, because the cross-root claim is still worth
    /// keeping: neither root's copy-up may seed from the other's snapshot
    /// entry for the same relative path. With disk seeding gone, both roots
    /// answer the same way — nothing is seeded at all without a director —
    /// so the assertion is now that `dest` does not exist rather than that it
    /// holds one root's bytes rather than the other's.
    #[test]
    fn copy_on_write_never_seeds_from_a_real_file_under_the_root() {
        use crate::overlay_layer_dir;
        use vfs_redirect::FILE_OPEN_IF;
        const WRITE: u32 = 0x4000_0000;

        let base = std::env::temp_dir().join(format!("vfs-engine-2root-cow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root0 = base.join("root0");
        let root1 = base.join("root1");
        let overlay_dir = base.join("overlay");
        std::fs::create_dir_all(root0.join("Data")).unwrap();
        std::fs::create_dir_all(root1.join("Data")).unwrap();
        std::fs::create_dir_all(&overlay_dir).unwrap();
        // A real file under root 1 at the *same* relative path the snapshot
        // publishes for root 0 — and one under root 0 too, so neither root's
        // answer can be the accident of there being nothing on disk to copy.
        const R1: &[u8] = b"ROOT-1 REAL BYTES";
        const R0: &[u8] = b"ROOT-0 REAL BYTES";
        std::fs::write(root1.join("Data").join("foo.esp"), R1).unwrap();
        std::fs::write(root0.join("Data").join("foo.esp"), R0).unwrap();

        let engine = Engine::with_roots_and_overlay(
            &[
                (RootId::DEFAULT, root0.to_string_lossy().into_owned()),
                (RootId(1), root1.to_string_lossy().into_owned()),
            ],
            overlay_dir.to_str().unwrap(),
            snapshot_bytes(),
        )
        .unwrap();

        for (root, dir, disk) in
            [(RootId::DEFAULT, &root0, R0), (RootId(1), &root1, R1)]
        {
            let nt = format!(r"\??\{}", dir.join("Data").join("foo.esp").to_string_lossy());
            let dest = overlay_layer_dir(&overlay_dir, root).join("data").join("foo.esp");
            // The write is still captured by the overlay — that half is
            // unchanged, and it is what keeps the write off real disk.
            assert_eq!(
                engine.decide_open(&nt, WRITE, FILE_OPEN_IF),
                Decision::Redirect { target_nt: to_nt(&dest.to_string_lossy()) },
                "root {root:?}: the write itself must still be redirected into the overlay"
            );
            // ...but nothing was copied up. The real file under the root is
            // not a source the VFS may read from, on this path or any other.
            assert!(
                !dest.exists(),
                "root {root:?}: copy-up seeded {dest:?} from somewhere with no director \
                 connected — the only thing it could have read is the real file under \
                 the managed root, which is exactly what must be unreachable"
            );
            // Stated as content too, so a future `dest` that exists for some
            // other reason still cannot quietly hold the disk bytes.
            assert_ne!(
                std::fs::read(&dest).unwrap_or_default(),
                disk,
                "root {root:?}: the overlay copy holds the real on-disk file's bytes"
            );
            // And the snapshot is not a source either: root 0's published
            // entry for this very vpath must not appear under root 1.
            assert_ne!(
                std::fs::read(&dest).unwrap_or_default(),
                R0,
                "root {root:?}: cross-root seed from root 0's snapshot entry"
            );
        }

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A delete under root 1 whites out root 1's copy and leaves root 0's
    /// alone. `whiteout` is the one write-path entry point with no return
    /// value to inspect, so the proof is what the *other* root reads back
    /// afterwards.
    #[test]
    fn a_delete_under_a_second_root_does_not_hide_root_zeros_file() {
        let base = std::env::temp_dir().join(format!("vfs-engine-2root-wh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root0 = base.join("root0");
        let root1 = base.join("root1");
        let overlay_dir = base.join("overlay");
        std::fs::create_dir_all(root0.join("Data")).unwrap();
        std::fs::create_dir_all(root1.join("Data")).unwrap();
        std::fs::create_dir_all(&overlay_dir).unwrap();

        let engine = Engine::with_roots_and_overlay(
            &[
                (RootId::DEFAULT, root0.to_string_lossy().into_owned()),
                (RootId(1), root1.to_string_lossy().into_owned()),
            ],
            overlay_dir.to_str().unwrap(),
            snapshot_bytes(),
        )
        .unwrap();

        let via_root1 = format!(r"\??\{}", root1.join("Data").join("foo.esp").to_string_lossy());
        assert!(engine.whiteout(&via_root1), "a delete under root 1 must be handled");
        assert!(matches!(
            engine.overlay_state(&via_root1),
            Some(OverlayState::Whiteout)
        ));

        // Root 0's identical relative path is untouched: still the snapshot's.
        let via_root0 = format!(r"\??\{}", root0.join("Data").join("foo.esp").to_string_lossy());
        assert!(
            matches!(engine.overlay_state(&via_root0), Some(OverlayState::Absent)),
            "root 1's whiteout leaked into root 0"
        );
        assert_eq!(
            engine.decide(&via_root0),
            Decision::Redirect { target_nt: r"\??\D:\Mods\Cool\foo.esp".to_string() }
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn overlay_absent_falls_through_to_snapshot() {
        let dir = std::env::temp_dir().join(format!("vfs-ovl-abs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let engine = overlay_engine(&dir);
        // No overlay entry -> snapshot redirect to the backing file.
        assert_eq!(
            engine.decide(r"\??\C:\Games\Skyrim\Data\foo.esp"),
            Decision::Redirect { target_nt: r"\??\D:\Mods\Cool\foo.esp".to_string() }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

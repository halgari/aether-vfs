//! A lock-free breadcrumb of the hook currently executing, in shared memory.
//!
//! This exists to diagnose one specific failure that nothing else here can see:
//! an injected process that hangs inside a hook, with **zero CPU, one thread,
//! and immune to `TerminateProcess`**. Observed repeatedly on 2026-09-02 in
//! `vfs-fixture-escape`. Such a process cannot be inspected the usual ways —
//! there is no debugger installed, its own stats reporter is a *second thread*
//! whose mere existence perturbs the race enough to hide it (32 clean runs with
//! `VFS_SHIM_STATS_LOG` set versus roughly one wedge in twelve without), and a
//! breadcrumb kept in the process's own heap would be unreadable precisely when
//! it mattered, because the process is stuck and cannot be attached to.
//!
//! So the breadcrumb lives in a **file-backed shared mapping**, which an
//! ordinary outside process reads while the target is wedged. That is the whole
//! trick, and the reason this is a separate module rather than another counter
//! in `hookstats`.
//!
//! # Why it should not move the race
//!
//! Per hook entry this performs **two relaxed atomic stores** and nothing else.
//! No clock read — `Instant::now()` is a syscall-ish `QueryPerformanceCounter`
//! and is what `hookstats` pays. No lock. No allocation. No I/O. No thread.
//! Compare `hookstats::Timed`, which reads the clock on entry *and* exit and
//! whose reporter thread is the thing already shown to suppress the bug.
//!
//! It is off unless `VFS_SHIM_BREADCRUMB` names a path, and the check is a
//! cached `bool`, so an ordinary run pays what it paid before.
//!
//! # Reading it
//!
//! Little-endian, at the start of the mapping:
//!
//! | offset | size | meaning |
//! |---|---|---|
//! | 0  | 4 | magic `0x4252_4342` (`"BCRB"`), so a truncated or foreign file is not misread |
//! | 4  | 4 | hook id currently entered, or [`NONE`] |
//! | 8  | 8 | entries so far — **the liveness signal** |
//! | 16 | 8 | exits so far |
//! | 24 | 4 | last hook to complete, or [`NONE`] |
//! | 32 | 4 | monotonic trail index |
//! | 40 | 32 | trail: 8 hook ids, most recent at `(idx-1) % 8` |
//! | 76 | 4 | sub-phase marker, for narrowing within one hook |
//!
//! The diagnosis is a comparison, not a single read: sample twice. If `entries`
//! has not moved and `current != NONE`, the process is stuck inside that hook.
//! If `entries` has not moved and `current == NONE`, it is stuck somewhere that
//! is *not* a hook, which would be just as informative and would redirect the
//! search entirely.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;

use vfs_win::SharedMapping;

/// No hook is currently entered. Not `0`, because `0` is a real hook id
/// (`Hook::Create`) and a zeroed page would otherwise read as "inside
/// NtCreateFile" — the exact false positive this diagnostic cannot afford.
pub const NONE: u32 = u32::MAX;

const MAGIC: u32 = 0x4252_4342;
const OFF_MAGIC: usize = 0;
const OFF_CURRENT: usize = 4;
const OFF_ENTRIES: usize = 8;
const OFF_EXITS: usize = 16;
const OFF_LAST_DONE: usize = 24;
/// Monotonic index into the trail; `% TRAIL` gives the next slot to write.
const OFF_TRAIL_IDX: usize = 32;
/// Start of the trail: [`TRAIL`] hook ids, most recent at `(idx-1) % TRAIL`.
const OFF_TRAIL: usize = 40;
/// Free-form sub-phase marker, for narrowing *within* one hook.
///
/// The trail names the hook a process died in; it cannot say which branch of
/// that hook. `NtClose` has three (synthetic fuse handle, synthetic zip
/// section, ordinary handle) plus a ring round trip, and picking between them
/// by reading the code is how several plausible-but-wrong theories got built.
const OFF_MARK: usize = 76;
/// Entries in the trail. Small on purpose — this answers "what was nested
/// inside what" at the moment of a hang, not "what has this process ever done".
const TRAIL: usize = 8;
/// One page is far more than the 28 bytes used; the surplus leaves room to add
/// fields without changing the file's size, which an external reader keys on.
const MAP_BYTES: usize = 4096;

/// The mapping, or `None` when disabled or when it could not be created.
static SLOT: OnceLock<Option<SharedMapping>> = OnceLock::new();

/// Borrow a 4-byte field as an atomic.
///
/// # Safety
/// `base` must be the start of a live mapping of at least `MAP_BYTES`, and
/// `off` a 4-aligned offset within it. Mapping bases are page-aligned, so every
/// offset here is naturally aligned.
#[allow(unsafe_code)]
unsafe fn u32_at(base: *mut u8, off: usize) -> &'static AtomicU32 {
    // SAFETY: caller guarantees a live, sufficiently large, aligned mapping.
    // The mapping outlives every caller: it is owned by `SLOT`, a `OnceLock`
    // that is never cleared, so `'static` is honest here rather than convenient.
    unsafe { AtomicU32::from_ptr(base.add(off).cast::<u32>()) }
}

/// Borrow an 8-byte field as an atomic. Same contract as [`u32_at`].
///
/// # Safety
/// As [`u32_at`], with `off` 8-aligned.
#[allow(unsafe_code)]
unsafe fn u64_at(base: *mut u8, off: usize) -> &'static AtomicU64 {
    // SAFETY: as above.
    unsafe { AtomicU64::from_ptr(base.add(off).cast::<u64>()) }
}

/// `dir/name.bin` -> `dir/name.<pid>.bin`, so concurrent injected processes do
/// not share one breadcrumb.
fn pid_path(base: &std::ffi::OsStr) -> std::path::PathBuf {
    let p = std::path::Path::new(base);
    let pid = std::process::id();
    let stem = p.file_stem().map_or_else(|| "breadcrumb".into(), |s| s.to_string_lossy());
    let ext = p.extension().map_or_else(|| "bin".into(), |s| s.to_string_lossy());
    let name = format!("{stem}.{pid}.{ext}");
    match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.join(name),
        _ => std::path::PathBuf::from(name),
    }
}

/// Create the mapping if `VFS_SHIM_BREADCRUMB` names a path.
///
/// **Call this before the detours are enabled.** Creating the file is real file
/// I/O, and once hooks are live that I/O re-enters them; doing it during
/// `install` — where `hookstats::start_reporter` is also called — keeps it out
/// of that path entirely.
pub fn init() {
    let _ = SLOT.get_or_init(|| {
        // Per-process file. The variable is inherited by every child, so a
        // single shared path would have them all scribbling over each other —
        // and the interesting case is precisely several injected processes at
        // once, where only one of them wedges.
        let base = vfs_env::raw(vfs_env::SHIM_BREADCRUMB)?;
        let path = pid_path(&base);
        let m = SharedMapping::create_file_backed(&path, MAP_BYTES).ok()?;
        let base = m.as_mut_ptr();
        // SAFETY: `m` is a live mapping of MAP_BYTES; all offsets are aligned.
        #[allow(unsafe_code)]
        unsafe {
            u32_at(base, OFF_CURRENT).store(NONE, Ordering::Relaxed);
            u64_at(base, OFF_ENTRIES).store(0, Ordering::Relaxed);
            u64_at(base, OFF_EXITS).store(0, Ordering::Relaxed);
            u32_at(base, OFF_LAST_DONE).store(NONE, Ordering::Relaxed);
            u32_at(base, OFF_TRAIL_IDX).store(0, Ordering::Relaxed);
            for i in 0..TRAIL {
                u32_at(base, OFF_TRAIL + i * 4).store(NONE, Ordering::Relaxed);
            }
            // Magic last, with a release: a reader that sees it knows the rest
            // is initialised rather than a zeroed page.
            u32_at(base, OFF_MAGIC).store(MAGIC, Ordering::Release);
        }
        Some(m)
    });
}

/// Record entry into `hook`. Two relaxed stores when enabled, nothing otherwise.
#[inline]
pub fn enter(hook: u32) {
    if let Some(Some(m)) = SLOT.get() {
        let base = m.as_mut_ptr();
        // SAFETY: `m` is a live MAP_BYTES mapping owned by SLOT.
        #[allow(unsafe_code)]
        unsafe {
            u64_at(base, OFF_ENTRIES).fetch_add(1, Ordering::Relaxed);
            u32_at(base, OFF_CURRENT).store(hook, Ordering::Relaxed);
            // Trail of the last few entries, so a hang shows the *nesting*.
            // `entries - exits` already reveals how many frames are
            // outstanding; this names them, which is what turns "stuck in
            // NtClose" into "NtClose nested inside <outer hook>".
            let i = u32_at(base, OFF_TRAIL_IDX).fetch_add(1, Ordering::Relaxed) as usize;
            u32_at(base, OFF_TRAIL + (i % TRAIL) * 4).store(hook, Ordering::Relaxed);
        }
    }
}

/// Record completion of `hook`.
///
/// `current` goes back to [`NONE`] rather than staying on the finished hook, so
/// a sample showing a hook id genuinely means "inside it now". Nested hooks are
/// not tracked: the shim's reentrancy guard takes re-entered calls straight to
/// real ntdll, so the depth that matters here is one.
#[inline]
pub fn exit(hook: u32) {
    if let Some(Some(m)) = SLOT.get() {
        let base = m.as_mut_ptr();
        // SAFETY: as `enter`.
        #[allow(unsafe_code)]
        unsafe {
            u64_at(base, OFF_EXITS).fetch_add(1, Ordering::Relaxed);
            u32_at(base, OFF_LAST_DONE).store(hook, Ordering::Relaxed);
            u32_at(base, OFF_CURRENT).store(NONE, Ordering::Relaxed);
        }
    }
}

/// Sub-phase markers for `NtClose`, whose three branches are otherwise
/// indistinguishable in a post-mortem. Named rather than bare numbers because
/// they are read by an external tool and by whoever debugs this next.
pub mod mark_close {
    /// Entered the hook, before any branch is chosen.
    pub const ENTER: u32 = 1000;
    /// Synthetic fuse handle: inside `close_fuse`.
    pub const FUSE_TABLE: u32 = 1001;
    /// Synthetic fuse handle: about to look up the ring client.
    pub const FUSE_CLIENT: u32 = 1002;
    /// Synthetic fuse handle: **inside the ring round trip** `c.close(fh)`.
    pub const FUSE_RING: u32 = 1003;
    /// Synthetic fuse handle: ring round trip returned.
    pub const FUSE_DONE: u32 = 1004;
    /// Synthetic fuse branch complete.
    pub const FUSE_EXIT: u32 = 1005;
    /// Synthetic zip section: inside `close_section`.
    pub const ZIP_TABLE: u32 = 1010;
    /// Synthetic zip section: inside `on_section_closed`.
    pub const ZIP_REGION: u32 = 1011;
    /// Synthetic zip section: region release returned.
    pub const ZIP_DONE: u32 = 1012;
    /// Synthetic zip branch complete.
    pub const ZIP_EXIT: u32 = 1013;
    /// Ordinary handle: **inside the four table locks**. Observed 2026-09-02 as
    /// the value a wedged fixture is stuck on, which is what identified this as
    /// a re-entrant acquisition of a shim table lock rather than anything in
    /// the synthetic-handle paths.
    pub const TABLES: u32 = 1020;
    /// Ordinary handle: about to call real ntdll `NtClose`.
    pub const TRAMP: u32 = 1021;
    /// Ordinary handle: real `NtClose` returned.
    pub const TRAMP_DONE: u32 = 1022;
}

/// Record a sub-phase marker. One relaxed store; see [`OFF_MARK`].
#[inline]
pub fn mark(m: u32) {
    if let Some(Some(mm)) = SLOT.get() {
        let base = mm.as_mut_ptr();
        // SAFETY: `mm` is a live MAP_BYTES mapping owned by SLOT.
        #[allow(unsafe_code)]
        unsafe {
            u32_at(base, OFF_MARK).store(m, Ordering::Relaxed);
        }
    }
}

/// Whether a breadcrumb mapping is live, for tests and diagnostics.
pub fn is_active() -> bool {
    matches!(SLOT.get(), Some(Some(_)))
}

/// Read the breadcrumb back in-process as `(current, entries, exits, last_done)`.
///
/// Tests use this; an outside observer reads the file instead, which is the
/// point of the design.
pub fn snapshot() -> Option<(u32, u64, u64, u32)> {
    let Some(Some(m)) = SLOT.get() else {
        return None;
    };
    let base = m.as_mut_ptr();
    // SAFETY: `m` is a live MAP_BYTES mapping owned by SLOT.
    #[allow(unsafe_code)]
    unsafe {
        if u32_at(base, OFF_MAGIC).load(Ordering::Acquire) != MAGIC {
            return None;
        }
        Some((
            u32_at(base, OFF_CURRENT).load(Ordering::Relaxed),
            u64_at(base, OFF_ENTRIES).load(Ordering::Relaxed),
            u64_at(base, OFF_EXITS).load(Ordering::Relaxed),
            u32_at(base, OFF_LAST_DONE).load(Ordering::Relaxed),
        ))
    }
}

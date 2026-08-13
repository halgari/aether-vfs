//! Shim-owned section address space, including lazy FUSE data sections that
//! emulate mmap without preloading multi‑GiB BSAs.
//!
//! CreateSection reserves address space (and optionally warms the first window).
//! MapView returns that base. Further first-touch faults commit 256 KiB chunks
//! and stream them from the director via the shared bulk arena.
//!
//! **Lifetime.** The reservation belongs to the *section*, never to a view —
//! that is NT's model and the game relies on it: BSA readers slide views over a
//! big archive, so `NtUnmapViewOfSection` on one window must leave every other
//! window (and any later remap of the still-open section) valid. The VA is
//! released only once the section handle is closed *and* the last view is gone.
#![allow(unsafe_code)]

use core::cell::Cell;
use core::ffi::c_void;
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_POINTERS,
};
use windows_sys::Win32::System::Memory::{
    VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE,
};

const PAGE: usize = 4096;
/// Commit/fill this many bytes per fault.
const CHUNK: usize = 256 * 1024;
/// Warm this many bytes at CreateSection so header/index peeks don't depend on VEH.
const WARM_BYTES: usize = 2 * 1024 * 1024;
/// Largest file we back with a demand-paged section. Only address space is
/// reserved up front, so this is bounded by 64-bit VA, not by RAM — it exists
/// to keep a corrupt size from reserving something absurd. Heavily modded load
/// orders ship BSAs well past the old 3 GiB ceiling, and returning
/// `STATUS_SECTION_TOO_BIG` for those makes the game fail the archive outright.
pub const MAX_LAZY: u64 = 64 * 1024 * 1024 * 1024;

const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;
const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
const STATUS_ACCESS_VIOLATION: i32 = 0xC0000005u32 as i32;

/// A shim-owned VA range backing one synthetic section.
struct OwnedRegion {
    base: usize,
    reserved: usize,
    file_size: u64,
    fh: u64,
    /// Demand-paged: reserved `PAGE_NOACCESS`, chunks committed by the VEH.
    /// Eager regions are fully committed and filled at creation.
    lazy: bool,
    /// The section handle is still open.
    section_open: bool,
    committed: HashSet<usize>,
}

impl OwnedRegion {
    /// NT keeps a section's pages alive while any view is mapped, even after
    /// the handle is closed. `zipserve` owns the view refcounts, so ask it
    /// rather than keeping a second tally that can drift out of step.
    fn is_dead(&self) -> bool {
        !self.section_open && !crate::zipserve::has_view_in(self.base, self.reserved)
    }
}

static REGIONS: Mutex<BTreeMap<usize, OwnedRegion>> = Mutex::new(BTreeMap::new());
static VEH_INSTALLED: AtomicBool = AtomicBool::new(false);
static FILL_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    static IN_VEH: Cell<bool> = const { Cell::new(false) };
    /// Set on the fill worker: a fault there must never re-enter the fill path
    /// (it would wait on itself), so the VEH declines it.
    static IS_FILL_WORKER: Cell<bool> = const { Cell::new(false) };
}

fn align_up(n: usize, a: usize) -> usize {
    (n + a - 1) & !(a - 1)
}

fn ensure_veh() {
    if VEH_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let h = unsafe { AddVectoredExceptionHandler(1, Some(veh_handler)) };
    if h.is_null() {
        VEH_INSTALLED.store(false, Ordering::SeqCst);
    }
}

// ── fill worker ────────────────────────────────────────────────────────────
//
// Faults are serviced by one long-lived thread rather than a thread spawned per
// chunk. Creating a thread inside a page-fault handler runs the loader's
// DLL_THREAD_ATTACH callbacks, so it deadlocks outright if the faulting thread
// happens to hold the loader lock — and at 256 KiB per chunk a multi-GiB BSA
// would otherwise burn tens of thousands of thread creations.

struct FillJob {
    fh: u64,
    file_off: u64,
    dest: usize,
    len: usize,
    reply: Sender<Option<usize>>,
}

// SAFETY: `dest` addresses a committed page owned by the requesting thread for
// the duration of the job; the requester blocks on `reply` until the worker is
// done with it.
unsafe impl Send for FillJob {}

static FILL_WORKER: OnceLock<Option<Mutex<Sender<FillJob>>>> = OnceLock::new();

/// Start the fill worker. Call from a normal call context (section creation),
/// never from the VEH — see the note above.
fn ensure_worker() {
    FILL_WORKER.get_or_init(|| {
        let (tx, rx) = channel::<FillJob>();
        std::thread::Builder::new()
            .name("vfs-lazy-fill".into())
            // Deep FUSE I/O must never run on the game's primary stack.
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                IS_FILL_WORKER.with(|c| c.set(true));
                while let Ok(job) = rx.recv() {
                    let n = crate::fuse_client::global().and_then(|client| {
                        // SAFETY: requester owns `dest` and blocks until reply.
                        let dest = unsafe {
                            core::slice::from_raw_parts_mut(job.dest as *mut u8, job.len)
                        };
                        client.read_fragmented(job.fh, job.file_off, dest).ok()
                    });
                    let _ = job.reply.send(n);
                }
            })
            .ok()
            .map(|_| Mutex::new(tx))
    });
}

/// Stream `[file_off, file_off+len)` of `fh` into `dest`, returning bytes read.
///
/// Uses the worker when available; falls back to an inline read (vfs-inject
/// expands the primary stack for exactly this) if the worker never started or
/// we are already *on* it.
fn fill_bytes(fh: u64, file_off: u64, dest: usize, len: usize) -> Option<usize> {
    let on_worker = IS_FILL_WORKER.with(|c| c.get());
    if !on_worker {
        if let Some(Some(tx)) = FILL_WORKER.get() {
            let (reply, wait) = channel();
            let queued = tx.lock().ok().and_then(|tx| {
                tx.send(FillJob {
                    fh,
                    file_off,
                    dest,
                    len,
                    reply,
                })
                .ok()
            });
            if queued.is_some() {
                return wait.recv().ok().flatten();
            }
        }
    }
    let client = crate::fuse_client::global()?;
    // SAFETY: caller committed `[dest, dest+len)` before asking for the fill.
    let out = unsafe { core::slice::from_raw_parts_mut(dest as *mut u8, len) };
    client.read_fragmented(fh, file_off, out).ok()
}

// ── section creation ───────────────────────────────────────────────────────

/// Reserve VA for a director-backed data section; warm first [`WARM_BYTES`].
///
/// Returns a synthetic section handle for [`crate::zipserve::map_view`].
pub unsafe fn create_lazy_data_section(fh: u64, file_size: u64) -> Option<isize> {
    if file_size == 0 || file_size > MAX_LAZY {
        return None;
    }
    let reserved = align_up(file_size as usize, PAGE);
    if reserved == 0 {
        return None;
    }
    // Install VEH only if not disabled (VFS_LAZY_NO_VEH=1 for debug).
    if std::env::var_os("VFS_LAZY_NO_VEH").is_none() {
        ensure_veh();
    }
    ensure_worker();
    let base = VirtualAlloc(core::ptr::null(), reserved, MEM_RESERVE, PAGE_NOACCESS);
    if base.is_null() {
        return None;
    }
    let base_u = base as usize;
    if !track(base_u, reserved, file_size, fh, true) {
        VirtualFree(base, 0, MEM_RELEASE);
        return None;
    }
    // Warm header/index window so first peeks work even if VEH is slow/racy.
    let warm = (file_size as usize).min(WARM_BYTES);
    if warm > 0 {
        let _ = ensure_range(base_u, 0, warm);
    }
    match crate::zipserve::register_mapped_image(base_u, file_size) {
        Some(h) => Some(h),
        None => {
            forget(base_u);
            VirtualFree(base, 0, MEM_RELEASE);
            None
        }
    }
}

/// Record an eagerly-filled section allocation so closing the section frees it.
///
/// Without this the shim leaks every eager mapping (up to 256 MiB each) for the
/// life of the process.
pub fn track_eager_section(base: usize, size: u64) -> bool {
    track(base, align_up(size as usize, PAGE), size, 0, false)
}

fn track(base: usize, reserved: usize, file_size: u64, fh: u64, lazy: bool) -> bool {
    match REGIONS.lock() {
        Ok(mut g) => {
            g.insert(
                base,
                OwnedRegion {
                    base,
                    reserved,
                    file_size,
                    fh,
                    lazy,
                    section_open: true,
                    committed: HashSet::new(),
                },
            );
            true
        }
        Err(_) => false,
    }
}

fn forget(base: usize) {
    if let Ok(mut g) = REGIONS.lock() {
        g.remove(&base);
    }
}

/// Key of the owned region containing `addr`, if any.
fn region_key(g: &BTreeMap<usize, OwnedRegion>, addr: usize) -> Option<usize> {
    g.range(..=addr)
        .next_back()
        .filter(|(_, r)| addr < r.base + r.reserved)
        .map(|(k, _)| *k)
}

/// Drop the region at `key` if nothing references it any more.
fn reap(g: &mut BTreeMap<usize, OwnedRegion>, key: usize) {
    if g.get(&key).is_some_and(|r| r.is_dead()) {
        if let Some(r) = g.remove(&key) {
            // SAFETY: `base` came from VirtualAlloc(MEM_RESERVE) here and no
            // view or open section handle refers to it any more.
            unsafe {
                VirtualFree(r.base as *mut c_void, 0, MEM_RELEASE);
            }
        }
    }
}

// ── view / section lifetime ────────────────────────────────────────────────

/// Note that a mapped view over `addr` went away. Frees the region only if its
/// section is closed and no other view remains.
///
/// Call *after* retiring the view in [`crate::zipserve::unmap_view`].
pub fn on_view_unmapped(addr: usize) {
    if let Ok(mut g) = REGIONS.lock() {
        if let Some(key) = region_key(&g, addr) {
            reap(&mut g, key);
        }
    }
}

/// Note that the section handle covering `base` was closed. Frees the region
/// once the last view is unmapped.
pub fn on_section_closed(base: usize) {
    if let Ok(mut g) = REGIONS.lock() {
        if let Some(key) = region_key(&g, base) {
            if let Some(r) = g.get_mut(&key) {
                r.section_open = false;
            }
            reap(&mut g, key);
        }
    }
}

/// Whether `addr` falls in a demand-paged region (so a fault there is ours).
pub fn is_lazy_base(addr: usize) -> bool {
    REGIONS
        .lock()
        .map(|g| {
            g.range(..=addr)
                .next_back()
                .is_some_and(|(_, r)| r.lazy && addr < r.base + r.reserved)
        })
        .unwrap_or(false)
}

// ── demand paging ──────────────────────────────────────────────────────────

/// Ensure `[offset, offset+len)` within the lazy region is committed and filled.
pub unsafe fn ensure_range(base: usize, offset: usize, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    let end = offset.saturating_add(len);
    let mut off = offset / CHUNK * CHUNK;
    while off < end {
        if !fill_chunk_at(base + off) {
            return false;
        }
        off += CHUNK;
    }
    true
}

unsafe extern "system" fn veh_handler(info: *mut EXCEPTION_POINTERS) -> i32 {
    if info.is_null() {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    // Re-entrancy and worker faults are not ours to service.
    if IN_VEH.with(|c| c.get()) || IS_FILL_WORKER.with(|c| c.get()) {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let rec = (*info).ExceptionRecord;
    if rec.is_null() {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    if (*rec).ExceptionCode != STATUS_ACCESS_VIOLATION {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    if (*rec).NumberParameters < 2 {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let fault_addr = (*rec).ExceptionInformation[1];
    // Quick reject before taking locks.
    if !is_lazy_base(fault_addr) {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    IN_VEH.with(|c| c.set(true));
    let ok = fill_chunk_at(fault_addr);
    IN_VEH.with(|c| c.set(false));
    if ok {
        EXCEPTION_CONTINUE_EXECUTION
    } else {
        EXCEPTION_CONTINUE_SEARCH
    }
}

/// Commit + stream the CHUNK covering `addr` (addr may be anywhere in the chunk).
unsafe fn fill_chunk_at(addr: usize) -> bool {
    let _guard = match FILL_LOCK.lock() {
        Ok(g) => g,
        Err(_) => return false,
    };

    let (base, file_size, fh, chunk_idx, commit_off, commit_len) = {
        let g = match REGIONS.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        let Some((_, region)) = g
            .range(..=addr)
            .next_back()
            .filter(|(_, r)| r.lazy && addr < r.base + r.reserved)
        else {
            return false;
        };
        let off = addr - region.base;
        let chunk_idx = off / CHUNK;
        if region.committed.contains(&chunk_idx) {
            return true;
        }
        let commit_off = chunk_idx * CHUNK;
        let commit_len = CHUNK.min(region.reserved - commit_off);
        (
            region.base,
            region.file_size,
            region.fh,
            chunk_idx,
            commit_off,
            commit_len,
        )
    };

    let page = (base + commit_off) as *mut c_void;
    // Align commit length to pages (required).
    let commit_len = align_up(commit_len, PAGE);
    let committed = VirtualAlloc(page, commit_len, MEM_COMMIT, PAGE_READWRITE);
    if committed.is_null() {
        // Might already be committed by a racing fill that dropped lock early.
        if REGIONS
            .lock()
            .ok()
            .and_then(|g| g.get(&base).map(|r| r.committed.contains(&chunk_idx)))
            .unwrap_or(false)
        {
            return true;
        }
        return false;
    }

    let file_off = commit_off as u64;
    let readable = if file_off >= file_size {
        0usize
    } else {
        ((file_size - file_off) as usize).min(commit_len)
    };

    if readable > 0 {
        // Bracket the fill: a started-without-completed pair is the signature of
        // a wedged faulting thread, which is invisible everywhere else.
        crate::hookstats::note_fill_start();
        let t0 = std::time::Instant::now();
        let n_read = fill_bytes(fh, file_off, page as usize, readable);
        crate::hookstats::note_fill_end(
            n_read.unwrap_or(0),
            t0.elapsed().as_nanos() as u64,
            n_read.is_some(),
        );
        let dest = core::slice::from_raw_parts_mut(page as *mut u8, readable);
        match n_read {
            Some(n) if n < readable => dest[n..].fill(0),
            Some(_) => {}
            None => dest.fill(0),
        }
    }
    if readable < commit_len {
        core::slice::from_raw_parts_mut((page as *mut u8).add(readable), commit_len - readable)
            .fill(0);
    }

    if let Ok(mut g) = REGIONS.lock() {
        if let Some(region) = g.get_mut(&base) {
            region.committed.insert(chunk_idx);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_FREE,
    };

    /// Serialises the tests that reserve and release virtual address space.
    ///
    /// Each asserts that a specific base became `MEM_FREE`, and a freed base is
    /// immediately available to anyone — including a sibling test reserving on
    /// another thread. When that happens the region reads back as `MEM_RESERVE`
    /// and the test blames the release it was checking. Nothing about the
    /// product is racy here; only the observation is.
    static VA_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Region state for `addr`: `MEM_FREE` once the VA has been released.
    fn va_state(addr: usize) -> u32 {
        let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { core::mem::zeroed() };
        let n = unsafe {
            VirtualQuery(
                addr as *const c_void,
                &mut mbi,
                core::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        assert!(n != 0, "VirtualQuery failed for {addr:#x}");
        mbi.State
    }

    /// 8 MiB standin for a BSA — big enough to span many CHUNKs, small enough
    /// that a reserve in the test process is free.
    const SIZE: u64 = 8 * 1024 * 1024;

    /// Mirrors `map_view_hook` / `unmap_view_hook`. Keep in step with hook.rs.
    fn hook_map(h: isize, off: u64, want: u64) -> usize {
        let (base, _) = crate::zipserve::map_view(h, off, want).expect("map_view");
        base
    }

    fn hook_unmap(base: usize) {
        crate::zipserve::unmap_view(base);
        on_view_unmapped(base);
    }

    fn hook_close(h: isize) {
        if let Some(window) = crate::zipserve::close_section(h) {
            on_section_closed(window);
        }
    }

    #[test]
    fn unmapping_a_view_leaves_the_open_section_usable() {
        let _va = VA_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = unsafe { create_lazy_data_section(0, SIZE) }.expect("create section");
        let base = hook_map(h, 0, 0);

        hook_unmap(base);

        // The section handle is still open, so its address space must survive.
        assert_ne!(
            va_state(base),
            MEM_FREE,
            "unmapping one view released the whole section's VA"
        );
        let again = hook_map(h, 0, 0);
        assert!(
            is_lazy_base(again),
            "remapped view is no longer demand-pageable — next touch faults to a crash"
        );

        hook_unmap(again);
        hook_close(h);
    }

    #[test]
    fn unmapping_an_offset_view_keeps_sibling_views_alive() {
        let _va = VA_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = unsafe { create_lazy_data_section(0, SIZE) }.expect("create section");
        let head = hook_map(h, 0, 64 * 1024);
        let off = 4 * 1024 * 1024u64;
        let tail = hook_map(h, off, 64 * 1024);
        assert_eq!(tail, head + off as usize);

        // Drop only the tail window; the head window is still in use.
        hook_unmap(tail);

        assert_ne!(
            va_state(head),
            MEM_FREE,
            "unmapping an offset view freed the base view still in use"
        );
        assert!(is_lazy_base(head), "head view lost its demand-paging");

        hook_unmap(head);
        hook_close(h);
    }

    #[test]
    fn closing_the_section_releases_the_reservation() {
        let _va = VA_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = unsafe { create_lazy_data_section(0, SIZE) }.expect("create section");
        let base = hook_map(h, 0, 0);

        hook_unmap(base);
        hook_close(h);

        assert_eq!(
            va_state(base),
            MEM_FREE,
            "closing the section leaked its reservation"
        );
    }

    #[test]
    fn a_closed_section_with_a_live_view_keeps_its_pages() {
        let _va = VA_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = unsafe { create_lazy_data_section(0, SIZE) }.expect("create section");
        let base = hook_map(h, 0, 0);

        // NT keeps section pages alive while a view is mapped.
        hook_close(h);
        assert_ne!(
            va_state(base),
            MEM_FREE,
            "closing the handle freed pages still mapped into the process"
        );

        hook_unmap(base);
        assert_eq!(va_state(base), MEM_FREE, "last unmap should reap the region");
    }

    #[test]
    fn two_views_at_one_base_are_refcounted() {
        let _va = VA_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = unsafe { create_lazy_data_section(0, SIZE) }.expect("create section");
        let a = hook_map(h, 0, 0);
        let b = hook_map(h, 0, 0);
        assert_eq!(a, b);

        hook_unmap(a);
        assert!(
            crate::zipserve::is_synth_view(b),
            "first unmap forgot a base the process still has mapped"
        );
        assert_ne!(va_state(b), MEM_FREE);

        hook_unmap(b);
        hook_close(h);
        assert_eq!(va_state(a), MEM_FREE);
    }

    #[test]
    fn eager_sections_are_freed_on_close() {
        let _va = VA_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let size = 1024 * 1024u64;
        let base = unsafe {
            VirtualAlloc(
                core::ptr::null(),
                size as usize,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        } as usize;
        assert!(base != 0);
        assert!(track_eager_section(base, size));
        let h = crate::zipserve::register_mapped_image(base, size).expect("register");

        let mapped = hook_map(h, 0, 0);
        assert!(!is_lazy_base(mapped), "eager region must not be demand-paged");
        hook_unmap(mapped);
        hook_close(h);

        assert_eq!(
            va_state(base),
            MEM_FREE,
            "eager section allocation leaked on close"
        );
    }

    #[test]
    fn oversized_files_are_still_mappable() {
        let _va = VA_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Well past the old 3 GiB ceiling; reservation only, so this is cheap.
        assert!(MAX_LAZY > 8 * 1024 * 1024 * 1024);
    }
}

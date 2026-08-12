//! Lazy FUSE data sections: emulate mmap without preloading multi‑GiB BSAs.
//!
//! CreateSection reserves address space only. MapView returns that base.
//! First touch faults (ACCESS_VIOLATION); a vectored exception handler commits
//! a chunk and streams it from the director via the shared bulk arena.
#![allow(unsafe_code)]

use core::cell::Cell;
use core::ffi::c_void;
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_POINTERS,
};
use windows_sys::Win32::System::Memory::{
    VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS,
    PAGE_READWRITE,
};

/// Page size (x64 Windows).
const PAGE: usize = 4096;
/// Commit/fill this many bytes per fault (reduces fault rate vs page-at-a-time).
const CHUNK: usize = 256 * 1024;
/// Hard cap for reserved region (largest SE BSAs ~1.7 GiB).
const MAX_LAZY: u64 = 3 * 1024 * 1024 * 1024;

const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;
const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
/// STATUS_ACCESS_VIOLATION as NTSTATUS (i32).
const STATUS_ACCESS_VIOLATION: i32 = 0xC0000005u32 as i32;

struct LazyRegion {
    base: usize,
    /// Page-aligned reserved length.
    reserved: usize,
    /// Exact file size (reads past this are zero-filled).
    file_size: u64,
    /// Director FUSE file handle.
    fh: u64,
    /// Chunk indices already committed (chunk = CHUNK bytes).
    committed: HashSet<usize>,
}

static REGIONS: Mutex<BTreeMap<usize, LazyRegion>> = Mutex::new(BTreeMap::new());
static VEH_INSTALLED: AtomicBool = AtomicBool::new(false);
/// Global lock while filling a fault (director read + commit).
static FILL_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    static IN_VEH: Cell<bool> = const { Cell::new(false) };
}

fn align_up(n: usize, a: usize) -> usize {
    (n + a - 1) & !(a - 1)
}

fn ensure_veh() {
    if VEH_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    // First handler so we run before language runtimes.
    let h = unsafe { AddVectoredExceptionHandler(1, Some(veh_handler)) };
    if h.is_null() {
        VEH_INSTALLED.store(false, Ordering::SeqCst);
    }
}

/// Reserve VA for a director-backed data section (no bulk read).
///
/// Returns synthetic section handle suitable for [`crate::zipserve::map_view`].
pub unsafe fn create_lazy_data_section(fh: u64, file_size: u64) -> Option<isize> {
    if file_size == 0 || file_size > MAX_LAZY {
        return None;
    }
    let reserved = align_up(file_size as usize, PAGE);
    if reserved == 0 {
        return None;
    }
    ensure_veh();
    let base = VirtualAlloc(
        core::ptr::null(),
        reserved,
        MEM_RESERVE,
        PAGE_NOACCESS,
    );
    if base.is_null() {
        return None;
    }
    let base_u = base as usize;
    {
        let mut g = REGIONS.lock().ok()?;
        g.insert(
            base_u,
            LazyRegion {
                base: base_u,
                reserved,
                file_size,
                fh,
                committed: HashSet::new(),
            },
        );
    }
    crate::zipserve::register_mapped_image(base_u, file_size)
}

/// Free a lazy region containing `addr` (MapView base may be section_offset into the region).
pub unsafe fn release_if_lazy(addr: usize) -> bool {
    let region = {
        let mut g = match REGIONS.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        let key = g
            .range(..=addr)
            .next_back()
            .filter(|(_, r)| addr < r.base + r.reserved)
            .map(|(k, _)| *k);
        key.and_then(|k| g.remove(&k))
    };
    if let Some(r) = region {
        VirtualFree(r.base as *mut c_void, 0, MEM_RELEASE);
        true
    } else {
        false
    }
}

pub fn is_lazy_base(addr: usize) -> bool {
    REGIONS
        .lock()
        .map(|g| {
            g.range(..=addr)
                .next_back()
                .is_some_and(|(_, r)| addr < r.base + r.reserved)
        })
        .unwrap_or(false)
}

unsafe extern "system" fn veh_handler(info: *mut EXCEPTION_POINTERS) -> i32 {
    if info.is_null() {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    // Avoid re-entering while we are filling (director I/O / alloc).
    if IN_VEH.with(|c| c.get()) {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let rec = (*info).ExceptionRecord;
    if rec.is_null() {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let code = (*rec).ExceptionCode;
    if code != STATUS_ACCESS_VIOLATION {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    if (*rec).NumberParameters < 2 {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let fault_addr = (*rec).ExceptionInformation[1];
    IN_VEH.with(|c| c.set(true));
    let ok = fill_fault(fault_addr);
    IN_VEH.with(|c| c.set(false));
    if ok {
        EXCEPTION_CONTINUE_EXECUTION
    } else {
        EXCEPTION_CONTINUE_SEARCH
    }
}

/// Commit + stream one CHUNK covering `fault_addr` from the director.
unsafe fn fill_fault(fault_addr: usize) -> bool {
    let _guard = match FILL_LOCK.lock() {
        Ok(g) => g,
        Err(_) => return false,
    };

    // Re-check under lock (another thread may have filled).
    let (base, reserved, file_size, fh, chunk_idx, commit_off, commit_len) = {
        let mut g = match REGIONS.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        let Some((&_k, region)) = g
            .range(..=fault_addr)
            .next_back()
            .filter(|(_, r)| fault_addr < r.base + r.reserved)
        else {
            return false;
        };
        let off = fault_addr - region.base;
        let chunk_idx = off / CHUNK;
        if region.committed.contains(&chunk_idx) {
            // Peer already filled this chunk — continue execution.
            return true;
        }
        let commit_off = chunk_idx * CHUNK;
        let commit_len = CHUNK.min(region.reserved - commit_off);
        (
            region.base,
            region.reserved,
            region.file_size,
            region.fh,
            chunk_idx,
            commit_off,
            commit_len,
        )
    };
    let _ = reserved;

    let page = (base + commit_off) as *mut c_void;
    let committed = VirtualAlloc(page, commit_len, MEM_COMMIT, PAGE_READWRITE);
    if committed.is_null() {
        return false;
    }

    // Stream only the file-backed portion; zero the rest of the last page/chunk.
    let file_off = commit_off as u64;
    let readable = if file_off >= file_size {
        0usize
    } else {
        ((file_size - file_off) as usize).min(commit_len)
    };

    if readable > 0 {
        let Some(client) = crate::fuse_client::global() else {
            VirtualFree(page, commit_len, MEM_DECOMMIT);
            return false;
        };
        let dest = core::slice::from_raw_parts_mut(page as *mut u8, readable);
        match client.read_fragmented(fh, file_off, dest) {
            Ok(n) => {
                if n < readable {
                    dest[n..].fill(0);
                }
            }
            Err(_) => {
                // Zeros so we don't spin-fault forever on a hard I/O error.
                dest.fill(0);
            }
        }
    }
    if readable < commit_len {
        let tail = core::slice::from_raw_parts_mut(
            (page as *mut u8).add(readable),
            commit_len - readable,
        );
        tail.fill(0);
    }

    if let Ok(mut g) = REGIONS.lock() {
        if let Some(region) = g.get_mut(&base) {
            region.committed.insert(chunk_idx);
        }
    }
    true
}

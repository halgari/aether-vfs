//! Lazy FUSE data sections: emulate mmap without preloading multi‑GiB BSAs.
//!
//! CreateSection reserves address space (and optionally warms the first window).
//! MapView returns that base. Further first-touch faults commit 256 KiB chunks
//! and stream them from the director via the shared bulk arena.
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
    VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE,
};

const PAGE: usize = 4096;
/// Commit/fill this many bytes per fault.
const CHUNK: usize = 256 * 1024;
/// Warm this many bytes at CreateSection so header/index peeks don't depend on VEH.
const WARM_BYTES: usize = 2 * 1024 * 1024;
const MAX_LAZY: u64 = 3 * 1024 * 1024 * 1024;

const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;
const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
const STATUS_ACCESS_VIOLATION: i32 = 0xC0000005u32 as i32;

struct LazyRegion {
    base: usize,
    reserved: usize,
    file_size: u64,
    fh: u64,
    committed: HashSet<usize>,
}

static REGIONS: Mutex<BTreeMap<usize, LazyRegion>> = Mutex::new(BTreeMap::new());
static VEH_INSTALLED: AtomicBool = AtomicBool::new(false);
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
    let h = unsafe { AddVectoredExceptionHandler(1, Some(veh_handler)) };
    if h.is_null() {
        VEH_INSTALLED.store(false, Ordering::SeqCst);
    }
}

/// Reserve VA for a director-backed data section; warm first [`WARM_BYTES`].
///
/// Returns synthetic section handle for [`crate::zipserve::map_view`].
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
    let base = VirtualAlloc(core::ptr::null(), reserved, MEM_RESERVE, PAGE_NOACCESS);
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
    // Warm header/index window so first peeks work even if VEH is slow/racy.
    let warm = (file_size as usize).min(WARM_BYTES);
    if warm > 0 {
        let _ = ensure_range(base_u, 0, warm);
    }
    crate::zipserve::register_mapped_image(base_u, file_size)
}

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

/// Free a lazy region containing `addr`.
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
    if IN_VEH.with(|c| c.get()) {
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
        let mut g = match REGIONS.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        let Some((_, region)) = g
            .range(..=addr)
            .next_back()
            .filter(|(_, r)| addr < r.base + r.reserved)
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
        // Fill on a worker with a large stack — never deep FUSE I/O on the
        // game's 1 MiB primary stack (or inside a tight VEH frame).
        let page_u = page as usize;
        let n_read = std::thread::Builder::new()
            .name("vfs-lazy-fill".into())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                let client = crate::fuse_client::global()?;
                let dest =
                    unsafe { core::slice::from_raw_parts_mut(page_u as *mut u8, readable) };
                client.read_fragmented(fh, file_off, dest).ok()
            })
            .ok()
            .and_then(|j| j.join().ok())
            .flatten();
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

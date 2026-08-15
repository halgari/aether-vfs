//! Synthetic *section* bookkeeping for live PE image mapping.
//!
//! **This file used to be two unrelated things.** The half it is named for —
//! serving zip-window bytes out of memory-mapped container files behind
//! synthetic *file* handles — is gone (gate 4 task 7). Zip-backed content is
//! the director's to serve over the ring; the shim no longer opens, maps, or
//! reads container files itself, and `Decision::Serve`, `open_synth`,
//! `ZIP_MAPS` and `copy_window_to_file` went with that.
//!
//! What remains is the half that never had anything to do with zips: a table of
//! synthetic section handles over address ranges the shim already owns, plus a
//! refcounted table of the views mapped out of them. `register_mapped_image`
//! takes a PE image [`crate::lazy_section`] or `fuse_create_section`'s
//! `SEC_IMAGE` path has already mapped into this process and hands back a
//! handle that `NtMapViewOfSection` can answer from — no second mapping, and no
//! file behind it at all. The module keeps its old name only because renaming
//! it would churn every call site in `hook.rs` and `lazy_section.rs` for no
//! behavioural gain.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// Tag bit (2^45) marking a synthetic *section* handle (an `NtCreateSection`
/// result); real kernel handles never reach this magnitude. The sign bit
/// (2^63) stays clear so the value is a positive handle, never confused with
/// pseudo-handles (-1..-6) or `INVALID_HANDLE_VALUE`. Distinct from
/// `fuse_synth`'s `FUSE_TAG` (2^47), which marks synthetic *file* handles.
const SYNTH_SECTION_TAG: usize = 0x0000_2000_0000_0000;

/// A synthetic section: the base address of the region it covers and its byte
/// length. The region is always memory the shim itself mapped — there is no
/// file object behind one of these.
struct SynthSection {
    window: usize,
    length: u64,
}

// Raw addresses stored as usize -> Send/Sync-safe in the maps.
static SYNTH_SECTIONS: Mutex<BTreeMap<usize, SynthSection>> = Mutex::new(BTreeMap::new());

/// One outstanding synthetic view. Callers may map the same base twice (two
/// views at the same section offset), so keep a refcount rather than letting
/// the first unmap forget a base the process is still using.
struct SynthView {
    length: u64,
    refs: u32,
}

/// Mapped synthetic views: base address → view (for UnmapView no-op).
static SYNTH_VIEWS: Mutex<BTreeMap<usize, SynthView>> = Mutex::new(BTreeMap::new());
static NEXT_SLOT: Mutex<usize> = Mutex::new(0);

/// Whether `handle` is a synthetic section (from [`register_mapped_image`]).
pub fn is_synth_section(handle: isize) -> bool {
    (handle as usize) & SYNTH_SECTION_TAG != 0
}

/// Register an already-mapped PE image (from `map_image_from_pe_bytes_local`,
/// or a [`crate::lazy_section`] reservation) as a synthetic section so
/// `MapViewOfSection` returns `base`.
pub fn register_mapped_image(base: usize, size: u64) -> Option<isize> {
    let mut slot = NEXT_SLOT.lock().ok()?;
    let handle = SYNTH_SECTION_TAG | (*slot << 3);
    *slot += 1;
    drop(slot);
    SYNTH_SECTIONS
        .lock()
        .ok()?
        .insert(handle, SynthSection { window: base, length: size });
    Some(handle as isize)
}

/// Close a synthetic section handle, returning the window base it covered.
///
/// The base lets the caller release shim-owned address space (see
/// [`crate::lazy_section::on_section_closed`]). Views may still be outstanding;
/// the owner frees only once the last one is unmapped.
pub fn close_section(handle: isize) -> Option<usize> {
    SYNTH_SECTIONS
        .lock()
        .ok()?
        .remove(&(handle as usize))
        .map(|s| s.window)
}

/// Map a view of a synthetic section into the current process.
///
/// Returns `(base, view_size)` on success. The base is a pointer into the
/// region the section already covers (no extra mapping), so [`unmap_view`] must
/// be used rather than `UnmapViewOfFile`, which would tear down memory the
/// owner is still tracking.
pub fn map_view(
    section_handle: isize,
    section_offset: u64,
    view_size: u64,
) -> Option<(usize, u64)> {
    let t = SYNTH_SECTIONS.lock().ok()?;
    let s = t.get(&(section_handle as usize))?;
    if section_offset > s.length {
        return None;
    }
    let max = s.length - section_offset;
    let size = if view_size == 0 { max } else { view_size.min(max) };
    if size == 0 && max == 0 {
        // Empty file: still "succeed" with a non-null? Prefer fail.
        return None;
    }
    let base = s.window.checked_add(section_offset as usize)?;
    drop(t);
    if let Ok(mut views) = SYNTH_VIEWS.lock() {
        views
            .entry(base)
            .and_modify(|v| {
                v.refs += 1;
                v.length = v.length.max(size);
            })
            .or_insert(SynthView { length: size, refs: 1 });
    }
    Some((base, size))
}

/// Whether any synthetic view still falls inside `[base, base+len)`.
///
/// Owners of shim-allocated section memory use this to decide when a region is
/// unreferenced. On a poisoned lock this reports "still in use": leaking a
/// reservation is recoverable, freeing one out from under a live view is not.
pub fn has_view_in(base: usize, len: usize) -> bool {
    match SYNTH_VIEWS.lock() {
        Ok(v) => v.range(base..base.saturating_add(len)).next().is_some(),
        Err(_) => true,
    }
}

/// Whether `base` is a synthetic mapped view (should no-op on UnmapView).
pub fn is_synth_view(base: usize) -> bool {
    SYNTH_VIEWS
        .lock()
        .map(|v| v.contains_key(&base))
        .unwrap_or(false)
}

/// Forget one reference to a synthetic mapped view (do not unmap the underlying
/// region). Returns true when this was the *last* reference to `base`.
pub fn unmap_view(base: usize) -> bool {
    let Ok(mut views) = SYNTH_VIEWS.lock() else {
        return false;
    };
    match views.get_mut(&base) {
        Some(v) if v.refs > 1 => {
            v.refs -= 1;
            false
        }
        Some(_) => {
            views.remove(&base);
            true
        }
        None => false,
    }
}

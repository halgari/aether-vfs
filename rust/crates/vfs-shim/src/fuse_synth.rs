//! Synthetic handles that store director FUSE file handles.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// Fuse *file* handles use 2^47. Distinct from `zipserve`'s synthetic *section*
/// tag (2^45), the only other tag still in use — 2^46 belonged to the
/// zip-window file handles gate 4 task 7 removed and is now unassigned.
const FUSE_TAG: usize = 0x0000_8000_0000_0000;

struct FuseOpen {
    fh: u64,
    size: u64,
    is_dir: bool,
    position: u64,
    /// `FILE_APPEND_DATA` granted without `FILE_WRITE_DATA` (NT's
    /// append-only access). A real file object enforces "every write lands
    /// at the current end of file" at the kernel level, ignoring whatever
    /// offset the caller passes; a synthetic handle has no kernel FCB to do
    /// that, so `write_hook` does it here instead, keyed off this flag.
    append_only: bool,
    /// Absolute NT/Win path for relative-open resolution (esp. directories).
    abs_path: Option<String>,
}

static TABLE: Mutex<BTreeMap<usize, FuseOpen>> = Mutex::new(BTreeMap::new());
static NEXT: Mutex<usize> = Mutex::new(1);

pub fn is_fuse_synth(handle: isize) -> bool {
    let h = handle as usize;
    h & FUSE_TAG != 0
}

pub fn open_fuse(fh: u64, size: u64, is_dir: bool) -> Option<isize> {
    open_fuse_at(fh, size, is_dir, None)
}

pub fn open_fuse_at(fh: u64, size: u64, is_dir: bool, abs_path: Option<String>) -> Option<isize> {
    open_fuse_at_ex(fh, size, is_dir, abs_path, false)
}

/// Like [`open_fuse_at`], but for a handle opened with NT append-only access
/// (`FILE_APPEND_DATA` without `FILE_WRITE_DATA`): the tracked position seeds
/// at the file's *current* size (end of file), matching what the kernel
/// would enforce for a real handle opened the same way. Seeding at `0` — what
/// every caller did before this existed — makes the first append on a
/// reopened handle overwrite from the start instead, which is silent data
/// corruption disguised as a successful append.
pub fn open_fuse_at_ex(
    fh: u64,
    size: u64,
    is_dir: bool,
    abs_path: Option<String>,
    append_only: bool,
) -> Option<isize> {
    let mut next = NEXT.lock().ok()?;
    let slot = *next;
    *next = next.wrapping_add(1);
    let handle = (slot & !FUSE_TAG) | FUSE_TAG;
    let mut g = TABLE.lock().ok()?;
    g.insert(
        handle,
        FuseOpen {
            fh,
            size,
            is_dir,
            position: if append_only { size } else { 0 },
            append_only,
            abs_path,
        },
    );
    Some(handle as isize)
}

pub fn lookup(handle: isize) -> Option<(u64, u64, bool, u64, bool)> {
    let g = TABLE.lock().ok()?;
    let e = g.get(&(handle as usize))?;
    Some((e.fh, e.size, e.is_dir, e.position, e.append_only))
}

/// Absolute path recorded for a FUSE handle (for relative RootDirectory opens).
pub fn abs_path(handle: isize) -> Option<String> {
    let g = TABLE.lock().ok()?;
    g.get(&(handle as usize))?.abs_path.clone()
}

pub fn set_position(handle: isize, pos: u64) {
    if let Ok(mut g) = TABLE.lock() {
        if let Some(e) = g.get_mut(&(handle as usize)) {
            e.position = pos;
        }
    }
}

/// Update the cached size after a successful truncate so later reads on this
/// handle see the new EOF.
pub fn set_size(handle: isize, size: u64) {
    if let Ok(mut g) = TABLE.lock() {
        if let Some(e) = g.get_mut(&(handle as usize)) {
            e.size = size;
            if e.position > size {
                e.position = size;
            }
        }
    }
}

pub fn close_fuse(handle: isize) -> Option<u64> {
    let mut g = TABLE.lock().ok()?;
    g.remove(&(handle as usize)).map(|e| e.fh)
}

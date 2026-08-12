//! Synthetic handles that store director FUSE file handles (no zip windows).

use std::collections::BTreeMap;
use std::sync::Mutex;

/// Distinct from zipserve SYNTH_TAG (2^46): fuse handles use 2^47.
const FUSE_TAG: usize = 0x0000_8000_0000_0000;

struct FuseOpen {
    fh: u64,
    size: u64,
    is_dir: bool,
    position: u64,
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
            position: 0,
            abs_path,
        },
    );
    Some(handle as isize)
}

pub fn lookup(handle: isize) -> Option<(u64, u64, bool, u64)> {
    let g = TABLE.lock().ok()?;
    let e = g.get(&(handle as usize))?;
    Some((e.fh, e.size, e.is_dir, e.position))
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

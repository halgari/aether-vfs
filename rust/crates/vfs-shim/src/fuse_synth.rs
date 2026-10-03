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
    /// This handle's file in the read cache (`crate::read_cache`), when the
    /// cache is on and this is a file. Whether reads through it may be served
    /// from the cache is the ref's to say ([`vfs_ipc::FileRef::cacheable`]);
    /// a write handle holds one too, so a write or truncate through it can
    /// drop the file.
    cache: Option<vfs_ipc::FileRef>,
}

/// What a read through a handle needs, under one lock: see [`lookup_read`].
pub struct ReadView {
    pub fh: u64,
    pub size: u64,
    pub position: u64,
    pub cache: Option<vfs_ipc::FileRef>,
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
            cache: None,
        },
    );
    Some(handle as isize)
}

/// Attach the handle's read-cache file (`crate::read_cache::register`).
pub fn set_cache(handle: isize, cache: vfs_ipc::FileRef) {
    if let Ok(mut g) = TABLE.lock() {
        if let Some(e) = g.get_mut(&(handle as usize)) {
            e.cache = Some(cache);
        }
    }
}

/// The handle's read-cache file, if it has one.
pub fn cache(handle: isize) -> Option<vfs_ipc::FileRef> {
    let g = TABLE.lock().ok()?;
    g.get(&(handle as usize))?.cache.clone()
}

/// [`lookup`] for the read path: the director handle, size, position and
/// read-cache file, taken under the one lock acquisition a read pays for.
pub fn lookup_read(handle: isize) -> Option<ReadView> {
    let g = TABLE.lock().ok()?;
    let e = g.get(&(handle as usize))?;
    Some(ReadView {
        fh: e.fh,
        size: e.size,
        position: e.position,
        cache: e.cache.clone(),
    })
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

/// Record that the file behind `handle` is now at `abs_path`: it was renamed
/// through this handle, and what the handle is finally named, and its file
/// id, follow the file.
pub fn set_abs_path(handle: isize, abs_path: String) {
    if let Ok(mut g) = TABLE.lock() {
        if let Some(e) = g.get_mut(&(handle as usize)) {
            e.abs_path = Some(abs_path);
        }
    }
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

/// Raise the cached size to at least `end` after a write that reached there.
///
/// Not [`set_size`]: two writes on one handle can be in flight at once, each
/// having read the size before either finished, and whichever stores last
/// would win — the earlier-ending write shrinking the file under the other,
/// so that reads past it report end of file and the position is pulled back
/// for the next append to overwrite. The maximum, taken under the table
/// lock, does not depend on the order they finish in.
pub fn grow_size(handle: isize, end: u64) {
    if let Ok(mut g) = TABLE.lock() {
        if let Some(e) = g.get_mut(&(handle as usize)) {
            e.size = e.size.max(end);
        }
    }
}

pub fn close_fuse(handle: isize) -> Option<u64> {
    let mut g = TABLE.lock().ok()?;
    g.remove(&(handle as usize)).map(|e| e.fh)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two writes on one handle that both read the size before either
    /// finished: [0, 100) and [100, 200). Whichever reports last, the file
    /// is 200 bytes long and the position has not been pulled back.
    #[test]
    fn the_size_after_overlapping_writes_is_the_furthest_end_in_either_order() {
        for ends in [[100u64, 200], [200, 100]] {
            let h = open_fuse_at_ex(77, 0, false, None, false).unwrap();
            set_position(h, 200);
            for end in ends {
                grow_size(h, end);
            }
            let (_, size, _, position, _) = lookup(h).unwrap();
            assert_eq!((size, position), (200, 200), "ends reported as {ends:?}");
            close_fuse(h);
        }
    }
}

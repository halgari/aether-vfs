//! The process's [`vfs_ipc::ReadCache`]: small reads of files the director
//! served immutable are answered from it instead of over the ring.
//!
//! The cache itself, and its coherence rule, are `vfs_ipc::readcache` (OS-free
//! so the native tests and `ring-bench` run the same code). This module only
//! holds the one instance and says when it is off: `VFS_SHIM_READ_CACHE=0`
//! ([`vfs_env::SHIM_READ_CACHE`]), read once.
//!
//! Where the shim feeds it, all in `hook/`:
//!
//! - **open** (`try_fuse_create`): every file handle the director opens is
//!   registered with the open reply's `immutable` and `mount_gen`, and whether
//!   it was a write open. A write or mutable open drops the file for good.
//! - **read** (`read_hook_body`): a synchronous read shorter than the threshold
//!   on a cacheable handle is offered to the cache first; `None` falls back to
//!   the uncached read, so the bytes, the IO_STATUS_BLOCK and the file
//!   position are what the uncached path produces.
//! - **write / truncate** (`write_hook_body`, `FileEndOfFileInformation`): the
//!   handle's file is dropped.
//! - **delete / rename** (`NtDeleteFile`, the set-info classes): the path, and
//!   for a rename the target too, is dropped with everything under it.

use std::sync::OnceLock;

use vfs_ipc::{CacheConfig, CacheStats, FileRef, FileReport, ReadCache};

static CACHE: OnceLock<Option<ReadCache>> = OnceLock::new();

/// The cache, unless `VFS_SHIM_READ_CACHE` turned it off, with the budget
/// `VFS_SHIM_READ_CACHE_MIB` sets.
pub(crate) fn get() -> Option<&'static ReadCache> {
    CACHE
        .get_or_init(|| {
            vfs_env::opt_out(vfs_env::SHIM_READ_CACHE).then(|| {
                ReadCache::new(config(
                    vfs_env::text(vfs_env::SHIM_READ_CACHE_MIB).as_deref(),
                ))
            })
        })
        .as_ref()
}

/// The configuration for a `VFS_SHIM_READ_CACHE_MIB` value: the default
/// budget for one that is absent, not a number, or zero; and never less
/// than 1 MiB, the most a single fetch reserves.
fn config(mib: Option<&str>) -> CacheConfig {
    let mut cfg = CacheConfig::default();
    if let Some(m) = mib
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&m| m > 0)
    {
        cfg.max_bytes = m.saturating_mul(1 << 20);
    }
    cfg
}

/// The busiest files, for the stats report.
pub(crate) fn top_files(n: usize) -> Vec<FileReport> {
    get().map(|c| c.top_files(n)).unwrap_or_default()
}

/// Register a file handle the director just opened. `None` when the cache is
/// off (and for directories, which are never read through it).
pub(crate) fn register(
    root: u32,
    vpath: &str,
    resp: &vfs_protocol::OpenResp,
    write: bool,
) -> Option<FileRef> {
    if resp.is_dir {
        return None;
    }
    Some(get()?.register(
        root,
        vpath,
        resp.size,
        resp.mount_gen,
        resp.immutable,
        write,
    ))
}

/// The file behind a handle changed through this process.
pub(crate) fn invalidate(f: &FileRef) {
    if let Some(c) = get() {
        c.invalidate(f);
    }
}

/// A path was deleted or renamed (or renamed onto) through this process.
pub(crate) fn invalidate_path(root: u32, vpath: &str) {
    if let Some(c) = get() {
        c.invalidate_path(root, vpath);
    }
}

/// For the stats report: `None` when the cache is off.
pub(crate) fn stats() -> Option<CacheStats> {
    get().map(ReadCache::stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_comes_from_the_switch_in_mib_or_defaults() {
        assert_eq!(config(None).max_bytes, 256 << 20);
        assert_eq!(config(Some("512")).max_bytes, 512 << 20);
        assert_eq!(config(Some(" 64 ")).max_bytes, 64 << 20);
        for bad in ["", "lots", "0", "-3"] {
            assert_eq!(config(Some(bad)).max_bytes, 256 << 20, "{bad:?}");
        }
    }
}

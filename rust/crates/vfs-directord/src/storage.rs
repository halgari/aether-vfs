//! The daemon's storage directory and opening it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use vfs_embed::{Storage, StorageConfig};

/// The daemon's storage directory: `flag` (`--storage-dir`), else
/// `VFS_STORAGE_DIR`, else `<home>/storage`, where the aether-vfs home is
/// `VFS_HOME`, else — on unix — `$XDG_DATA_HOME/aether-vfs`, then
/// `$HOME/.local/share/aether-vfs`; on Windows `%LOCALAPPDATA%\aether-vfs`.
/// `None` when nothing names a home at all.
///
/// `env` is the environment lookup (`std::env::var_os` in the daemon), so the
/// order can be tested without touching the process environment.
pub fn storage_dir_from(
    flag: Option<&Path>,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    storage_dir_for(flag, env, cfg!(windows))
}

/// [`storage_dir_from`] with the OS as a parameter, so both orders are tested
/// on either OS.
fn storage_dir_for(
    flag: Option<&Path>,
    env: &dyn Fn(&str) -> Option<OsString>,
    windows: bool,
) -> Option<PathBuf> {
    if let Some(dir) = flag {
        return Some(dir.to_path_buf());
    }
    let set = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(dir) = set(vfs_env::STORAGE_DIR) {
        return Some(dir);
    }
    let home = match set(vfs_env::HOME) {
        Some(h) => h,
        None if windows => set("LOCALAPPDATA")?.join("aether-vfs"),
        None => set("XDG_DATA_HOME")
            .map(|x| x.join("aether-vfs"))
            .or_else(|| set("HOME").map(|h| h.join(".local/share/aether-vfs")))?,
    };
    Some(home.join("storage"))
}

/// Open the daemon's storage at `dir`, with `cache_max_gib` as the cache
/// budget (the default otherwise; `0` is refused), and print what
/// reconciliation repaired.
///
/// A failure — above all another daemon holding the directory — is an error
/// naming the directory and how to choose another.
pub fn open_daemon_storage(dir: &Path, cache_max_gib: Option<u64>) -> Result<Arc<Storage>, String> {
    let mut cfg = StorageConfig::default();
    if let Some(gib) = cache_max_gib {
        if gib == 0 {
            return Err(
                "--cache-max-gib 0: the cache budget must be at least 1 GiB (omit the flag \
                 for the default, 32)"
                    .to_string(),
            );
        }
        cfg.cache_max_bytes = gib.saturating_mul(1 << 30);
    }
    let storage = Storage::open(dir, cfg).map_err(|e| {
        let hint = if e.is_locked() {
            " (is another vfs daemon using it?)"
        } else {
            ""
        };
        format!(
            "cannot open storage at {}: {e}{hint}; choose another directory with \
             --storage-dir or {}",
            dir.display(),
            vfs_env::STORAGE_DIR
        )
    })?;
    let r = storage.last_reconcile();
    let repaired = r.emptied_files.len()
        + r.zero_filled_files.len()
        + r.corrupt_files.len()
        + r.resized_rows.len()
        + r.orphans_deleted as usize
        + r.cache_rows_dropped as usize
        + r.failed_repairs.len();
    if repaired > 0 {
        eprintln!(
            "vfs daemon: storage at {} was reconciled at open:",
            dir.display()
        );
        for (layer, path) in &r.emptied_files {
            eprintln!("  layer {layer:?}: {path} lost its data and is now empty");
        }
        for (layer, path) in &r.zero_filled_files {
            eprintln!("  layer {layer:?}: {path} had missing blocks, now zeros");
        }
        for (layer, path) in &r.corrupt_files {
            eprintln!(
                "  layer {layer:?}: {path} is CORRUPT: blocks of the closed file are \
                 missing (reads of them fail)"
            );
        }
        for (layer, path) in &r.resized_rows {
            eprintln!("  layer {layer:?}: {path} length corrected to the store's");
        }
        if r.orphans_deleted > 0 {
            eprintln!("  {} unreferenced store file(s) deleted", r.orphans_deleted);
        }
        for what in &r.failed_repairs {
            eprintln!("  repair failed (retried at the next open): {what}");
        }
        if r.cache_rows_dropped > 0 {
            eprintln!(
                "  {} cache entr(ies) without data dropped",
                r.cache_rows_dropped
            );
        }
    }
    Ok(storage)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--storage-dir`, then `VFS_STORAGE_DIR`, then `<home>/storage`: the
    /// home is `VFS_HOME`, then XDG/HOME on unix and LOCALAPPDATA on Windows.
    #[test]
    fn storage_dir_resolution_order() {
        use std::collections::HashMap;
        use std::ffi::OsString;
        let env = |pairs: &[(&str, &str)]| {
            let m: HashMap<String, OsString> = pairs
                .iter()
                .map(|(k, v)| (k.to_string(), OsString::from(v)))
                .collect();
            move |k: &str| m.get(k).cloned()
        };
        let p = |s: &str| PathBuf::from(s);
        let all = env(&[
            (vfs_env::STORAGE_DIR, "/env/storage"),
            (vfs_env::HOME, "/vfs-home"),
            ("XDG_DATA_HOME", "/xdg"),
            ("HOME", "/home/u"),
            ("LOCALAPPDATA", "/lad"),
        ]);
        for windows in [false, true] {
            let flag = p("/flag/storage");
            assert_eq!(
                storage_dir_for(Some(&flag), &all, windows),
                Some(flag.clone())
            );
            assert_eq!(
                storage_dir_for(None, &all, windows),
                Some(p("/env/storage"))
            );
            let vfs_home = env(&[(vfs_env::HOME, "/vfs-home"), ("HOME", "/home/u")]);
            assert_eq!(
                storage_dir_for(None, &vfs_home, windows),
                Some(p("/vfs-home").join("storage"))
            );
            assert_eq!(storage_dir_for(None, &env(&[]), windows), None);
        }
        // Unix: XDG, then HOME; LOCALAPPDATA is not consulted.
        let rest = env(&[
            ("XDG_DATA_HOME", "/xdg"),
            ("HOME", "/home/u"),
            ("LOCALAPPDATA", "/lad"),
        ]);
        assert_eq!(
            storage_dir_for(None, &rest, false),
            Some(p("/xdg").join("aether-vfs").join("storage"))
        );
        assert_eq!(
            storage_dir_for(
                None,
                &env(&[("HOME", "/home/u"), ("LOCALAPPDATA", "/lad")]),
                false
            ),
            Some(p("/home/u").join(".local/share/aether-vfs").join("storage"))
        );
        assert_eq!(
            storage_dir_for(None, &env(&[("LOCALAPPDATA", "/lad")]), false),
            None
        );
        // Windows: LOCALAPPDATA wins over HOME and XDG.
        assert_eq!(
            storage_dir_for(None, &rest, true),
            Some(p("/lad").join("aether-vfs").join("storage"))
        );
        assert_eq!(
            storage_dir_for(None, &env(&[("HOME", "/home/u")]), true),
            None
        );
    }

    /// A storage directory another daemon holds is refused with an error that
    /// names the directory and the way to pick another.
    #[test]
    fn a_locked_storage_dir_is_refused_by_name() {
        let dir = vfs_testkit::tempdir().unwrap();
        let held = open_daemon_storage(dir.path(), None).expect("first open");
        let e = open_daemon_storage(dir.path(), None)
            .err()
            .expect("second open refused");
        assert!(
            e.contains(&dir.path().display().to_string())
                && e.contains("--storage-dir")
                && e.contains("another vfs daemon"),
            "{e}"
        );
        drop(held);
    }

    /// Only a lock asks about another daemon: a directory that is a file is a
    /// different failure and must not be blamed on one.
    #[test]
    fn only_a_locked_storage_dir_blames_another_daemon() {
        let dir = vfs_testkit::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        let e = open_daemon_storage(&file, None)
            .err()
            .expect("a file is refused");
        assert!(
            e.contains("--storage-dir") && !e.contains("another vfs daemon"),
            "{e}"
        );
    }

    /// `--cache-max-gib 0` is refused rather than silently making every
    /// cached block evictable at once.
    #[test]
    fn a_zero_cache_budget_is_refused() {
        let dir = vfs_testkit::tempdir().unwrap();
        let e = open_daemon_storage(dir.path(), Some(0))
            .err()
            .expect("0 refused");
        assert!(e.contains("--cache-max-gib"), "{e}");
        assert!(open_daemon_storage(dir.path(), Some(1)).is_ok());
    }
}

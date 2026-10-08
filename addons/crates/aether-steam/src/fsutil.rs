//! Small file helpers: atomic writes with owner-only permissions.
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Write `bytes` to `path` atomically: write a sibling
/// `<name>.<pid>.<counter>.partial`, sync it, then rename over `path`. The
/// pid and a per-process counter keep two writers of the same `path` —
/// different processes, or two concurrent calls in this one — from
/// colliding on the same temp file and corrupting or truncating each
/// other's write; the final `rename` is still what makes the write atomic.
/// With `private`, the file is created with mode 0600 and a newly created
/// parent directory with mode 0700 (Unix; no-op elsewhere).
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], private: bool) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other(format!("{} has no parent directory", path.display())))?;
    create_dir(parent, private)?;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = path
        .file_name()
        .ok_or_else(|| io::Error::other(format!("{} has no file name", path.display())))?
        .to_os_string();
    name.push(format!(".{}.{n}.partial", std::process::id()));
    let tmp = parent.join(name);
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let result = (|| {
        let mut f = opts.open(&tmp)?;
        #[cfg(unix)]
        if private {
            // A stale .partial from an older run keeps its old mode; fix it.
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn create_dir(dir: &Path, private: bool) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
}

/// Read a file, mapping "not found" to `None`.
pub(crate) fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_and_leaves_no_partial() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a/b/file.bin");
        write_atomic(&p, b"one", false).unwrap();
        write_atomic(&p, b"two", false).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        assert!(!dir.path().join("a/b/file.bin.partial").exists());
        assert_eq!(leftover_partials(dir.path()), Vec::<String>::new());
    }

    /// Every temp-file name under `dir`, recursively, ending in `.partial`.
    fn leftover_partials(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        for entry in walkdir(dir) {
            let name = entry.file_name().unwrap().to_string_lossy().into_owned();
            if name.ends_with(".partial") {
                out.push(name);
            }
        }
        out
    }

    fn walkdir(dir: &Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                out.extend(walkdir(&path));
            } else {
                out.push(path);
            }
        }
        out
    }

    #[test]
    fn concurrent_writes_to_the_same_path_never_collide_on_the_temp_name() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("shared.bin");
        let contents: Vec<Vec<u8>> = (0u8..8).map(|i| vec![i; 4096]).collect();
        std::thread::scope(|scope| {
            for c in &contents {
                let p = &p;
                scope.spawn(move || write_atomic(p, c, false).unwrap());
            }
        });
        // Whichever write finished last "wins", but the result must be one
        // whole write, never a mix of two, and no stray temp file left
        // behind by a rename that the previous, shared-name scheme could
        // have silently overwritten mid-write.
        let result = fs::read(&p).unwrap();
        assert!(contents.iter().any(|c| c == &result), "{result:?}");
        assert_eq!(leftover_partials(dir.path()), Vec::<String>::new());
    }

    #[cfg(unix)]
    #[test]
    fn private_write_is_0600_in_0700_dir() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("cfg/secret.json");
        write_atomic(&p, b"{}", true).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&p), 0o600);
        assert_eq!(mode(&dir.path().join("cfg")), 0o700);
    }
}

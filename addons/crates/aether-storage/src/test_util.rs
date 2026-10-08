//! Helpers for tests of crash behaviour: this crate's, and (feature
//! `test-hooks`) other crates'.

/// Copies the storage directory as it is on disk right now: what a process
/// killed at this instant leaves behind. redb keeps non-durable commits out of
/// the file's committed state, so a row that was never made durable is absent
/// from the copy.
///
/// **Quiesce writers first.** The copy is file by file, not one atomic moment:
/// a write that lands while it runs can be seen in one file and not in
/// another, which is a state no kill leaves. A file that vanishes between the
/// listing and its copy (a store file deleted by a background step) is skipped.
/// Any other error is returned.
///
/// Not on Windows, where redb and the store hold mandatory locks that make a
/// live copy fail.
#[cfg(not(windows))]
pub fn snapshot_as_killed(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    use std::io::ErrorKind::NotFound;
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = match e {
            Err(err) if err.kind() == NotFound => continue,
            e => e?,
        };
        let dest = to.join(e.file_name());
        let copied = match e.file_type() {
            Ok(t) if t.is_dir() => snapshot_as_killed(&e.path(), &dest),
            Ok(_) => std::fs::copy(e.path(), dest).map(|_| ()),
            Err(err) => Err(err),
        };
        match copied {
            Err(err) if err.kind() == NotFound => {}
            other => other?,
        }
    }
    Ok(())
}

/// What [`Storage::crash_on_drop_for_tests`](crate::Storage::crash_on_drop_for_tests)
/// leaves behind. redb publishes its non-durable commits when its database is
/// dropped, so dropping the storage after a "crash" would make them durable. This
/// holds a [`snapshot_as_killed`] copy of the directory taken at the crash, and
/// puts it back when it is dropped, which is after every other field of the
/// storage has released its files and its lock.
#[cfg(all(any(test, feature = "test-hooks"), not(windows)))]
pub(crate) struct CrashImage {
    dir: std::path::PathBuf,
    image: tempfile::TempDir,
}

#[cfg(all(any(test, feature = "test-hooks"), not(windows)))]
impl CrashImage {
    pub(crate) fn take(dir: &std::path::Path) -> std::io::Result<Self> {
        // Beside the directory, not in the host's temp dir: a test's directories
        // are already somewhere it chose.
        let parent = dir.parent().filter(|p| !p.as_os_str().is_empty());
        let image = tempfile::tempdir_in(parent.unwrap_or(std::path::Path::new(".")))?;
        snapshot_as_killed(dir, image.path())?;
        Ok(CrashImage {
            dir: dir.to_path_buf(),
            image,
        })
    }
}

#[cfg(all(any(test, feature = "test-hooks"), not(windows)))]
impl Drop for CrashImage {
    /// Never panics (it may run while another panic unwinds). If the image
    /// cannot be put back, the directory is left as the failed attempt left it
    /// and the error is logged; the test then sees a directory that is not a
    /// faithful crash, and the log says why.
    fn drop(&mut self) {
        if let Err(e) = self.restore() {
            tracing::error!(error = %e, dir = %self.dir.display(), "crash image not restored");
        }
    }
}

#[cfg(all(any(test, feature = "test-hooks"), not(windows)))]
impl CrashImage {
    fn restore(&self) -> std::io::Result<()> {
        // Copy the image next to the directory's contents first, so a failure
        // leaves the directory untouched rather than half wiped.
        let staged = tempfile::tempdir_in(self.dir.parent().unwrap_or(std::path::Path::new(".")))?;
        snapshot_as_killed(self.image.path(), staged.path())?;
        for e in std::fs::read_dir(&self.dir)? {
            let p = e?.path();
            if p.is_dir() {
                std::fs::remove_dir_all(&p)?;
            } else {
                std::fs::remove_file(&p)?;
            }
        }
        for e in std::fs::read_dir(staged.path())? {
            let e = e?;
            std::fs::rename(e.path(), self.dir.join(e.file_name()))?;
        }
        Ok(())
    }
}

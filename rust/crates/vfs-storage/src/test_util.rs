//! Helpers for tests of crash behaviour: this crate's, and (feature
//! `test-hooks`) other crates'.

/// Copies the storage directory as it is on disk right now: what a process
/// killed at this instant leaves behind. redb keeps non-durable commits out of
/// the file's committed state, so a row that was never made durable is absent
/// from the copy.
///
/// Not on Windows, where redb and the store hold mandatory locks that make a
/// live copy fail.
#[cfg(not(windows))]
pub fn snapshot_as_killed(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            snapshot_as_killed(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), dest).unwrap();
        }
    }
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
        let image = tempfile::tempdir()?;
        snapshot_as_killed(dir, image.path());
        Ok(CrashImage { dir: dir.to_path_buf(), image })
    }
}

#[cfg(all(any(test, feature = "test-hooks"), not(windows)))]
impl Drop for CrashImage {
    fn drop(&mut self) {
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for e in entries.flatten() {
                let p = e.path();
                let _ = if e.file_type().is_ok_and(|t| t.is_dir()) {
                    std::fs::remove_dir_all(p)
                } else {
                    std::fs::remove_file(p)
                };
            }
        }
        snapshot_as_killed(self.image.path(), &self.dir);
    }
}

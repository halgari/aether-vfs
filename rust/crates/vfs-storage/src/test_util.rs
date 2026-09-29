//! Helpers shared by the crate's tests.

/// Copies the storage directory as it is on disk right now: what a process
/// killed at this instant leaves behind. redb keeps non-durable commits out of
/// the file's committed state, so a row that was never made durable is absent
/// from the copy.
///
/// Not on Windows, where redb and the store hold mandatory locks that make a
/// live copy fail.
#[cfg(not(windows))]
pub(crate) fn snapshot(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            snapshot(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), dest).unwrap();
        }
    }
}

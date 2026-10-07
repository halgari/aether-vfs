//! The unit tests' temp directory: `target/[<triple>/]tmp`, never the host's
//! `/tmp`. (Cargo sets `CARGO_TARGET_TMPDIR` for integration tests only.)

use std::path::{Path, PathBuf};

pub(crate) fn dir() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    // target/[<triple>/]<profile>/deps/<exe>
    let target = exe
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .unwrap();
    let d = target.join("tmp");
    let _ = std::fs::create_dir_all(&d);
    d
}

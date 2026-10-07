//! Scratch directories for the unit tests: under the build's target directory,
//! never the host's `/tmp`. (Integration tests use `CARGO_TARGET_TMPDIR`, which
//! Cargo does not set for unit tests.)

use std::path::{Path, PathBuf};

use crate::Session;

/// A fresh `target/[<triple>/]tmp/<tag>-<pid>` directory, not yet created.
pub(crate) fn scratch_dir(tag: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    // target/[<triple>/]<profile>/deps/<exe>
    let target = exe
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .unwrap();
    let dir = target
        .join("tmp")
        .join(format!("vfs-embed-unit-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A fresh, empty, created scratch directory.
pub(crate) fn scratch_created(tag: &str) -> PathBuf {
    let dir = scratch_dir(tag);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A session whose root, overlay and state directories are under a scratch
/// directory of its own, instead of [`Session::new`]'s defaults in the temp dir.
pub(crate) fn session_in_scratch(tag: &str) -> Session {
    let base = scratch_dir(tag);
    let mut s = Session::new();
    s.set_root(base.join("root"));
    s.set_overlay(base.join("overlay"));
    s.set_state_dir(base.join("state"));
    s
}

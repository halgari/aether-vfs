//! Scratch directories for host-side tests: under the build's `target/`, never
//! the host's temp dir (on the owner's machine `/tmp` is RAM).
//!
//! Integration tests get `CARGO_TARGET_TMPDIR` from Cargo; unit tests do not, so
//! the root is derived from the running test binary instead
//! (`target/[<triple>/]<profile>/deps/<exe>` gives `target/[<triple>/]tmp`).
//! `tests/no_host_tmp.rs` fails the build when a test reaches for the host's
//! temp dir instead of these.
//!
//! Set `VFS_TEST_KEEP_SCRATCH` to any value to keep [`Scratch`] directories for
//! inspection.

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The directory all scratch space lives under. Created on demand.
pub fn scratch_root() -> PathBuf {
    let root = match std::env::var_os("CARGO_TARGET_TMPDIR") {
        Some(d) => PathBuf::from(d),
        None => derived_root(),
    };
    let _ = std::fs::create_dir_all(&root);
    root
}

fn derived_root() -> PathBuf {
    let from_exe = std::env::current_exe().ok().and_then(|exe| {
        let deps = exe.parent()?;
        (deps.file_name()? == "deps").then(|| deps.parent()?.parent().map(|t| t.join("tmp")))?
    });
    from_exe.unwrap_or_else(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("tmp")
    })
}

/// A path under [`scratch_root`] that is unique to this process and `name`.
/// Not created, not cleared, not removed: the drop-in for the old
/// `temp_dir().join(format!("<name>-{}", process::id()))` idiom, for tests that
/// manage the directory themselves.
pub fn scratch_path(name: &str) -> PathBuf {
    scratch_root().join(format!("{name}-{}", std::process::id()))
}

/// A fresh, empty, created directory, removed on drop (kept when
/// `VFS_TEST_KEEP_SCRATCH` is set). Derefs to [`Path`].
pub struct Scratch {
    path: PathBuf,
}

/// A unique directory for `name`, safe to call many times with the same name.
pub fn scratch_dir(name: &str) -> Scratch {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = scratch_root().join(format!("{name}-{}-{seq}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("create scratch dir");
    Scratch { path }
}

impl Scratch {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::env::var_os("VFS_TEST_KEEP_SCRATCH").is_none() {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// The drop-in for `vfs_testkit::tempdir()`: a [`tempfile::TempDir`] created under
/// [`scratch_root`], removed on drop like any other.
pub fn tempdir() -> std::io::Result<tempfile::TempDir> {
    tempfile::tempdir_in(scratch_root())
}

/// Point the process's `TMPDIR` at [`scratch_root`], so code under test that
/// defaults to the system temp dir (a daemon's per-session directories, say)
/// and the child processes it spawns land under `target/` too.
///
/// Changes the process environment, so call it before any thread exists: from
/// a `#[ctor::ctor]` function, which runs before `main`.
pub fn use_scratch_as_tmpdir() {
    std::env::set_var("TMPDIR", scratch_root());
}

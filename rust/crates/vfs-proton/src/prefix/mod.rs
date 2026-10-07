//! Per-session Wine prefixes: creation, root links into `drive_c`, the
//! per-launch lock and `wineserver` control.
//!
//! A session reaches host directories through symlinks inside `drive_c`
//! ([`Prefix::link_location`]) rather than drive letters, so a prefix needs no
//! `dosdevices` edits.

mod init;
mod links;
mod wineserver;

pub use init::{
    ensure, ensure_with, prefix_dir, PrefixInit, PROTON_GRAPHICS_OVERRIDES, PROTON_INIT_LOG,
    PROTON_INIT_TIMEOUT,
};
pub use links::{parse_location, LINK_MANIFEST};
pub use wineserver::WINESERVER_STOP_TIMEOUT;

use std::io;
use std::path::{Path, PathBuf};

use crate::layout::Root;

/// A session's private Wine prefix: the directory `WINEPREFIX` points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prefix {
    pub dir: PathBuf,
}

/// Why preparing or using a prefix failed.
#[derive(Debug)]
pub enum PrefixError {
    /// Filesystem I/O failed, including "the session id was rejected" and
    /// "`wineboot` could not even be launched" (e.g. binary not found).
    Io(io::Error),
    /// `wineboot -u` ran and exited non-zero for a reason other than the
    /// missing-32-bit-loader case. Carries its combined stdout/stderr.
    Wineboot(String),
    /// `runtime` is not a verified GE-Proton build. `PROTONPATH` defaults to
    /// stock Valve Proton, and silently launching a session on top of that
    /// default is the exact failure this crate exists to prevent, so this is
    /// always a hard error, never a fallback.
    NotGe(String),
    /// `wineboot` failed because no 32-bit runtime is installed. The `wine`
    /// launcher probes for the 32-bit loader even under `WINEARCH=win64`, so
    /// this can't be avoided by architecture choice — only by installing the
    /// packages.
    Missing32Bit,
    /// Another live launch holds this prefix's lock.
    Busy(PathBuf),
    /// A root location that cannot be linked into the prefix (not on `C:`,
    /// contains `..`, is the drive root, or is occupied by a real file, a
    /// directory, or a symlink aether-vfs did not create).
    BadLocation(String),
    /// Proton's own prefix setup ([`PrefixInit::Proton`]) could not run, did
    /// not finish, or finished without a prefix. Carries what happened and
    /// the tail of its log.
    ProtonInit(String),
}

impl std::fmt::Display for PrefixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrefixError::Io(e) => write!(f, "io error: {e}"),
            PrefixError::Wineboot(s) => write!(f, "wineboot failed: {s}"),
            PrefixError::NotGe(s) => write!(f, "runtime is not GE-Proton: {s}"),
            PrefixError::Busy(d) => {
                write!(f, "prefix {} is in use by another live launch", d.display())
            }
            PrefixError::BadLocation(s) => write!(f, "bad root location {s}"),
            PrefixError::ProtonInit(s) => write!(f, "proton prefix setup failed: {s}"),
            PrefixError::Missing32Bit => write!(
                f,
                "wineboot needs a 32-bit runtime: install lib32-glibc and \
                 lib32-gcc-libs (Arch) or your distro's equivalent packages"
            ),
        }
    }
}

impl std::error::Error for PrefixError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PrefixError::Io(e) => Some(e),
            PrefixError::Wineboot(_)
            | PrefixError::NotGe(_)
            | PrefixError::Missing32Bit
            | PrefixError::Busy(_)
            | PrefixError::BadLocation(_)
            | PrefixError::ProtonInit(_) => None,
        }
    }
}

impl From<io::Error> for PrefixError {
    fn from(e: io::Error) -> Self {
        PrefixError::Io(e)
    }
}

/// Held for one launch; the OS releases the lock when the file closes.
#[derive(Debug)]
pub struct PrefixLock {
    // Held, never read: closing the file releases the lock.
    _file: std::fs::File,
}

/// How long [`Prefix::lock`] keeps retrying a lock that looks held before it
/// reports [`PrefixError::Busy`].
const LOCK_RETRY_BUDGET: std::time::Duration = std::time::Duration::from_millis(500);

/// Deletes `root.sessions()/<session>` — prefix and all. Absent is fine.
pub fn remove_session(root: &Root, session: &str) -> io::Result<()> {
    let dir = root
        .try_session_dir(session)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
    match std::fs::remove_dir_all(&dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

impl Prefix {
    /// An exclusive lock on this prefix for the duration of one launch, so two
    /// live sessions cannot relink the same roots under each other.
    pub fn lock(&self) -> Result<PrefixLock, PrefixError> {
        std::fs::create_dir_all(&self.dir)?;
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join(".aether-vfs.lock"))?;
        // `flock` belongs to the open file description, and a `fork` in another
        // thread of this process copies the descriptor until the child execs
        // (the fd is close-on-exec). A lock dropped an instant ago can
        // therefore still look held, for microseconds to milliseconds. Retry
        // `WouldBlock` briefly, as `spawn_retrying_busy` does for `ETXTBSY`;
        // a live holder still fails, just `LOCK_RETRY_BUDGET` later.
        let deadline = std::time::Instant::now() + LOCK_RETRY_BUDGET;
        let mut pause = std::time::Duration::from_millis(1);
        loop {
            match f.try_lock() {
                Ok(()) => return Ok(PrefixLock { _file: f }),
                Err(std::fs::TryLockError::WouldBlock) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(PrefixError::Busy(self.dir.clone()));
                    }
                    std::thread::sleep(pause);
                    pause = (pause * 2).min(std::time::Duration::from_millis(20));
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
            }
        }
    }

    /// `<prefix>/drive_c`, the root of the Windows-visible filesystem.
    pub fn drive_c(&self) -> PathBuf {
        self.dir.join("drive_c")
    }

    /// Renders a host path under `drive_c` as the `C:\...` form Wine sees.
    /// Returns `None` for anything not under `drive_c` — such a path has no
    /// `C:` form and one must not be invented.
    pub fn windows_path(&self, host: &Path) -> Option<String> {
        let rel = host.strip_prefix(self.drive_c()).ok()?;
        let mut out = String::from("C:");
        for component in rel.components() {
            match component {
                std::path::Component::Normal(part) => {
                    out.push('\\');
                    out.push_str(&part.to_string_lossy());
                }
                _ => return None,
            }
        }
        Some(out)
    }
}

#[cfg(test)]
fn scratch(tag: &str) -> std::path::PathBuf {
    let d = crate::test_tmp::dir().join(format!("vfs-prefix-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_path_maps_only_paths_under_drive_c() {
        let p = Prefix { dir: scratch("wp") };
        std::fs::create_dir_all(p.drive_c().join("Games")).unwrap();
        let inside = p.drive_c().join("Games").join("g.exe");
        assert_eq!(
            p.windows_path(&inside).as_deref(),
            Some(r"C:\Games\g.exe"),
            "a path under drive_c must render as a C: path with backslashes"
        );
        assert_eq!(
            p.windows_path(std::path::Path::new("/etc/passwd")),
            None,
            "a path outside the prefix has no C: form and must not be invented"
        );
    }

    /// A lock dropped while another thread of this process forks must not look
    /// held: the forked child briefly owns a copy of the descriptor.
    #[test]
    fn a_lock_released_while_another_thread_forks_is_not_busy() {
        let p = Prefix {
            dir: scratch("lock-fork"),
        };
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let forker = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = std::process::Command::new("true").status();
                }
            })
        };
        for i in 0..300 {
            let l = p.lock().unwrap_or_else(|e| panic!("lock {i}: {e}"));
            drop(l);
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        forker.join().unwrap();
    }

    #[test]
    fn a_second_lock_is_busy_and_the_lock_is_released_on_drop() {
        let p = Prefix {
            dir: scratch("lock"),
        };
        let held = p.lock().unwrap();
        assert!(matches!(p.lock(), Err(PrefixError::Busy(_))));
        drop(held);
        // `lock` itself rides out a sibling thread's half-forked child.
        p.lock().expect("the lock must be released on drop");
    }

    #[test]
    fn remove_session_deletes_only_that_session() {
        let base = scratch("rm");
        let root = Root::at(base.clone());
        let a = root.try_session_dir("a").unwrap();
        let b = root.try_session_dir("b").unwrap();
        std::fs::create_dir_all(a.join("prefix")).unwrap();
        std::fs::create_dir_all(b.join("prefix")).unwrap();
        remove_session(&root, "a").unwrap();
        assert!(!a.exists() && b.exists());
        remove_session(&root, "a").expect("removing an absent session is not an error");
    }
}

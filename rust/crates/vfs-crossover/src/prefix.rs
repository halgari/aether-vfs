//! Session prefixes as CrossOver bottles.
//!
//! [`vfs_proton::Prefix`] is reused wholesale rather than mirrored: `drive_c`,
//! `windows_path`, `map_drive` and `unmap_drive` are pure path logic over a
//! Wine prefix layout, and a CrossOver bottle *is* a Wine prefix — `drive_c`,
//! `dosdevices`, `system.reg`, plus a `cxbottle.conf` that only CrossOver
//! reads. Only creation differs, so only creation lives here.

use std::io;
use std::path::Path;
use std::process::Command;

use vfs_proton::Prefix;

use crate::runtime::Runtime;

/// Why a session prefix could not be created.
#[derive(Debug)]
pub enum PrefixError {
    Io(io::Error),
    /// `cxbottle --create` ran and failed. Carries its combined output.
    Cxbottle(String),
    /// `cxbottle` could not be started at all.
    Spawn(String),
}

impl std::fmt::Display for PrefixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrefixError::Io(e) => write!(f, "io error: {e}"),
            PrefixError::Cxbottle(s) => write!(f, "cxbottle --create failed: {s}"),
            PrefixError::Spawn(s) => write!(f, "could not run cxbottle: {s}"),
        }
    }
}

impl std::error::Error for PrefixError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PrefixError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for PrefixError {
    fn from(e: io::Error) -> Self {
        PrefixError::Io(e)
    }
}

/// The bottle template new session prefixes are created from.
///
/// `win10_64` rather than `win11_64`: the games this hosts are Windows-10-era
/// titles, and a Windows 11 prefix reports a build number that some launchers
/// and script extenders check. Nothing here needs 11.
pub const TEMPLATE: &str = "win10_64";

/// Creates `dir` as a CrossOver bottle if it is not one already.
///
/// Idempotent on the same test [`vfs_proton::prefix::ensure`] uses —
/// `drive_c/windows/system32` exists — because creating a bottle takes about
/// twelve seconds and a session relaunches into the prefix it already booted.
///
/// # Why the path is passed as the bottle name
///
/// `cxbottle --bottle` normally names a bottle inside CrossOver's own bottle
/// directories, but `CXBottle::find_bottle` returns the name unchanged when it
/// starts with `/`. So an absolute path is a private bottle, and a session
/// prefix stays where the host put it — under the session's own state
/// directory — instead of being installed into the user's CrossOver library
/// where it would show up as a bottle they did not make.
pub fn ensure(runtime: &Runtime, dir: &Path) -> Result<Prefix, PrefixError> {
    if is_initialised(dir) {
        return Ok(Prefix {
            dir: dir.to_path_buf(),
        });
    }
    // Absolute, because that is the *whole* mechanism above: a relative path
    // is looked up as an ordinary bottle name and would either miss or, worse,
    // hit an unrelated bottle of the same name.
    let dir = absolute(dir)?;
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let out = Command::new(runtime.dir.join("bin").join("cxbottle"))
        .arg("--bottle")
        .arg(&dir)
        .arg("--create")
        .arg("--template")
        .arg(TEMPLATE)
        .arg("--description")
        .arg("aether-vfs session")
        // `cxbottle` resolves its own libraries from here, and inherits
        // nothing useful when spawned from a daemon.
        .env("CX_ROOT", &runtime.dir)
        .output()
        .map_err(|e| PrefixError::Spawn(format!("{}: {e}", runtime.dir.display())))?;

    if !is_initialised(&dir) {
        // The exit status is not the test. `cxbottle` shells out to
        // `rundll32` and friends during creation and has been observed
        // exiting 0 with cosmetic noise on stderr; what matters is whether a
        // usable prefix exists afterwards.
        return Err(PrefixError::Cxbottle(format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(Prefix { dir })
}

fn is_initialised(dir: &Path) -> bool {
    dir.join("drive_c")
        .join("windows")
        .join("system32")
        .is_dir()
}

fn absolute(p: &Path) -> Result<std::path::PathBuf, io::Error> {
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    Ok(std::env::current_dir()?.join(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("vfs-cxpfx-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The idempotence check, without CrossOver: a directory that already
    /// looks booted is returned as-is, and nothing is spawned. This is what
    /// keeps a relaunch from paying twelve seconds, so it is worth pinning
    /// even though the creating half needs a real installation.
    #[test]
    fn an_initialised_prefix_is_returned_without_running_cxbottle() {
        let dir = scratch("already");
        std::fs::create_dir_all(dir.join("drive_c/windows/system32")).unwrap();
        // A runtime that could not possibly run: if `ensure` tried to spawn
        // anything, this fails.
        let rt = Runtime {
            dir: std::path::PathBuf::from("/nonexistent"),
            version: None,
        };
        let p = ensure(&rt, &dir).expect("an initialised prefix needs no creation");
        assert_eq!(p.dir, dir);
    }

    #[test]
    fn a_bare_directory_is_not_mistaken_for_a_prefix() {
        let dir = scratch("bare");
        assert!(!is_initialised(&dir));
        std::fs::create_dir_all(dir.join("drive_c")).unwrap();
        assert!(
            !is_initialised(&dir),
            "drive_c alone is what a half-created bottle has; system32 is the marker"
        );
    }
}

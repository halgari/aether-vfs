//! Per-session Wine prefixes: creation, drive-letter rerouting, and dropping
//! `dosdevices/z:` for containment.
//!
//! A fresh Wine prefix maps `dosdevices/z: -> /`, putting the entire host
//! filesystem inside the game's namespace. `unmap_drive('z', ..)` is how this
//! crate removes that: containment Windows does not have, and worth keeping
//! as a first-class, tested operation rather than an incidental side effect.

use std::io;
use std::path::{Path, PathBuf};

use crate::layout::Root;
use crate::runtime::verify_ge;

/// A session's private Wine prefix: the directory `WINEPREFIX` points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prefix {
    pub dir: PathBuf,
}

/// Why [`ensure`], [`Prefix::map_drive`], or [`Prefix::unmap_drive`] failed.
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
}

impl std::fmt::Display for PrefixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrefixError::Io(e) => write!(f, "io error: {e}"),
            PrefixError::Wineboot(s) => write!(f, "wineboot failed: {s}"),
            PrefixError::NotGe(s) => write!(f, "runtime is not GE-Proton: {s}"),
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
            PrefixError::Wineboot(_) | PrefixError::NotGe(_) | PrefixError::Missing32Bit => None,
        }
    }
}

impl From<io::Error> for PrefixError {
    fn from(e: io::Error) -> Self {
        PrefixError::Io(e)
    }
}

/// Creates (if needed) `root.sessions()/<session>/prefix` as a Wine prefix
/// for `session`, verifying `runtime` is GE-Proton first.
///
/// Idempotent: if `drive_c/windows/system32` already exists, the prefix is
/// treated as initialised and `wineboot` is not run again.
pub fn ensure(root: &Root, runtime: &Path, session: &str) -> Result<Prefix, PrefixError> {
    let session_dir = root.try_session_dir(session).map_err(|e| {
        PrefixError::Io(io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))
    })?;
    let dir = session_dir.join("prefix");

    if !is_initialised(&dir) {
        // Verify GE *before* touching anything else on disk: `PROTONPATH`
        // defaults to stock Proton, and refusing here is what stops a
        // silent downgrade rather than a partially-created prefix.
        verify_ge(runtime).map_err(|e| PrefixError::NotGe(e.to_string()))?;
        std::fs::create_dir_all(&dir)?;
        run_wineboot(runtime, &dir)?;
    }

    Ok(Prefix { dir })
}

fn is_initialised(prefix_dir: &Path) -> bool {
    prefix_dir
        .join("drive_c")
        .join("windows")
        .join("system32")
        .is_dir()
}

fn run_wineboot(runtime: &Path, prefix_dir: &Path) -> Result<(), PrefixError> {
    let wine = runtime.join("files").join("bin").join("wine");
    let output = std::process::Command::new(&wine)
        .arg("wineboot")
        .arg("-u")
        .env("WINEPREFIX", prefix_dir)
        .env("WINEDLLOVERRIDES", "mscoree=d;mshtml=d")
        .env("WINEDEBUG", "-all")
        .output()?;

    if output.status.success() {
        return Ok(());
    }

    // FreeType warnings are cosmetic for console targets and show up on
    // stderr even on success; only a non-zero exit gets here at all, so no
    // extra filtering of the "good" case is needed.
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if combined.contains("ld-linux.so.2") {
        return Err(PrefixError::Missing32Bit);
    }
    Err(PrefixError::Wineboot(combined))
}

impl Prefix {
    /// `<prefix>/drive_c`, the root of the Windows-visible filesystem.
    pub fn drive_c(&self) -> PathBuf {
        self.dir.join("drive_c")
    }

    /// Points `dosdevices/<letter>:` at `target`, replacing any existing
    /// mapping (a session reuses a prefix across launches, so remapping must
    /// not fail just because a link is already there).
    pub fn map_drive(&self, letter: char, target: &Path) -> Result<(), PrefixError> {
        let link = self.dosdevices_link(letter);
        remove_link_if_present(&link)?;

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, &link)?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = target;
            Err(PrefixError::Io(io::Error::new(
                io::ErrorKind::Unsupported,
                "dosdevices drive mapping is a unix (Wine) concept only",
            )))
        }
    }

    /// Removes `dosdevices/<letter>:`. In particular, removing `z:` — which
    /// a fresh prefix maps to `/`, the whole host filesystem — is how this
    /// crate achieves containment; it is a supported, intentional case, not
    /// an edge case.
    pub fn unmap_drive(&self, letter: char) -> Result<(), PrefixError> {
        remove_link_if_present(&self.dosdevices_link(letter))
    }

    fn dosdevices_link(&self, letter: char) -> PathBuf {
        self.dir.join("dosdevices").join(format!("{letter}:"))
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

fn remove_link_if_present(link: &Path) -> Result<(), PrefixError> {
    match std::fs::remove_file(link) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(PrefixError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("vfs-prefix-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

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

    #[cfg(unix)]
    #[test]
    fn map_drive_points_dosdevices_at_the_target_and_unmap_removes_it() {
        let p = Prefix { dir: scratch("drv") };
        std::fs::create_dir_all(p.dir.join("dosdevices")).unwrap();
        let target = scratch("drv-target");
        p.map_drive('d', &target).unwrap();
        let link = p.dir.join("dosdevices").join("d:");
        assert_eq!(std::fs::read_link(&link).unwrap(), target);
        // Remapping must replace, not fail: a session reuses a prefix.
        let target2 = scratch("drv-target2");
        p.map_drive('d', &target2).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), target2);
        p.unmap_drive('d').unwrap();
        assert!(!link.exists(), "unmap must remove the link");
    }

    #[cfg(unix)]
    #[test]
    fn unmapping_z_is_how_containment_is_achieved() {
        // `dosdevices/z: -> /` maps the whole host filesystem into the game's
        // namespace. Removing it gives containment Windows does not have, so
        // this is a feature and needs to keep working.
        let p = Prefix { dir: scratch("z") };
        let dd = p.dir.join("dosdevices");
        std::fs::create_dir_all(&dd).unwrap();
        std::os::unix::fs::symlink("/", dd.join("z:")).unwrap();
        p.unmap_drive('z').unwrap();
        assert!(!dd.join("z:").exists());
    }
}

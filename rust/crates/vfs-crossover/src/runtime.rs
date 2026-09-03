//! Finding CrossOver, and refusing an installation that cannot host the shim.
//!
//! The counterpart of [`vfs_proton::runtime`], with the polarity reversed.
//! There, every runtime must be *proved* to be GE-Proton, because the default
//! when the check is skipped is stock Proton and the downgrade is silent.
//! Here there is nothing to downgrade to — CrossOver is installed or it is
//! not — so [`verify`] checks only that the tree has the pieces a launch will
//! reach, and says which one is missing when it does not.

use std::io;
use std::path::{Path, PathBuf};

/// A CrossOver installation: the `SharedSupport/CrossOver` directory inside
/// the app bundle, which is what holds `bin/` and `lib/`.
///
/// Not the `.app` itself. Every path this crate builds hangs off this
/// directory, and naming the bundle would mean re-appending the same two
/// components at each use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runtime {
    pub dir: PathBuf,
    /// `ProductVersion` from the bundle's `Info.plist`, when it could be read.
    /// Informational only — nothing branches on it. It exists so a failure
    /// report can say *which* CrossOver behaved unexpectedly, which is the
    /// first question when a launch works on one machine and not another.
    pub version: Option<String>,
}

/// Why a directory is not a usable CrossOver installation.
#[derive(Debug)]
pub enum VerifyError {
    /// Nothing at the path at all.
    Missing(PathBuf),
    /// A required component is absent. Carries the path that was looked for,
    /// because "CrossOver is broken" is not an actionable message and
    /// "`…/lib/wine/x86_64-windows` is missing" is.
    Incomplete(PathBuf),
    /// The filesystem refused to answer.
    Io(io::Error),
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::Missing(p) => {
                write!(f, "no CrossOver installation at {}", p.display())
            }
            VerifyError::Incomplete(p) => write!(
                f,
                "CrossOver is installed but {} is missing, so it cannot host a Windows process",
                p.display()
            ),
            VerifyError::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for VerifyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            VerifyError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for VerifyError {
    fn from(e: io::Error) -> Self {
        VerifyError::Io(e)
    }
}

/// Where CrossOver installs by default, and the only place [`installed`]
/// looks before consulting the environment.
pub const DEFAULT_APP: &str = "/Applications/CrossOver.app";

/// The path *inside* the app bundle that everything else hangs off.
const SHARED_SUPPORT: &str = "Contents/SharedSupport/CrossOver";

/// Environment override, for an installation somewhere other than
/// `/Applications`. Names the **app bundle**, not the shared-support
/// directory, because that is the path a person can see in Finder.
pub const APP_ENV: &str = "VFS_CROSSOVER_APP";

/// The installed CrossOver, or why there isn't one.
///
/// `$VFS_CROSSOVER_APP` first, then [`DEFAULT_APP`]. Two locations rather
/// than a search: CrossOver is a single licensed application that installs to
/// one place, and scanning the filesystem for a second copy would make which
/// one runs a game depend on directory order.
pub fn installed() -> Result<Runtime, VerifyError> {
    let app = match std::env::var_os(APP_ENV) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(DEFAULT_APP),
    };
    verify(&app)
}

/// Checks that `app` is a CrossOver bundle with the pieces a launch reaches.
///
/// Three things are required, and each is the direct cause of a distinct
/// failure if absent:
///
/// - `bin/wine` — the Perl wrapper this crate execs. Without it there is
///   nothing to run.
/// - `bin/cxbottle` — how [`crate::prefix::ensure`] creates a session prefix.
///   A launch into a prefix that was never created fails much later and much
///   less clearly.
/// - `lib/wine/x86_64-windows` — the PE-format builtins (Wine's own
///   `ntdll.dll` among them). This is the tree the shim's detours are
///   installed *into*, so an installation without it can start a process and
///   never virtualise a single read.
pub fn verify(app: &Path) -> Result<Runtime, VerifyError> {
    if !app.exists() {
        return Err(VerifyError::Missing(app.to_path_buf()));
    }
    // Tolerate being handed either the bundle or the shared-support directory
    // inside it: both are things a person reasonably calls "where CrossOver
    // is", and guessing wrong costs a confusing "missing" error.
    let dir = if app.join(SHARED_SUPPORT).is_dir() {
        app.join(SHARED_SUPPORT)
    } else {
        app.to_path_buf()
    };

    for required in [
        dir.join("bin").join("wine"),
        dir.join("bin").join("cxbottle"),
        dir.join("lib").join("wine").join("x86_64-windows"),
    ] {
        if !required.exists() {
            return Err(VerifyError::Incomplete(required));
        }
    }

    Ok(Runtime {
        version: read_version(app),
        dir,
    })
}

/// `ProductVersion` out of the bundle's `Info.plist`.
///
/// Parsed by scanning rather than with a plist library: this value is only
/// ever shown to a human in an error, so a dependency to read it would cost
/// more than the value is worth, and `None` is an acceptable answer.
fn read_version(app: &Path) -> Option<String> {
    let text = std::fs::read_to_string(app.join("Contents").join("Info.plist")).ok()?;
    let after_key = text.split("<key>CFBundleShortVersionString</key>").nth(1)?;
    let open = after_key.find("<string>")? + "<string>".len();
    let close = after_key[open..].find("</string>")? + open;
    Some(after_key[open..close].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vfs-cx-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Builds a directory that looks like a CrossOver bundle, minus whatever
    /// `omit` names.
    fn fake_bundle(at: &Path, omit: Option<&str>) {
        let ss = at.join(SHARED_SUPPORT);
        for rel in ["bin/wine", "bin/cxbottle"] {
            if Some(rel) == omit {
                continue;
            }
            let p = ss.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"#!/usr/bin/perl\n").unwrap();
        }
        if omit != Some("lib/wine/x86_64-windows") {
            std::fs::create_dir_all(ss.join("lib/wine/x86_64-windows")).unwrap();
        }
        std::fs::create_dir_all(at.join("Contents")).unwrap();
        std::fs::write(
            at.join("Contents").join("Info.plist"),
            "<plist><dict><key>CFBundleShortVersionString</key><string>26.3.0</string></dict></plist>",
        )
        .unwrap();
    }

    #[test]
    fn a_complete_bundle_verifies_and_reports_its_version() {
        let app = scratch("ok");
        fake_bundle(&app, None);
        let rt = verify(&app).expect("a complete bundle verifies");
        assert_eq!(rt.dir, app.join(SHARED_SUPPORT));
        assert_eq!(rt.version.as_deref(), Some("26.3.0"));
    }

    #[test]
    fn the_shared_support_directory_is_accepted_directly() {
        // Being handed the inner directory is not an error: it is the other
        // thing a person means by "where CrossOver is".
        let app = scratch("inner");
        fake_bundle(&app, None);
        let rt = verify(&app.join(SHARED_SUPPORT)).expect("the inner directory verifies");
        assert_eq!(rt.dir, app.join(SHARED_SUPPORT));
    }

    #[test]
    fn a_missing_path_is_missing_not_incomplete() {
        let app = scratch("gone").join("nope");
        match verify(&app) {
            Err(VerifyError::Missing(p)) => assert_eq!(p, app),
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    /// The one that matters. An installation can be present, runnable, and
    /// still unable to host the shim, because the detours are installed into
    /// the **PE** ntdll under `lib/wine/x86_64-windows`. Without that tree a
    /// launch starts a process and virtualises nothing — which reads exactly
    /// like a mod list that is simply empty.
    #[test]
    fn a_bundle_without_the_pe_builtins_is_refused_by_name() {
        let app = scratch("nope-pe");
        fake_bundle(&app, Some("lib/wine/x86_64-windows"));
        match verify(&app) {
            Err(VerifyError::Incomplete(p)) => {
                assert!(
                    p.ends_with("lib/wine/x86_64-windows"),
                    "the error must name the missing component, got {}",
                    p.display()
                );
            }
            other => panic!("expected Incomplete, got {other:?}"),
        }
    }

    #[test]
    fn a_bundle_without_cxbottle_is_refused_before_a_prefix_is_attempted() {
        let app = scratch("nope-bottle");
        fake_bundle(&app, Some("bin/cxbottle"));
        assert!(matches!(verify(&app), Err(VerifyError::Incomplete(_))));
    }
}

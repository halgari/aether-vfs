//! CrossOver as the Wine host on macOS.
//!
//! The Wine half of a launch is the same on both hosts — the injector, the
//! shim, the file-backed ring, the prefix and its root links — so this crate
//! keeps it, and CrossOver differs only where its install does
//! ([`crate::host::WineHost`]):
//!
//! - **Installed by the user.** There is nothing to download and no GE gate:
//!   a CrossOver bundle is verified by its parts ([`verify`]), and its version
//!   is informational.
//! - **`bin/wine` is a Perl front end**, not Wine's loader. It resolves a
//!   *bottle* before anything else and overwrites `WINEPREFIX` while doing
//!   so, so the prefix is chosen with `--bottle <absolute path>` — an
//!   absolute path is taken as the bottle itself, wherever it is. It needs
//!   `CX_ROOT` to find its own Perl modules.
//! - **A prefix is a bottle**, created with `cxbottle --create` ([`create_bottle`])
//!   rather than `wineboot`. It still lives under `$VFS_HOME/sessions`, never
//!   in CrossOver's own bottle list.
//! - **`bin/wineserver` is Wine's own** and is driven exactly as Proton's is,
//!   with `WINEPREFIX`.
//!
//! CrossOver's Unix side is x86_64, so on Apple Silicon Wine runs under
//! Rosetta while the Director is native; the ring is a file both map, which
//! is coherent across the two.

use std::io;
use std::path::{Path, PathBuf};

use crate::process::{own_process_group, run_bounded_status};

/// Where CrossOver is installed when nothing says otherwise.
pub const DEFAULT_APPS: [&str; 1] = ["/Applications/CrossOver.app"];

/// Names a CrossOver bundle (`…/CrossOver.app`) or its
/// `Contents/SharedSupport/CrossOver` directory, overriding the search.
pub const APP_ENV: &str = vfs_env::CROSSOVER_APP;

/// The directory inside the bundle that holds `bin/` and `lib/`.
const SHARED_SUPPORT: &str = "Contents/SharedSupport/CrossOver";

/// The template new bottles are made from. Windows 10, because launchers and
/// script extenders check the build number.
pub const TEMPLATE: &str = "win10_64";

/// How long creating a bottle may take before it is killed. A first one
/// takes about 15 s.
pub const CREATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Why a directory is not a usable CrossOver install.
#[derive(Debug)]
pub enum VerifyError {
    /// Nothing at that path.
    Missing(PathBuf),
    /// A required part of the install is not there.
    Incomplete(PathBuf),
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::Missing(p) => write!(f, "no CrossOver at {}", p.display()),
            VerifyError::Incomplete(p) => {
                write!(
                    f,
                    "incomplete CrossOver install: {} is missing",
                    p.display()
                )
            }
        }
    }
}

impl std::error::Error for VerifyError {}

/// Resolves `app` — a bundle or its `Contents/SharedSupport/CrossOver` — to
/// the directory holding `bin/wine`, and checks the parts a launch uses are
/// there.
pub fn verify(app: &Path) -> Result<PathBuf, VerifyError> {
    if !app.exists() {
        return Err(VerifyError::Missing(app.to_path_buf()));
    }
    let inner = app.join(SHARED_SUPPORT);
    let dir = if inner.is_dir() {
        inner
    } else {
        app.to_path_buf()
    };
    for part in [
        "bin/wine",
        "bin/wineserver",
        "bin/cxbottle",
        "lib/wine/x86_64-windows",
    ] {
        let p = dir.join(part);
        if !p.exists() {
            return Err(VerifyError::Incomplete(p));
        }
    }
    Ok(dir)
}

/// The CrossOver install a launch uses: [`APP_ENV`] when set (and then only
/// that one, so a wrong value is an error rather than a silent fallback),
/// else the first of [`DEFAULT_APPS`] and `~/Applications/CrossOver.app` that
/// verifies.
pub fn installed() -> Result<PathBuf, VerifyError> {
    if let Some(app) = std::env::var_os(APP_ENV).filter(|v| !v.is_empty()) {
        return verify(Path::new(&app));
    }
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join("Applications/CrossOver.app"));
    let mut first_err = None;
    for app in DEFAULT_APPS.iter().map(PathBuf::from).chain(home) {
        match verify(&app) {
            Ok(dir) => return Ok(dir),
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
    }
    Err(first_err.expect("DEFAULT_APPS is not empty"))
}

/// CrossOver's version (`26.3`), read from the bundle's `Info.plist`, when
/// `dir` is inside a bundle. Informational: nothing is gated on it.
pub fn version(dir: &Path) -> Option<String> {
    // `dir` is `<bundle>/Contents/SharedSupport/CrossOver`.
    let plist = dir.parent()?.parent()?.join("Info.plist");
    let text = std::fs::read_to_string(plist).ok()?;
    let after = text.split("<key>CFBundleShortVersionString</key>").nth(1)?;
    let start = after.find("<string>")? + "<string>".len();
    let end = after[start..].find("</string>")? + start;
    Some(after[start..end].trim().to_string()).filter(|v| !v.is_empty())
}

/// `wine` for the install at `dir`: CrossOver's front end.
pub fn wine(dir: &Path) -> PathBuf {
    dir.join("bin").join("wine")
}

/// `wineserver` for the install at `dir`.
pub fn wineserver(dir: &Path) -> PathBuf {
    dir.join("bin").join("wineserver")
}

/// The arguments that put a `wine` run in the bottle at `prefix`, before the
/// program. `prefix` is made absolute: a relative one would be taken as the
/// name of a bottle in CrossOver's own list.
///
/// `--no-convert`: the front end otherwise rewrites every argument that names
/// an existing native file into a Windows path — the program's arguments
/// included. Proton hands them over as given, and so does this; Wine resolves
/// the injector's own host-path arguments through `Z:` either way.
pub fn bottle_args(prefix: &Path) -> Vec<String> {
    let abs = std::path::absolute(prefix).unwrap_or_else(|_| prefix.to_path_buf());
    vec![
        "--bottle".into(),
        abs.to_string_lossy().into_owned(),
        "--no-convert".into(),
    ]
}

/// Creates a bottle at `prefix` (absolute or not) from [`TEMPLATE`], with the
/// install at `dir`, its output to `log`. `cxbottle`'s exit status is not the
/// judge: the bottle exists when its `system32` does, which the caller checks.
///
/// `cxbottle` refuses a directory that exists, and `prefix` may: it holds the
/// prefix's lock file, taken before the prefix is set up. So the bottle is
/// made beside it (`<prefix>.creating`) and its entries moved in. Nothing in
/// a bottle names its own path — `dosdevices/c:` is `../drive_c` — so it
/// works from where it lands.
pub fn create_bottle(
    dir: &Path,
    prefix: &Path,
    log: &Path,
) -> io::Result<Option<std::process::ExitStatus>> {
    let abs = std::path::absolute(prefix)?;
    let mut staging = abs.clone().into_os_string();
    staging.push(".creating");
    let staging = PathBuf::from(staging);
    match std::fs::remove_dir_all(&staging) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let status = run_cxbottle(dir, &staging, log)?;
    if staging.is_dir() {
        std::fs::create_dir_all(&abs)?;
        for entry in std::fs::read_dir(&staging)? {
            let entry = entry?;
            std::fs::rename(entry.path(), abs.join(entry.file_name()))?;
        }
        std::fs::remove_dir(&staging)?;
    }
    Ok(status)
}

/// `cxbottle --create` into `bottle`, which must not exist.
fn run_cxbottle(
    dir: &Path,
    bottle: &Path,
    log: &Path,
) -> io::Result<Option<std::process::ExitStatus>> {
    let out = std::fs::File::create(log)?;
    let mut cmd = std::process::Command::new(dir.join("bin").join("cxbottle"));
    cmd.arg("--bottle")
        .arg(bottle)
        .arg("--create")
        .arg("--template")
        .arg(TEMPLATE)
        .arg("--description")
        .arg("aether-vfs session")
        .env("CX_ROOT", dir)
        .stdin(std::process::Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out);
    own_process_group(&mut cmd);
    run_bounded_status(&mut cmd, CREATE_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = crate::test_tmp::dir().join(format!("vfs-cx-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn fake_bundle(tag: &str, version: &str) -> PathBuf {
        let app = tmpdir(tag).join("CrossOver.app");
        let dir = app.join(SHARED_SUPPORT);
        for f in ["bin/wine", "bin/wineserver", "bin/cxbottle"] {
            std::fs::create_dir_all(dir.join(f).parent().unwrap()).unwrap();
            std::fs::write(dir.join(f), "").unwrap();
        }
        std::fs::create_dir_all(dir.join("lib/wine/x86_64-windows")).unwrap();
        std::fs::write(
            app.join("Contents/Info.plist"),
            format!(
                "<dict>\n\t<key>CFBundleName</key>\n\t<string>CrossOver</string>\n\
                 \t<key>CFBundleShortVersionString</key>\n\t<string>{version}</string>\n</dict>"
            ),
        )
        .unwrap();
        app
    }

    #[test]
    fn a_bundle_or_its_inner_directory_verifies_to_the_inner_directory() {
        let app = fake_bundle("ok", "26.3");
        let inner = app.join(SHARED_SUPPORT);
        assert_eq!(verify(&app).unwrap(), inner);
        assert_eq!(verify(&inner).unwrap(), inner);
        assert_eq!(version(&inner).as_deref(), Some("26.3"));
    }

    #[test]
    fn a_missing_part_is_named() {
        let app = fake_bundle("incomplete", "26.3");
        let gone = app.join(SHARED_SUPPORT).join("lib/wine/x86_64-windows");
        std::fs::remove_dir(&gone).unwrap();
        match verify(&app) {
            Err(VerifyError::Incomplete(p)) => assert_eq!(p, gone),
            other => panic!("{other:?}"),
        }
        let nowhere = app.join("nope");
        assert!(matches!(verify(&nowhere), Err(VerifyError::Missing(p)) if p == nowhere));
    }

    #[test]
    fn the_bottle_is_named_by_an_absolute_path() {
        let args = bottle_args(Path::new("rel/prefix"));
        assert_eq!(args[0], "--bottle");
        assert!(Path::new(&args[1]).is_absolute(), "{args:?}");
        assert!(Path::new(&args[1]).ends_with("rel/prefix"), "{args:?}");
        assert_eq!(
            args[2], "--no-convert",
            "arguments reach the program as given"
        );
    }
}

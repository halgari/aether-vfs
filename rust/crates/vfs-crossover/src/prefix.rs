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
    /// The Visual C++ runtime could not be fetched. Carries the reason.
    Fetch(String),
    /// The redistributable ran and did not leave a usable runtime behind.
    Runtime(String),
}

impl std::fmt::Display for PrefixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrefixError::Io(e) => write!(f, "io error: {e}"),
            PrefixError::Cxbottle(s) => write!(f, "cxbottle --create failed: {s}"),
            PrefixError::Spawn(s) => write!(f, "could not run cxbottle: {s}"),
            PrefixError::Fetch(s) => write!(f, "could not fetch the Visual C++ runtime: {s}"),
            PrefixError::Runtime(s) => write!(f, "the Visual C++ runtime is not usable: {s}"),
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
        let prefix = Prefix {
            dir: dir.to_path_buf(),
        };
        // Not only on creation: a prefix booted before this existed is exactly
        // the one that crashes, and it cannot fix itself.
        provision(runtime, &prefix)?;
        return Ok(prefix);
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
    let prefix = Prefix { dir };
    provision(runtime, &prefix)?;
    Ok(prefix)
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

// ---------------------------------------------------------------------------
// Provisioning: what a bottle needs beyond being a bottle
// ---------------------------------------------------------------------------

/// Marker naming what a prefix has already been given, so provisioning runs
/// once. Bumping the version re-provisions every prefix.
const PROVISIONED: &str = ".aether-vfs-provisioned";
const PROVISION_VERSION: &str = "1";

/// Everything a fresh bottle needs before a modded game will run in it.
///
/// # Why a bottle is not enough on its own
///
/// `cxbottle --create` gives a Windows that boots. It does not give one a
/// heavily modded Skyrim survives in, and the gap is not gradual — it is a
/// hard crash a couple of minutes into gameplay:
///
/// ```text
/// Unhandled exception ... kernelbase.dll
///   Parameter[0]: "concrt140.dll"
///   Parameter[1]: "??0_TaskCollection@details@Concurrency@@QEAA@XZ"
/// ```
///
/// A non-continuable exception whose two parameters are a DLL and a function
/// is Wine's **unimplemented stub** signal: the symbol is exported so loading
/// and linking succeed, and calling it raises. Measured 2026-09-03 — an SKSE
/// plugin using the Parallel Patterns Library called
/// `Concurrency::details::_TaskCollection::_TaskCollection()`, and CrossOver's
/// 144 KB `concrt140.dll` is a stub where Microsoft's is 324 KB.
///
/// So the runtime is installed from Microsoft's own redistributable and the
/// DLLs are overridden to `native` — Wine prefers its builtin for a name it
/// knows even with the real file sitting beside it, which would otherwise
/// leave the stub in charge of a prefix that now contains the real thing.
/// This is what `protontricks vcrun2022` does for the same games on Linux.
///
/// Idempotent, and it has to be: `ensure` runs on every launch.
pub fn provision(runtime: &Runtime, prefix: &Prefix) -> Result<(), PrefixError> {
    let marker = prefix.dir.join(PROVISIONED);
    if std::fs::read_to_string(&marker).is_ok_and(|v| v.trim() == PROVISION_VERSION) {
        return Ok(());
    }

    install_vc_runtime(runtime, prefix)?;
    set_dll_overrides(runtime, prefix)?;

    std::fs::write(&marker, PROVISION_VERSION)?;
    Ok(())
}

/// The runtime DLLs Wine must be told to prefer from disk.
///
/// `concrt140` is the one measured to crash a game; the rest of the 2015-2022
/// runtime is listed with it because they are one redistributable at one
/// version, and a prefix running Microsoft's `concrt140` against Wine's
/// `msvcp140` is a mixture nobody tests.
pub const NATIVE_DLLS: &[&str] = &[
    "concrt140",
    "msvcp140",
    "msvcp140_1",
    "msvcp140_2",
    "vcruntime140",
    "vcruntime140_1",
];

/// Point the loader at the DLLs on disk rather than Wine's built-ins.
///
/// Written into the prefix's own registry rather than passed as
/// `WINEDLLOVERRIDES`: the environment form has to be repeated by every launch
/// and is lost the moment something starts the game another way. Wine merges
/// the two per module, so a launch setting its own overrides for other DLLs
/// does not disturb these.
pub fn set_dll_overrides(runtime: &Runtime, prefix: &Prefix) -> Result<(), PrefixError> {
    for dll in NATIVE_DLLS {
        let out = Command::new(wine(runtime))
            .arg("--bottle")
            .arg(&prefix.dir)
            .args(["reg", "add", r"HKCU\Software\Wine\DllOverrides", "/v"])
            .arg(dll)
            .args(["/d", "native,builtin", "/f"])
            .env("CX_ROOT", &runtime.dir)
            .output()
            .map_err(|e| PrefixError::Spawn(format!("wine reg: {e}")))?;
        if !out.status.success() {
            return Err(PrefixError::Runtime(format!(
                "could not set the {dll} override: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
    }
    Ok(())
}

fn wine(runtime: &Runtime) -> std::path::PathBuf {
    runtime.dir.join("bin").join("wine")
}

/// Where Microsoft publishes the current x64 redistributable. A permalink that
/// redirects to whatever the latest 2015-2022 build is, which is what we want:
/// the exact build does not matter, only that it is Microsoft's rather than a
/// stub.
pub const VC_REDIST_URL: &str = "https://aka.ms/vs/17/release/vc_redist.x64.exe";

/// `concrt140.dll` as CrossOver ships it, for telling a stub from the real
/// thing after an install.
fn builtin_concrt(runtime: &Runtime) -> std::path::PathBuf {
    runtime.dir.join("lib").join("wine").join("x86_64-windows").join("concrt140.dll")
}

fn installed_concrt(prefix: &Prefix) -> std::path::PathBuf {
    prefix.dir.join("drive_c").join("windows").join("system32").join("concrt140.dll")
}

/// Whether the prefix's `concrt140.dll` is still Wine's.
///
/// Compared by bytes against the runtime's own copy rather than by size or
/// version: a size is a guess and the stub carries a plausible version
/// resource. Missing on either side is "cannot tell", which is not "installed".
fn is_stub_runtime(runtime: &Runtime, prefix: &Prefix) -> Option<bool> {
    let a = std::fs::read(installed_concrt(prefix)).ok()?;
    let b = std::fs::read(builtin_concrt(runtime)).ok()?;
    Some(a == b)
}

/// Download Microsoft's redistributable and run it in `prefix`.
///
/// The installer is fetched to the prefix and deleted afterwards: it is 25 MB,
/// it is needed exactly once, and leaving a copy in every prefix would be the
/// kind of accumulation nobody goes looking for.
#[cfg(feature = "acquire")]
fn install_vc_runtime(runtime: &Runtime, prefix: &Prefix) -> Result<(), PrefixError> {
    // Already Microsoft's — an earlier provision, or a user who ran it by
    // hand. Either way there is nothing to do and 25 MB not to download.
    if is_stub_runtime(runtime, prefix) == Some(false) {
        return Ok(());
    }

    let installer = prefix.dir.join("drive_c").join("vc_redist.x64.exe");
    if let Some(parent) = installer.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut res = ureq::get(VC_REDIST_URL)
        .call()
        .map_err(|e| PrefixError::Fetch(format!("GET {VC_REDIST_URL}: {e}")))?;
    if !res.status().is_success() {
        return Err(PrefixError::Fetch(format!(
            "GET {VC_REDIST_URL}: {}",
            res.status()
        )));
    }
    let mut body = res.body_mut().as_reader();
    {
        let mut file = std::fs::File::create(&installer)?;
        std::io::copy(&mut body, &mut file).map_err(|e| {
            PrefixError::Fetch(format!("writing {}: {e}", installer.display()))
        })?;
    }

    let out = Command::new(wine(runtime))
        .arg("--bottle")
        .arg(&prefix.dir)
        .arg(r"C:\vc_redist.x64.exe")
        .args(["/quiet", "/norestart"])
        .env("CX_ROOT", &runtime.dir)
        .output()
        .map_err(|e| PrefixError::Spawn(format!("wine vc_redist: {e}")))?;
    let _ = std::fs::remove_file(&installer);

    // The exit status is not the test. What matters is whether the DLL in the
    // prefix stopped being Wine's — an installer that "succeeded" and left the
    // stub in place is the failure this whole function exists to prevent, and
    // it would otherwise be discovered as a crash two minutes into a game.
    match is_stub_runtime(runtime, prefix) {
        Some(false) => Ok(()),
        Some(true) => Err(PrefixError::Runtime(format!(
            "the redistributable ran ({}) and {} is still Wine's stub",
            out.status,
            installed_concrt(prefix).display()
        ))),
        None => Err(PrefixError::Runtime(format!(
            "cannot tell whether the runtime installed: {} is not readable",
            installed_concrt(prefix).display()
        ))),
    }
}

/// Without `acquire` there is nothing to install with, and a caller who turned
/// the feature off has taken that on. The overrides are still set, so a prefix
/// someone provisioned by hand works.
#[cfg(not(feature = "acquire"))]
fn install_vc_runtime(_runtime: &Runtime, _prefix: &Prefix) -> Result<(), PrefixError> {
    Ok(())
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
        // Marked as provisioned too, because `ensure` provisions as well as
        // creates: without this it would try to install the Visual C++ runtime
        // into a prefix whose runtime does not exist, and a unit test has no
        // business reaching the network to find that out.
        std::fs::write(dir.join(PROVISIONED), PROVISION_VERSION).unwrap();
        // A runtime that could not possibly run: if `ensure` tried to spawn
        // anything, this fails.
        let rt = Runtime {
            dir: std::path::PathBuf::from("/nonexistent"),
            version: None,
        };
        let p = ensure(&rt, &dir).expect("an initialised prefix needs no creation");
        assert_eq!(p.dir, dir);
    }

    /// Provisioning is skipped only for the exact version marker, so bumping
    /// `PROVISION_VERSION` re-provisions every prefix that already exists.
    #[test]
    fn only_the_current_marker_counts_as_provisioned() {
        let dir = scratch("marker");
        std::fs::create_dir_all(dir.join("drive_c/windows/system32")).unwrap();
        let rt = Runtime {
            dir: std::path::PathBuf::from("/nonexistent"),
            version: None,
        };
        let prefix = Prefix { dir: dir.clone() };

        std::fs::write(dir.join(PROVISIONED), PROVISION_VERSION).unwrap();
        assert!(
            provision(&rt, &prefix).is_ok(),
            "the current marker must short-circuit, touching nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The override list is the whole 2015-2022 runtime, not only the DLL that
    /// was measured to crash: they ship as one redistributable at one version,
    /// and mixing Microsoft's `concrt140` with Wine's `msvcp140` is a
    /// combination nobody tests.
    #[test]
    fn the_override_list_covers_the_runtime_that_ships_together() {
        assert!(NATIVE_DLLS.contains(&"concrt140"), "the one that crashed");
        for dll in ["msvcp140", "vcruntime140", "vcruntime140_1"] {
            assert!(NATIVE_DLLS.contains(&dll), "{dll} ships with it");
        }
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

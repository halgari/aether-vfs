//! Creating a session's prefix: `wineboot`, or Proton's own setup.

use std::io;
use std::path::{Path, PathBuf};

use super::{Prefix, PrefixError};
use crate::layout::Root;
use crate::process::{own_process_group, run_bounded_status};
use crate::runtime::verify_ge;

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

/// How a session's prefix is created, and so where it lives.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PrefixInit {
    /// `wine wineboot -u` into `sessions/<name>/prefix` — what [`ensure`]
    /// does. Enough for a console program; it has none of what Proton adds
    /// to a prefix (DXVK and vkd3d-proton in `system32`, the DirectX and
    /// Visual C++ redistributables from `default_pfx`, and the Steam bridge
    /// `lsteamclient.dll` + `steam.exe`), so most games cannot start in it.
    #[default]
    Wineboot,
    /// Proton's own setup: `<runtime>/proton run cmd /c exit` with
    /// `STEAM_COMPAT_DATA_PATH=sessions/<name>/compat`, so the prefix is
    /// `sessions/<name>/compat/pfx`. Proton records the runtime that set it
    /// up in `compat/version`; a prefix recorded by another runtime is set
    /// up again, which is Proton's own upgrade (or downgrade) path.
    ///
    /// The runtime's `proton` is a Python 3 script, so `python3` must be on
    /// `PATH`. Launches still run the runtime's `wine` directly, so Proton's
    /// per-launch `WINEDLLOVERRIDES` are not applied: a host that wants DXVK
    /// passes [`PROTON_GRAPHICS_OVERRIDES`] with its launch environment.
    Proton {
        /// `STEAM_COMPAT_CLIENT_INSTALL_PATH`: the Steam client's install
        /// directory (`~/.local/share/Steam`). Must exist; Proton reads
        /// Steam's own files from it where they exist and does without them
        /// where they do not.
        steam_client: PathBuf,
        /// Sent as `SteamAppId` and `SteamGameId`; `0` when `None`.
        app_id: Option<u32>,
    },
}

/// The `WINEDLLOVERRIDES` Proton itself sets to run a game on DXVK and
/// vkd3d-proton (its default, non-wined3d configuration), for a host that
/// launches in a [`PrefixInit::Proton`] prefix and wants the same.
pub const PROTON_GRAPHICS_OVERRIDES: &str =
    "d3d11=n;d3d10core=n;d3d9=n;dxgi=n;d3d8=n;d3d12=n;d3d12core=n";

/// How long Proton's prefix setup may take before it is killed. A first
/// setup copies `default_pfx` (seconds); the bound is for a wedged one.
pub const PROTON_INIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// The log Proton's prefix setup writes to, inside `sessions/<name>/compat`.
pub const PROTON_INIT_LOG: &str = "aether-proton-init.log";

/// Where [`ensure_with`] puts `session`'s prefix under `init`, without
/// creating anything: `sessions/<session>/prefix`, or
/// `sessions/<session>/compat/pfx` for [`PrefixInit::Proton`].
pub fn prefix_dir(root: &Root, session: &str, init: &PrefixInit) -> Result<PathBuf, PrefixError> {
    let session_dir = root.try_session_dir(session).map_err(|e| {
        PrefixError::Io(io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))
    })?;
    Ok(match init {
        PrefixInit::Wineboot => session_dir.join("prefix"),
        PrefixInit::Proton { .. } => session_dir.join("compat").join("pfx"),
    })
}

/// [`ensure`], choosing how the prefix is set up. Idempotent: a Wineboot
/// prefix is set up once; a Proton prefix is set up again only when the
/// runtime that set it up (`compat/version`) is not `runtime`.
///
/// `runtime` is verified as GE-Proton before anything runs, as [`ensure`]
/// does.
pub fn ensure_with(
    root: &Root,
    runtime: &Path,
    session: &str,
    init: &PrefixInit,
) -> Result<Prefix, PrefixError> {
    let PrefixInit::Proton { steam_client, app_id } = init else {
        return ensure(root, runtime, session);
    };
    let dir = prefix_dir(root, session, init)?;
    let compat = dir.parent().expect("compat/pfx has a parent").to_path_buf();
    let tag = verify_ge(runtime).map_err(|e| PrefixError::NotGe(e.to_string()))?;
    if is_initialised(&dir) && proton_prefix_version(&compat).as_deref() == Some(tag.as_str()) {
        return Ok(Prefix { dir });
    }
    if !steam_client.is_dir() {
        return Err(PrefixError::ProtonInit(format!(
            "the Steam client directory {} (STEAM_COMPAT_CLIENT_INSTALL_PATH) does not exist",
            steam_client.display()
        )));
    }
    std::fs::create_dir_all(&compat)?;
    run_proton_init(runtime, &compat, steam_client, app_id.unwrap_or(0))?;
    if !is_initialised(&dir) {
        return Err(PrefixError::ProtonInit(format!(
            "proton exited cleanly but {} has no drive_c/windows/system32{}",
            dir.display(),
            log_tail(&compat.join(PROTON_INIT_LOG))
        )));
    }
    Ok(Prefix { dir })
}

/// The runtime tag Proton recorded when it last set up the prefix under
/// `compat` (its `version` file), if any.
fn proton_prefix_version(compat: &Path) -> Option<String> {
    let v = std::fs::read_to_string(compat.join("version")).ok()?;
    Some(v.trim().to_string()).filter(|v| !v.is_empty())
}

fn run_proton_init(
    runtime: &Path,
    compat: &Path,
    steam_client: &Path,
    app_id: u32,
) -> Result<(), PrefixError> {
    let proton = runtime.join("proton");
    let log_path = compat.join(PROTON_INIT_LOG);
    let log = std::fs::File::create(&log_path)?;
    let app = app_id.to_string();
    let mut cmd = std::process::Command::new(&proton);
    // The host's own Proton knobs (`PROTON_USE_WINED3D`, `PROTON_LOG`, …) and
    // DLL overrides would shape the prefix this builds for every later launch;
    // it is set up from this runtime's defaults alone.
    for name in inherited_proton_env(std::env::vars_os().map(|(k, _)| k)) {
        cmd.env_remove(name);
    }
    cmd.args(["run", "cmd", "/c", "exit"])
        .env("STEAM_COMPAT_DATA_PATH", compat)
        .env("STEAM_COMPAT_CLIENT_INSTALL_PATH", steam_client)
        .env("SteamAppId", &app)
        .env("SteamGameId", &app)
        // Game fixes are per-game, not prefix setup, and some of them
        // download (winetricks); none belongs in a prefix aether-vfs builds.
        .env("PROTONFIXES_DISABLE", "1")
        .env("WINEDEBUG", "-all")
        // Proton picks the prefix from STEAM_COMPAT_DATA_PATH; an inherited
        // WINEPREFIX would only confuse a reader of the log.
        .env_remove("WINEPREFIX")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    let status = match run_bounded_status(&mut cmd, PROTON_INIT_TIMEOUT) {
        Ok(s) => s,
        Err(e) => {
            return Err(PrefixError::ProtonInit(format!(
                "could not run {} (a Python 3 script: is python3 installed?): {e}",
                proton.display()
            )))
        }
    };
    match status {
        Some(s) if s.success() => Ok(()),
        Some(s) => Err(PrefixError::ProtonInit(format!(
            "{} run exited {s}{}",
            proton.display(),
            log_tail(&log_path)
        ))),
        None => Err(PrefixError::ProtonInit(format!(
            "{} run did not finish within {:?} and was killed{}",
            proton.display(),
            PROTON_INIT_TIMEOUT,
            log_tail(&log_path)
        ))),
    }
}

/// The names among `inherited` that Proton's prefix setup must not see:
/// `WINEDLLOVERRIDES` and every `PROTON_*` variable. [`run_proton_init`] sets
/// its own after removing these, so none of them is its own.
fn inherited_proton_env(
    inherited: impl Iterator<Item = std::ffi::OsString>,
) -> Vec<std::ffi::OsString> {
    inherited
        .filter(|k| {
            k.to_str()
                .is_some_and(|k| k == "WINEDLLOVERRIDES" || k.starts_with("PROTON_"))
        })
        .collect()
}

/// The last lines of `log`, formatted to follow an error message.
fn log_tail(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines[lines.len().saturating_sub(20)..].join("\n");
    format!("; log {}:\n{tail}", log.display())
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
    let mut cmd = std::process::Command::new(&wine);
    cmd.arg("wineboot")
        .arg("-u")
        .env("WINEPREFIX", prefix_dir)
        .env("WINEDLLOVERRIDES", "mscoree=d;mshtml=d")
        .env("WINEDEBUG", "-all");
    let runtime_abs = std::path::absolute(runtime).unwrap_or_else(|_| runtime.to_path_buf());
    cmd.envs(crate::runtime::runtime_lib_env_host(&runtime_abs));
    own_process_group(&mut cmd);
    let output = cmd.output()?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prefix::scratch;

    /// A runtime whose `proton` is a shell script: it logs its argv and the
    /// env Proton setup is given to `$STEAM_COMPAT_DATA_PATH/calls`, then
    /// does what Proton does to the disk (a `pfx/drive_c/windows/system32`
    /// and a `version` naming the runtime's tag) unless `body` exits first.
    #[cfg(unix)]
    fn fake_proton_runtime(tag: &str, version: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let rt = scratch(tag);
        std::fs::write(rt.join("version"), format!("1 {version}\n")).unwrap();
        let script = format!(
            "#!/bin/sh\n{body}\n\
             echo \"$*|$STEAM_COMPAT_CLIENT_INSTALL_PATH|$SteamAppId|$SteamGameId|$PROTONFIXES_DISABLE\" \
             >> \"$STEAM_COMPAT_DATA_PATH/calls\"\n\
             mkdir -p \"$STEAM_COMPAT_DATA_PATH/pfx/drive_c/windows/system32\"\n\
             awk '{{print $2}}' \"$(dirname \"$0\")/version\" > \"$STEAM_COMPAT_DATA_PATH/version\"\n"
        );
        // Written under a temporary name and renamed into place, so `proton`
        // never exists half-written; `spawn_retrying_busy` covers the other
        // half of `ETXTBSY` (a write descriptor another thread's fork holds).
        let tmp = rt.join("proton.tmp");
        std::fs::write(&tmp, script).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&tmp, rt.join("proton")).unwrap();
        rt
    }

    #[cfg(unix)]
    fn calls(root: &Root, session: &str) -> Vec<String> {
        let f = root.try_session_dir(session).unwrap().join("compat").join("calls");
        std::fs::read_to_string(f).unwrap_or_default().lines().map(str::to_string).collect()
    }

    #[test]
    fn proton_setup_drops_the_hosts_proton_knobs_and_dll_overrides() {
        let names = [
            "PATH",
            "WINEDLLOVERRIDES",
            "PROTON_USE_WINED3D",
            "PROTON_LOG",
            "PROTONFIXES_DISABLE",
            "proton_x",
        ];
        let cleared = inherited_proton_env(names.iter().map(std::ffi::OsString::from));
        assert_eq!(cleared, ["WINEDLLOVERRIDES", "PROTON_USE_WINED3D", "PROTON_LOG"]);
    }

    #[test]
    fn prefix_dir_follows_the_init() {
        let root = Root::at(PathBuf::from("/home/x"));
        let s = root.try_session_dir("s").unwrap();
        assert_eq!(prefix_dir(&root, "s", &PrefixInit::Wineboot).unwrap(), s.join("prefix"));
        let proton = PrefixInit::Proton { steam_client: PathBuf::from("/steam"), app_id: None };
        assert_eq!(prefix_dir(&root, "s", &proton).unwrap(), s.join("compat").join("pfx"));
        assert!(prefix_dir(&root, "../x", &proton).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn proton_init_runs_proton_once_and_is_idempotent() {
        let root = Root::at(scratch("pi-home"));
        let steam = scratch("pi-steam");
        let rt = fake_proton_runtime("pi-rt", "GE-Proton99-1", "");
        let init = PrefixInit::Proton { steam_client: steam.clone(), app_id: Some(489830) };
        let p = ensure_with(&root, &rt, "s", &init).unwrap();
        assert_eq!(p.dir, prefix_dir(&root, "s", &init).unwrap());
        assert!(p.drive_c().join("windows").join("system32").is_dir());
        assert_eq!(
            calls(&root, "s"),
            [format!("run cmd /c exit|{}|489830|489830|1", steam.display())]
        );
        ensure_with(&root, &rt, "s", &init).unwrap();
        assert_eq!(calls(&root, "s").len(), 1, "an up-to-date prefix is not set up again");
    }

    #[cfg(unix)]
    #[test]
    fn a_prefix_set_up_by_another_runtime_is_set_up_again() {
        let root = Root::at(scratch("pu-home"));
        let steam = scratch("pu-steam");
        let rt = fake_proton_runtime("pu-rt", "GE-Proton99-1", "");
        let init = PrefixInit::Proton { steam_client: steam, app_id: None };
        ensure_with(&root, &rt, "s", &init).unwrap();
        std::fs::write(rt.join("version"), "2 GE-Proton99-2\n").unwrap();
        ensure_with(&root, &rt, "s", &init).unwrap();
        let c = calls(&root, "s");
        assert_eq!(c.len(), 2, "{c:?}");
        assert!(c[1].ends_with("|0|0|1"), "no app id is sent as 0: {}", c[1]);
        let compat = root.try_session_dir("s").unwrap().join("compat");
        assert_eq!(proton_prefix_version(&compat).as_deref(), Some("GE-Proton99-2"));
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_proton_setup_reports_its_log() {
        let root = Root::at(scratch("pf-home"));
        let rt = fake_proton_runtime("pf-rt", "GE-Proton99-1", "echo boom-from-proton >&2; exit 7");
        let init = PrefixInit::Proton { steam_client: scratch("pf-steam"), app_id: None };
        match ensure_with(&root, &rt, "s", &init) {
            Err(PrefixError::ProtonInit(m)) => {
                assert!(m.contains("boom-from-proton") && m.contains(PROTON_INIT_LOG), "{m}")
            }
            other => panic!("expected ProtonInit, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn proton_setup_that_leaves_no_prefix_is_an_error() {
        let root = Root::at(scratch("pn-home"));
        let rt = fake_proton_runtime("pn-rt", "GE-Proton99-1", "exit 0");
        let init = PrefixInit::Proton { steam_client: scratch("pn-steam"), app_id: None };
        match ensure_with(&root, &rt, "s", &init) {
            Err(PrefixError::ProtonInit(m)) => assert!(m.contains("system32"), "{m}"),
            other => panic!("expected ProtonInit, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_steam_client_or_a_non_ge_runtime_is_refused_before_proton_runs() {
        let root = Root::at(scratch("pm-home"));
        let rt = fake_proton_runtime("pm-rt", "GE-Proton99-1", "");
        let missing = PrefixInit::Proton { steam_client: scratch("pm-x").join("nope"), app_id: None };
        assert!(matches!(ensure_with(&root, &rt, "s", &missing), Err(PrefixError::ProtonInit(m)) if m.contains("nope")));
        std::fs::write(rt.join("version"), "1 proton-9.0-4\n").unwrap();
        let init = PrefixInit::Proton { steam_client: scratch("pm-steam"), app_id: None };
        assert!(matches!(ensure_with(&root, &rt, "s", &init), Err(PrefixError::NotGe(_))));
        assert!(calls(&root, "s").is_empty(), "proton never ran");
    }
}

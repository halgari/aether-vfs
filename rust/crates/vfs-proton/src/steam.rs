//! What a launch needs for the target's Steam API to find the running Steam
//! client, without the game being started from the client.
//!
//! # What Proton does, and what a direct `wine` launch lacks
//!
//! `proton waitforexitandrun game.exe` runs `wine
//! c:\windows\system32\steam.exe game.exe`. That `steam.exe` is Proton's own
//! helper, and with `SteamGameId` in its environment it:
//!
//! 1. writes its own pid to `HKCU\Software\Valve\Steam\ActiveProcess`
//!    first, then `SteamPath`, and through `lsteamclient.dll`
//!    `SteamClientDll` and `SteamClientDll64` (connecting to the client);
//! 2. writes `steamapps\libraryfolders.vdf` in the prefix's Steam directory
//!    from `STEAM_COMPAT_CLIENT_INSTALL_PATH` and
//!    `STEAM_COMPAT_LIBRARY_PATHS`;
//! 3. becomes a Wine system process, so it lives exactly as long as the
//!    prefix runs any ordinary process.
//!
//! `SteamAPI_IsSteamRunning` reads `ActiveProcess\pid` and reports whether
//! that process is alive. A prefix Proton set up carries the key already, but
//! with the pid of the helper that ran during setup; in a launch that runs
//! `wine` directly no helper exists, so the pid names a dead process or, by
//! coincidence of Wine's pid numbering, a short-lived unrelated one. That is
//! the `SteamAPI_IsSteamRunning() did not locate a running instance of Steam`
//! line. Everything else the Steam API needs already works without the
//! helper: `steam_api64.dll` takes the app id from `SteamAppId`, loads
//! `steamclient64.dll` (which Proton's loader answers with `lsteamclient`),
//! and that loads the host's `~/.steam/sdk64/steamclient.so`, which talks to
//! the running client.
//!
//! # Why the helper is not the target's parent here
//!
//! Proton's `steam.exe game.exe` gives its child no standard handles (the
//! child's stdout and stderr go nowhere, and so does the Unix stdout of
//! everything below it) and returns 0 whatever the child returned. A launch
//! here reports the target's exit code and captures its output, so
//! `vfs-injector` starts the helper beside the target instead
//! ([`vfs_env::INJECT_STEAM_HELPER`], [`STEAM_HELPER`]) and creates the
//! target once the helper has published its pid. The Steam API cannot tell the difference: it looks
//! at the registry and at the process the registry names.
//!
//! The helper also waits on `PROTON_STEAM_EXE_RESTART_APP` and re-runs its
//! program each time a `steam.exe -- steam://rungameid/<id>` (or
//! `steam://launch/<id>`) inside the prefix fires it. Under Proton that
//! program is the game; here it is `rundll32.exe`, so such a request — a
//! launcher's "Play", or `RestartAppIfNecessary` without `SteamAppId` — now
//! does nothing, where without the helper it went to the host client, which
//! started the unmodded game outside the VFS.
//!
//! # A prefix's stale pid
//!
//! The pid a helper wrote stays in the prefix after it exits, and Wine
//! numbers processes the same way in every run, so in a later launch without
//! the helper that pid is often a live process again and
//! `SteamAPI_IsSteamRunning` says true by coincidence. A Proton prefix's
//! launch without the helper therefore still asks the injector to clear it
//! ([`SteamSide::Off`]).
//!
//! # What a launch outside the client cannot have
//!
//! `Timed out waiting for game mapping!` is `ISteamInput::
//! SetInputActionManifestFilePath` waiting, for one second, for the client to
//! have loaded a controller configuration for the app. The client loads one
//! for a game it started itself. Nothing in the game's environment changes
//! that, so the line (and its one-second wait) remains.

use std::path::{Path, PathBuf};

/// The command line that starts Proton's Steam helper, as a Wine process
/// writes it: the helper, and a program for it to run.
///
/// The helper does its setup only when it is given a program; with none it
/// takes its command line for a Steam command and hands it to the host's
/// `steam`. The program here is `rundll32.exe` with no arguments, which
/// returns at once and has no window or console. The helper outlives it: it
/// stays for as long as the prefix runs any ordinary process.
pub const STEAM_HELPER: &str = r"C:\windows\system32\steam.exe C:\windows\system32\rundll32.exe";

/// The `WINEDLLOVERRIDES` entry Proton sets so that `steam.exe` is its
/// built-in helper wherever a program asks for it by name.
pub const STEAM_HELPER_OVERRIDE: &str = "steam.exe=b";

/// What a launch does about Steam: see this module's docs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SteamSide {
    /// Nothing: the environment and the prefix are left as they are. A
    /// prefix Proton did not set up has no Steam state to look after.
    #[default]
    Untouched,
    /// No helper; the injector clears the pid an earlier helper left, so the
    /// program's Steam API does not find a running client by coincidence.
    Off,
    /// The injector starts Proton's Steam helper before the target.
    Helper(SteamLaunch),
}

/// What the launch asked of the injector, and what it reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperStatus {
    /// Nothing was asked ([`SteamSide::Untouched`]).
    NotRequested,
    /// The injector has not reached that step yet.
    Pending,
    /// The helper published itself as process `pid` after `ms` milliseconds.
    Started { pid: u32, ms: u64 },
    /// No helper was asked for, and the stale pid was cleared.
    Cleared,
    /// The helper was asked for and is not running: why, in the injector's
    /// words.
    NotRunning(String),
    /// The target is running and the injector said nothing: it predates the
    /// helper.
    Unreported,
}

/// Where the injector reports what it did about the helper: the ready
/// file's path plus [`vfs_env::STEAM_HELPER_REPORT_SUFFIX`].
pub fn helper_report_path(ready_file: &Path) -> PathBuf {
    let mut s = ready_file.as_os_str().to_owned();
    s.push(vfs_env::STEAM_HELPER_REPORT_SUFFIX);
    PathBuf::from(s)
}

/// The helper's status for a launch that asked `side` of the injector and
/// whose ready file is `ready_file`. `past_helper` says the injector is past
/// the helper step whatever the files say — its `wine` has exited — so that
/// a missing report means an injector that does not write one. The ready
/// file existing says the same: the injector writes its report before it
/// creates the target.
pub fn helper_status(side: &SteamSide, ready_file: &Path, past_helper: bool) -> HelperStatus {
    if *side == SteamSide::Untouched {
        return HelperStatus::NotRequested;
    }
    match std::fs::read_to_string(helper_report_path(ready_file)) {
        Ok(raw) => parse_helper_report(&raw),
        Err(_) if past_helper || ready_file.exists() => HelperStatus::Unreported,
        Err(_) => HelperStatus::Pending,
    }
}

fn parse_helper_report(raw: &str) -> HelperStatus {
    let raw = raw.trim();
    if raw == vfs_env::STEAM_HELPER_CLEARED {
        return HelperStatus::Cleared;
    }
    if let Some(rest) = raw.strip_prefix(vfs_env::STEAM_HELPER_STARTED_PREFIX) {
        let mut parts = rest.splitn(2, ':');
        if let (Some(Ok(pid)), Some(Ok(ms))) =
            (parts.next().map(str::parse), parts.next().map(str::parse))
        {
            return HelperStatus::Started { pid, ms };
        }
    }
    let why = raw
        .strip_prefix(vfs_env::STEAM_HELPER_FAILED_PREFIX)
        .or_else(|| raw.strip_prefix(vfs_env::STEAM_HELPER_DISABLED_PREFIX))
        .unwrap_or(raw);
    HelperStatus::NotRunning(why.to_string())
}

/// The one line a launch says about `status`, if it has anything to say.
pub fn helper_note(side: &SteamSide, status: &HelperStatus) -> Option<String> {
    match status {
        HelperStatus::NotRunning(why) => Some(format!(
            "aether-vfs: the Steam helper is not running ({why}), so the program runs without \
             Steam"
        )),
        HelperStatus::Unreported => Some(format!(
            "aether-vfs: vfs-injector.exe did not report on the Steam helper{}: the Windows \
             artifacts are older than this aether-vfs; rebuild them (bin/build-windows)",
            match side {
                SteamSide::Helper(_) => ", so the program runs without Steam",
                _ => "",
            }
        )),
        _ => None,
    }
}

/// The Steam side of a launch: see this module's docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteamLaunch {
    /// The Steam client's install directory (`~/.local/share/Steam`), sent as
    /// `STEAM_COMPAT_CLIENT_INSTALL_PATH`. Only the path is used; nothing
    /// under it is read here.
    pub client: PathBuf,
    /// The game's app id, sent as `SteamAppId` and `SteamGameId`.
    pub app_id: u32,
}

/// The directory the Steam client keeps its runtime state in — `steam.pid`,
/// `steam.pipe` and the `sdk64` link `lsteamclient` loads `steamclient.so`
/// through: `$HOME/.steam`.
pub fn state_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".steam"))
}

/// The pid of the Steam client running out of `state_dir`
/// ([`state_dir`]), or `None` when none is: `steam.pid` there names a live
/// process called `steam`.
///
/// The pid file is the client's own statement of where it is running; no
/// configuration or credential file is read.
pub fn running_client(state_dir: &Path) -> Option<u32> {
    running_client_in(state_dir, Path::new("/proc"))
}

/// [`running_client`] against a process table at `proc` instead of `/proc`.
pub fn running_client_in(state_dir: &Path, proc: &Path) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(state_dir.join("steam.pid"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    // A pid file outlives its process, and the number is reused.
    let comm = std::fs::read_to_string(proc.join(pid.to_string()).join("comm")).ok()?;
    (comm.trim() == "steam").then_some(pid)
}

/// The one line a launch says when it leaves the Steam helper out because no
/// client is running.
pub fn not_running_note(state_dir: &Path) -> String {
    format!(
        "aether-vfs: the Steam client is not running (no live process for {}), so the \
         program runs without Steam",
        state_dir.join("steam.pid").display()
    )
}

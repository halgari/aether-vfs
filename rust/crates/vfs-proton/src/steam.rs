//! What a launch needs for the target's Steam API to find the running Steam
//! client, without the game being started from the client.
//!
//! # What Proton does, and what a direct `wine` launch lacks
//!
//! `proton waitforexitandrun game.exe` runs `wine
//! c:\windows\system32\steam.exe game.exe`. That `steam.exe` is Proton's own
//! helper, and with `SteamGameId` in its environment it:
//!
//! 1. writes `HKCU\Software\Valve\Steam\ActiveProcess` — `pid` (its own),
//!    `SteamClientDll`, `SteamClientDll64`, `SteamPath` — through
//!    `lsteamclient.dll`;
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

/// The Steam side of a launch: see this module's docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteamLaunch {
    /// The Steam client's install directory (`~/.local/share/Steam`), sent as
    /// `STEAM_COMPAT_CLIENT_INSTALL_PATH`. Only the path is used; nothing
    /// under it is read here.
    pub client: PathBuf,
    /// The game's app id, sent as `SteamAppId`, `SteamGameId` and
    /// `STEAM_COMPAT_APP_ID`.
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

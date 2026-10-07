//! A program launched through `Session::launch` in a Proton prefix finds the
//! running Steam client: `SteamAPI_IsSteamRunning` is true and
//! `SteamAPI_Init` succeeds, with the launch's exit code and output capture
//! intact.
//!
//! The program is `vfs-fixture-steam.exe`, which loads a game's own
//! `steam_api64.dll` and prints what it sees. It runs with the shim injected,
//! like any launch.
//!
//! Needs, and cannot provide for itself:
//! * a verified GE-Proton runtime and `python3` (Proton's prefix setup);
//! * the Windows artifacts from `bin/build-windows` for this test's profile;
//! * a Steam install (`VFS_TEST_STEAM_CLIENT`, else `~/.local/share/Steam`)
//!   and, for the first test, that client **running** and logged in, with
//!   `HOME` pointing at the home it runs in;
//! * a `steam_api64.dll`: `VFS_TEST_STEAM_API_DLL` names one, else the first
//!   found in a game under the client's `steamapps/common`;
//! * `VFS_TEST_STEAM_APP_ID`: an app the account owns (default 489830,
//!   Skyrim Special Edition). The client shows it as running for the few
//!   seconds each launch takes.
//!
//! A test whose runtime, artifacts, Steam install, `steam_api64.dll` or (first test) running
//! client is missing prints `SKIP ...` and passes (`tests/support/mod.rs` has the policy).
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use std::sync::Arc;

mod support;

use vfs_embed::{DiskProvider, LaunchOpts, PrefixInit, Provider, Session};

/// Root 0 as the program sees it: `Session::launch` links the managed root at
/// `C:\vfs-session\root` unless a location is declared.
const ROOT0: &str = r"C:\vfs-session\root";

fn tmp(tag: &str) -> PathBuf {
    support::scratch("vfs-proton-steam", tag)
}

/// `VFS_TEST_STEAM_API_DLL`, else the first `steam_api64.dll` a game under
/// the client ships. `None` (the test skips) when there is none.
fn steam_api_dll(client: &Path) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("VFS_TEST_STEAM_API_DLL") {
        return Some(PathBuf::from(p));
    }
    let common = client.join("steamapps").join("common");
    let mut games: Vec<PathBuf> = std::fs::read_dir(&common)
        .ok()?
        .filter_map(|e| Some(e.ok()?.path().join("steam_api64.dll")))
        .filter(|p| p.is_file())
        .collect();
    games.sort();
    games.into_iter().next()
}

/// The two tests share one prefix (the second checks what the first left in
/// it), so they take turns.
static PREFIX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The `steam-probe: key=value` lines of a launch's log.
fn probe(log: &str) -> BTreeMap<String, String> {
    support::probe_lines(log, "steam-probe: ")
}

struct Probe {
    session: Session,
    opts: LaunchOpts,
    log: PathBuf,
}

/// A served session in a Proton-initialized prefix named `prefix`, with the
/// probe and the Steam API DLL as real files in root 0; `None`, after a
/// `SKIP` line, when this machine lacks the runtime, the artifacts, a Steam
/// client or a `steam_api64.dll`.
fn probe_session(test: &str, tag: &str, prefix: &str) -> Option<Probe> {
    let rig = support::rig(test, "steam", &["vfs-fixture-steam.exe"])?;
    let client = match support::steam_client() {
        Ok(c) => c,
        Err(why) => {
            support::skip(test, why);
            return None;
        }
    };
    let Some(dll) = steam_api_dll(&client) else {
        support::skip(
            test,
            "no steam_api64.dll found under the client's steamapps/common: \
             set VFS_TEST_STEAM_API_DLL",
        );
        return None;
    };
    let app_id: u32 = std::env::var("VFS_TEST_STEAM_APP_ID")
        .ok()
        .map(|v| v.parse().expect("VFS_TEST_STEAM_APP_ID is a number"))
        .unwrap_or(489830);

    let root = tmp(&format!("{tag}-root"));
    std::fs::copy(
        rig.art.path("vfs-fixture-steam.exe"),
        root.join("probe.exe"),
    )
    .unwrap();
    std::fs::copy(&dll, root.join("steam_api64.dll")).unwrap();

    let mut s = Session::new();
    s.set_home(&rig.home);
    s.set_root(&root);
    s.set_state_dir(tmp(&format!("{tag}-state")));
    s.set_overlay(tmp(&format!("{tag}-overlay")));
    s.set_prefix_name(prefix).unwrap();
    s.set_prefix_init(PrefixInit::Proton {
        steam_client: client,
        app_id: Some(app_id),
    });
    s.mount("", Arc::new(DiskProvider::new(&root)) as Arc<dyn Provider>)
        .unwrap();
    s.serve().unwrap();

    let log = tmp(&format!("{tag}-log")).join("wine.log");
    let opts = LaunchOpts {
        image: "probe.exe".into(),
        wait: true,
        shim_dll: Some(rig.art.shim_dll()),
        payload_dll: Some(rig.art.payload_dll()),
        env: BTreeMap::from([
            (
                "VFS_FIXTURE_STEAM_API_DLL".to_string(),
                format!(r"{ROOT0}\steam_api64.dll"),
            ),
            // The controller inits are not what this is about, and one of
            // them waits a second for a mapping only a client-started game
            // gets.
            ("VFS_FIXTURE_STEAM_INPUT".to_string(), "0".to_string()),
        ]),
        log_file: Some(log.clone()),
        ..Default::default()
    };
    Some(Probe {
        session: s,
        opts,
        log,
    })
}

#[test]
#[ignore = "needs a GE-Proton runtime, the Windows artifacts from \
            bin/build-windows, a Steam install with a game shipping steam_api64.dll, and that \
            client running and logged in to an account that owns the app"]
fn a_launched_program_finds_the_running_steam_client() {
    let _turn = PREFIX.lock().unwrap_or_else(|e| e.into_inner());
    let Some(p) = probe_session(
        "proton_steam::a_launched_program_finds_the_running_steam_client",
        "on",
        "steam-probe",
    ) else {
        return;
    };
    let handle = p.session.launch_detached(&p.opts).expect("launch");
    if !handle.steam_helper() {
        support::skip(
            "proton_steam::a_launched_program_finds_the_running_steam_client",
            format!(
                "no Steam helper was started: either the Steam client is not running (start \
                 Steam and log in) or the helper failed to start; launch notes: {:?}",
                handle.notes()
            ),
        );
        return;
    }
    assert!(handle.notes().is_empty(), "{:?}", handle.notes());
    let exit = handle.wait().expect("wait");
    let log = std::fs::read_to_string(&p.log).unwrap();
    assert_eq!(
        exit,
        vfs_embed::LaunchExit::Exited(0),
        "the probe exits 0 only when SteamAPI_Init succeeded, and its code must survive the \
         helper.\n{log}"
    );
    let seen = probe(&log);
    assert!(
        log.contains("[vfs-injector] steam helper: started:"),
        "the injector starts the helper and says so in the captured output.\n{log}"
    );
    assert_eq!(
        seen.get("registry_pid_alive").map(String::as_str),
        Some("true"),
        "{log}"
    );
    assert_eq!(
        seen.get("is_steam_running").map(String::as_str),
        Some("true"),
        "{log}"
    );
    assert_eq!(seen.get("init").map(String::as_str), Some("true"), "{log}");
    assert_eq!(
        seen.get("logged_on").map(String::as_str),
        Some("true"),
        "{log}"
    );
    assert_eq!(
        seen.get("app_id"),
        seen.get("env_SteamAppId"),
        "the client knows the program as the launch's app.\n{log}"
    );
}

#[test]
#[ignore = "needs a GE-Proton runtime, the Windows artifacts from \
            bin/build-windows, and a Steam install with a game shipping steam_api64.dll (the \
            client need not run)"]
fn without_a_running_client_the_launch_goes_ahead_and_says_so() {
    let _turn = PREFIX.lock().unwrap_or_else(|e| e.into_inner());
    // The prefix the other test runs the helper in: the pid it leaves must
    // not make this launch's Steam API find a client.
    let Some(mut p) = probe_session(
        "proton_steam::without_a_running_client_the_launch_goes_ahead_and_says_so",
        "off",
        "steam-probe",
    ) else {
        return;
    };
    // A state directory with no pid file: no client is running, as far as
    // the launch can tell.
    let state = tmp("off-steam-state");
    p.session.set_steam_state_dir(&state);
    let handle = p.session.launch_detached(&p.opts).expect("launch");
    assert!(!handle.steam_helper());
    let notes = handle.notes();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("runs without Steam"), "{notes:?}");
    // Whatever the probe makes of Steam, the launch itself must work: the
    // probe ran and reported.
    handle.wait().expect("wait");
    let log = std::fs::read_to_string(&p.log).unwrap();
    assert!(log.starts_with(&notes[0]), "the note leads the log.\n{log}");
    assert!(
        log.contains("[vfs-injector] steam helper: cleared"),
        "no helper, only the stale pid cleared.\n{log}"
    );
    let seen = probe(&log);
    assert_eq!(
        seen.get("registry_pid").map(String::as_str),
        Some("0"),
        "{log}"
    );
    assert_eq!(
        seen.get("is_steam_running").map(String::as_str),
        Some("false"),
        "{log}"
    );
}

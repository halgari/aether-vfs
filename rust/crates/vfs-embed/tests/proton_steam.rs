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
//! * a verified GE-Proton runtime under `$VFS_HOME/runtimes`, and `python3`
//!   (Proton's prefix setup);
//! * the Windows artifacts from `bin/build-windows` beside the test binary;
//! * a **running Steam client** logged in to an account, and `HOME` pointing
//!   at the home it runs in;
//! * a `steam_api64.dll`: `VFS_TEST_STEAM_API_DLL` names one, else the first
//!   found in a game under the client's `steamapps/common`;
//! * `VFS_TEST_STEAM_APP_ID`: an app the account owns (default 489830,
//!   Skyrim Special Edition). The client shows it as running for the few
//!   seconds each launch takes.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use vfs_embed::{DiskProvider, LaunchOpts, PrefixInit, Provider, Session};

/// Root 0 as the program sees it: `Session::launch` links the managed root at
/// `C:\vfs-session\root` unless a location is declared.
const ROOT0: &str = r"C:\vfs-session\root";

fn tmp(tag: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("vfs-proton-steam-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn profile_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let dir = exe.parent().unwrap();
    if dir.file_name().and_then(|s| s.to_str()) == Some("deps") {
        dir.parent().unwrap().to_path_buf()
    } else {
        dir.to_path_buf()
    }
}

fn artifact(name: &str) -> PathBuf {
    let p = profile_dir().join(name);
    assert!(
        p.is_file(),
        "{} is missing: cross-build the Windows artifacts with `bin/build-windows`",
        p.display()
    );
    p
}

fn steam_client() -> PathBuf {
    let home = std::env::var_os("HOME").expect("HOME");
    PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("Steam")
}

fn steam_api_dll(client: &Path) -> PathBuf {
    if let Some(p) = std::env::var_os("VFS_TEST_STEAM_API_DLL") {
        return PathBuf::from(p);
    }
    let common = client.join("steamapps").join("common");
    let mut games: Vec<PathBuf> = std::fs::read_dir(&common)
        .unwrap_or_else(|e| panic!("reading {}: {e}", common.display()))
        .filter_map(|e| Some(e.ok()?.path().join("steam_api64.dll")))
        .filter(|p| p.is_file())
        .collect();
    games.sort();
    games.into_iter().next().unwrap_or_else(|| {
        panic!(
            "no game under {} ships steam_api64.dll; set VFS_TEST_STEAM_API_DLL",
            common.display()
        )
    })
}

/// The `steam-probe: key=value` lines of a launch's log.
fn probe(log: &str) -> BTreeMap<String, String> {
    log.lines()
        .filter_map(|l| l.trim().strip_prefix("steam-probe: "))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

struct Probe {
    session: Session,
    opts: LaunchOpts,
    log: PathBuf,
}

/// A served session in a Proton-initialized prefix named `prefix`, with the
/// probe and the Steam API DLL as real files in root 0.
fn probe_session(tag: &str, prefix: &str) -> Probe {
    assert!(
        std::env::var_os("VFS_HOME").is_some(),
        "set VFS_HOME to the aether-vfs home holding runtimes/GE-Proton…"
    );
    let client = steam_client();
    let app_id: u32 = std::env::var("VFS_TEST_STEAM_APP_ID")
        .ok()
        .map(|v| v.parse().expect("VFS_TEST_STEAM_APP_ID is a number"))
        .unwrap_or(489830);

    let root = tmp(&format!("{tag}-root"));
    std::fs::copy(artifact("vfs-fixture-steam.exe"), root.join("probe.exe")).unwrap();
    std::fs::copy(steam_api_dll(&client), root.join("steam_api64.dll")).unwrap();

    let mut s = Session::new();
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
        shim_dll: Some(artifact("vfs_shim_dll.dll").to_string_lossy().into_owned()),
        payload_dll: Some(artifact("vfs_payload.dll").to_string_lossy().into_owned()),
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
    Probe {
        session: s,
        opts,
        log,
    }
}

#[test]
#[ignore = "needs a GE-Proton runtime under $VFS_HOME/runtimes, the Windows artifacts from \
            bin/build-windows, and a running, logged-in Steam client that owns the app"]
fn a_launched_program_finds_the_running_steam_client() {
    let p = probe_session("on", "steam-probe");
    let handle = p.session.launch_detached(&p.opts).expect("launch");
    assert!(
        handle.steam_helper(),
        "the Steam client is running, so the helper is asked for"
    );
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
        log.contains("[vfs-injector] steam helper") && log.contains("is process"),
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
#[ignore = "needs a GE-Proton runtime under $VFS_HOME/runtimes and the Windows artifacts from \
            bin/build-windows"]
fn without_a_running_client_the_launch_goes_ahead_and_says_so() {
    let mut p = probe_session("off", "steam-probe-off");
    // A state directory with no pid file: no client is running, as far as
    // the launch can tell.
    let state = tmp("off-steam-state");
    p.session.set_steam_state_dir(&state);
    let handle = p.session.launch_detached(&p.opts).expect("launch");
    assert!(!handle.steam_helper());
    let notes = handle.notes().to_vec();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("runs without Steam"), "{notes:?}");
    // Whatever the probe makes of Steam, the launch itself must work: the
    // probe ran and reported.
    handle.wait().expect("wait");
    let log = std::fs::read_to_string(&p.log).unwrap();
    assert!(log.starts_with(&notes[0]), "the note leads the log.\n{log}");
    assert!(!log.contains("[vfs-injector] steam helper"), "{log}");
    assert!(
        probe(&log).contains_key("is_steam_running"),
        "the probe ran.\n{log}"
    );
}

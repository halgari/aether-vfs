//! `Session::launch` on unix against a **fake** GE-Proton runtime: its
//! `proton`, `wine` and `wineserver` are shell scripts, so everything a Proton
//! launch does on the host side — the aether home, Proton prefix setup,
//! root links, the child's environment, the working directory, the ready
//! timeout, detaching and stopping — runs here without Wine. The real thing
//! is `proton_launch.rs` and `proton_skyrim.rs`, both `#[ignore]`d.
//!
//! The fake `wine` records the environment and argv it was started with in
//! its prefix, then acts on `FAKE_WINE_MODE` (which reaches it only through
//! `LaunchOpts::env`): `ok` exits 0, `sleep` runs until killed, `fail` writes
//! the injector's failure report and exits 3 as `vfs-injector` does, and
//! `game` is `skse64_loader.exe`: it starts a detached "game" that outlives
//! it (running while `fake-game.run` exists in the prefix) and exits 5.
//!
//! It also writes one line to stdout and one to stderr, which is what
//! `LaunchOpts::log_file` captures. When the launch asks the injector about
//! the Steam helper, it writes the report a current injector would
//! (`cleared`, or a started helper), or `FAKE_STEAM_REPORT` instead (`none`:
//! no report, as an injector that predates the helper writes).
//!
//! The fake `wineserver` models the prefix: `-k` kills the fake `wine` and
//! the game, and `-w` returns once the game is gone.
//!
//! The Steam client is faked too, where a test wants one: a process named
//! `steam` and a state directory whose `steam.pid` names it
//! ([`FakeSteam`]). Every other test turns the Steam helper off, so none of
//! them depends on whether a real client is running on the machine.
#![cfg(unix)]

mod support;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use vfs_embed::{
    DiskProvider, HelperStatus, LaunchExit, LaunchOpts, PrefixInit, Session, STOPPED_EXIT_CODE,
};

const ROOT0: &str = r"C:\Games\Fake";

/// Scratch under Cargo's `CARGO_TARGET_TMPDIR`, not `/tmp`.
fn tmp(tag: &str) -> PathBuf {
    support::scratch("vfs-fake-rt", tag)
}

/// Minimal PE: MZ header, e_lfanew, PE32+ optional header, no imports — the
/// same shape `vfs_director::stage`'s own tests and `launch_vfs_content.rs`
/// use. Good enough for [`vfs_director::stage::stage_launch_into`] to accept
/// and stage; nothing here ever actually runs it (the fake `wine` doesn't
/// read it).
fn bare_pe() -> Vec<u8> {
    let mut pe = vec![0u8; 0x400];
    pe[0] = b'M';
    pe[1] = b'Z';
    pe[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
    pe[0x80..0x84].copy_from_slice(b"PE\0\0");
    pe[0x84..0x86].copy_from_slice(&0x8664u16.to_le_bytes());
    pe[0x94..0x96].copy_from_slice(&240u16.to_le_bytes());
    pe[0x98..0x9A].copy_from_slice(&0x20Bu16.to_le_bytes());
    pe
}

fn script(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// An aether home holding one fake runtime, `runtimes/GE-Proton99-1`.
fn fake_home(tag: &str) -> PathBuf {
    let home = tmp(tag);
    let rt = home.join("runtimes").join("GE-Proton99-1");
    std::fs::create_dir_all(&rt).unwrap();
    std::fs::write(rt.join("version"), "1 GE-Proton99-1\n").unwrap();
    script(
        &rt.join("proton"),
        r#"mkdir -p "$STEAM_COMPAT_DATA_PATH/pfx/drive_c/windows/system32"
echo GE-Proton99-1 > "$STEAM_COMPAT_DATA_PATH/version""#,
    );
    script(
        &rt.join("files").join("bin").join("wine"),
        r#"env > "$WINEPREFIX/fake-wine.env"
if [ -n "$VFS_INJECT_STEAM_HELPER" ]; then
  case "$FAKE_STEAM_REPORT" in
    none) ;;
    "") if [ "$VFS_INJECT_STEAM_HELPER" = off ]; then r=cleared; else r=started:236:300; fi
        printf '%s' "$r" > "$6.steam-helper" ;;
    *) printf '%s' "$FAKE_STEAM_REPORT" > "$6.steam-helper" ;;
  esac
fi
echo "fake wine stdout"
echo "fake wine stderr" >&2
echo "$@" > "$WINEPREFIX/fake-wine.args"
echo $$ > "$WINEPREFIX/fake-wine.pid"
case "$FAKE_WINE_MODE" in
  sleep) exec sleep 30 ;;
  fail) echo "target-exited:0xc0000135" > "$6.injector-error"; exit 3 ;;
  game)
    touch "$WINEPREFIX/fake-game.run"
    ( while [ -f "$WINEPREFIX/fake-game.run" ]; do sleep 0.05; done ) </dev/null >/dev/null 2>&1 &
    echo $! > "$WINEPREFIX/fake-game.pid"
    touch "$WINEPREFIX/fake-wine.exited"
    exit 5 ;;
esac
exit 0"#,
    );
    script(
        &rt.join("files").join("bin").join("wineserver"),
        r#"game="$WINEPREFIX/fake-game.pid"
case "$1" in
  -k)
    echo "$WINEPREFIX" >> "$WINEPREFIX/fake-wineserver-k.log"
    if [ -f "$WINEPREFIX/fake-wine.pid" ]; then
      kill "$(cat "$WINEPREFIX/fake-wine.pid")" 2>/dev/null
    fi
    if [ -f "$game" ]; then kill "$(cat "$game")" 2>/dev/null; fi ;;
  -w)
    if [ -f "$game" ]; then
      while kill -0 "$(cat "$game")" 2>/dev/null; do sleep 0.05; done
    fi ;;
esac
exit 0"#,
    );
    home
}

/// Whether the fake `wineserver -k` ran **in this prefix** — proof that a
/// stop reached the real mechanism (`Prefix::stop_wineserver` with this
/// prefix's own `WINEPREFIX`), not just that `stop`/`stop_launch` returned
/// `Ok`.
fn wineserver_was_killed(pfx: &Path) -> bool {
    std::fs::read_to_string(pfx.join("fake-wineserver-k.log"))
        .map(|s| s.lines().any(|l| Path::new(l) == pfx))
        .unwrap_or(false)
}

/// A served session in `home`, with a Proton-initialized named prefix, root 0
/// at [`ROOT0`] holding a real `game.exe`, and placeholder Windows artifacts.
/// Returns the session, the prefix directory and the `shim_dll` path.
fn session(tag: &str, home: &Path) -> (Session, PathBuf, String) {
    let mut s = Session::new();
    s.set_home(home);
    s.set_root(tmp(&format!("{tag}-root")));
    s.set_state_dir(tmp(&format!("{tag}-state")));
    s.set_overlay(tmp(&format!("{tag}-overlay")));
    s.declare_root(0, ROOT0);
    s.set_prefix_name("fake").unwrap();
    s.set_prefix_init(PrefixInit::Proton {
        steam_client: tmp(&format!("{tag}-steam")),
        app_id: Some(489830),
    });
    // Off unless a test asks: a real Steam client may or may not be running.
    s.set_steam_helper(false);
    std::fs::write(s.virtual_root().join("game.exe"), b"MZ").unwrap();
    let art = tmp(&format!("{tag}-art"));
    for f in vfs_proton::artifacts::LAUNCH {
        std::fs::write(art.join(f), b"MZ").unwrap();
    }
    s.serve().unwrap();
    let shim = art.join(vfs_proton::artifacts::SHIM_DLL).to_string_lossy().into_owned();
    let pfx = home.join("sessions").join("fake").join("compat").join("pfx");
    (s, pfx, shim)
}

fn opts(shim: &str, mode: &str, wait: bool) -> LaunchOpts {
    LaunchOpts {
        image: "game.exe".into(),
        wait,
        shim_dll: Some(shim.into()),
        payload_dll: Some(
            shim.replace(vfs_proton::artifacts::SHIM_DLL, vfs_proton::artifacts::PAYLOAD_DLL)
                .into(),
        ),
        env: BTreeMap::from([("FAKE_WINE_MODE".to_string(), mode.to_string())]),
        ..Default::default()
    }
}

/// The environment the fake `wine` was started with.
fn child_env(pfx: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(pfx.join("fake-wine.env"))
        .unwrap()
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn a_launch_sets_up_and_uses_a_proton_prefix() {
    let home = fake_home("pfx");
    let (s, pfx, shim) = session("pfx", &home);
    assert_eq!(s.launch(&opts(&shim, "ok", true)).unwrap(), 0);
    assert!(pfx.join("drive_c").join("windows").join("system32").is_dir());
    assert_eq!(Path::new(&child_env(&pfx)["WINEPREFIX"]), pfx, "wine ran in compat/pfx");
    assert!(
        pfx.join("drive_c").join("Games").join("Fake").is_symlink(),
        "root 0 is linked into the Proton prefix"
    );
}

#[test]
fn the_launch_environment_is_the_childs_alone() {
    assert!(std::env::var_os("SteamAppId").is_none(), "run without SteamAppId in the test env");
    let home = fake_home("env");
    let (s, pfx, shim) = session("env", &home);
    let mut o = opts(&shim, "ok", true);
    o.env.insert("SteamAppId".into(), "489830".into());
    o.env.insert("WINEDLLOVERRIDES".into(), "d3dx9_42=n,b".into());
    o.ready_timeout = Some(Duration::from_secs(240));
    assert_eq!(s.launch(&o).unwrap(), 0);

    let env = child_env(&pfx);
    assert_eq!(env["SteamAppId"], "489830");
    assert_eq!(env["WINEDLLOVERRIDES"], "mscoree=d;mshtml=d;d3dx9_42=n,b");
    assert_eq!(env["WINEDEBUG"], "-all");
    assert_eq!(env["VFS_INJECT_CWD"], ROOT0, "the image's directory is the default cwd");
    assert_eq!(env["VFS_READY_TIMEOUT_SECS"], "240");
    assert!(std::env::var_os("SteamAppId").is_none(), "this process's environment is untouched");
    let args = std::fs::read_to_string(pfx.join("fake-wine.args")).unwrap();
    assert!(args.contains(r"C:\Games\Fake\game.exe"), "{args}");
}

#[test]
fn a_log_file_receives_wines_stdout_and_stderr() {
    let home = fake_home("log");
    let (s, _pfx, shim) = session("log", &home);
    let log = tmp("log-out").join("lists").join("tpf").join("wine.log");
    let mut o = opts(&shim, "ok", true);
    o.log_file = Some(log.clone());
    assert_eq!(s.launch(&o).unwrap(), 0);
    let out = std::fs::read_to_string(&log).unwrap();
    assert_eq!(out, "fake wine stdout\nfake wine stderr\n");

    // The next launch replaces it rather than appending.
    assert_eq!(s.launch(&o).unwrap(), 0);
    assert_eq!(std::fs::read_to_string(&log).unwrap(), out);
}

#[test]
fn a_detached_launch_writes_its_log_file_too() {
    let home = fake_home("log-detach");
    let (s, pfx, shim) = session("log-detach", &home);
    let log = tmp("log-detach-out").join("wine.log");
    let mut o = opts(&shim, "game", false);
    o.log_file = Some(log.clone());
    let h = s.launch_detached(&o).unwrap();
    wait_for(&pfx.join("fake-wine.exited"));
    let out = std::fs::read_to_string(&log).unwrap();
    assert!(out.contains("fake wine stderr"), "{out}");
    h.stop().unwrap();
}

#[test]
fn without_a_log_file_nothing_is_written_beside_the_launch() {
    let home = fake_home("nolog");
    let (s, pfx, shim) = session("nolog", &home);
    assert_eq!(s.launch(&opts(&shim, "ok", true)).unwrap(), 0);
    assert!(opts(&shim, "ok", true).log_file.is_none());
    assert!(pfx.join("fake-wine.env").is_file(), "wine ran");
}

#[test]
fn a_reserved_name_in_the_launch_env_is_refused() {
    let home = fake_home("reserved");
    let (s, _pfx, shim) = session("reserved", &home);
    let mut o = opts(&shim, "ok", true);
    o.env.insert("VFS_VIRTUAL_DIR".into(), r"C:\elsewhere".into());
    let e = s.launch(&o).unwrap_err();
    assert!(e.contains("VFS_VIRTUAL_DIR"), "{e}");
}

#[test]
fn a_relative_cwd_is_under_root_zero() {
    let home = fake_home("cwd");
    let (s, pfx, shim) = session("cwd", &home);
    let mut o = opts(&shim, "ok", true);
    o.cwd = Some("Data/SKSE".into());
    s.launch(&o).unwrap();
    assert_eq!(child_env(&pfx)["VFS_INJECT_CWD"], r"C:\Games\Fake\Data\SKSE");
}

#[test]
fn the_injectors_reason_reaches_the_caller() {
    let home = fake_home("fail");
    let (s, _pfx, shim) = session("fail", &home);
    let e = s.launch(&opts(&shim, "fail", true)).unwrap_err();
    assert!(e.contains("0xc0000135") && e.contains("STATUS_DLL_NOT_FOUND"), "{e}");
}

/// A running "Steam client": a process named `steam`, and the state
/// directory whose `steam.pid` names it. Killed on drop.
struct FakeSteam {
    child: std::process::Child,
    state: PathBuf,
}

impl FakeSteam {
    fn start(tag: &str) -> Self {
        let dir = tmp(&format!("{tag}-fake-steam"));
        // A script's process is named after the script, and `read` holds it
        // on the pipe this keeps open.
        let exe = dir.join("steam");
        script(&exe, "read _");
        let mut cmd = std::process::Command::new(&exe);
        cmd.stdin(std::process::Stdio::piped());
        let child = spawn_retrying_busy(&mut cmd);
        let state = dir.join("state");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("steam.pid"), format!("{}\n", child.id())).unwrap();
        FakeSteam { child, state }
    }
}

impl Drop for FakeSteam {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `cmd.spawn()`, retried while the script it runs is still open for writing
/// in a child another test thread forked (`ETXTBSY`).
fn spawn_retrying_busy(cmd: &mut std::process::Command) -> std::process::Child {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match cmd.spawn() {
            Ok(c) => return c,
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                assert!(Instant::now() < deadline, "{e}");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("spawn: {e}"),
        }
    }
}

const STEAM_NAMES: [&str; 3] = [
    "SteamAppId",
    "SteamGameId",
    "STEAM_COMPAT_CLIENT_INSTALL_PATH",
];

#[test]
fn with_a_running_steam_client_the_launch_asks_for_the_helper_and_sets_steams_environment() {
    for name in STEAM_NAMES {
        assert!(
            std::env::var_os(name).is_none(),
            "run without {name} in the test env"
        );
    }
    let home = fake_home("steam-on");
    let (mut s, pfx, shim) = session("steam-on", &home);
    let steam = FakeSteam::start("steam-on");
    s.set_steam_helper(true);
    s.set_steam_state_dir(&steam.state);
    let log = tmp("steam-on-log").join("wine.log");
    let mut o = opts(&shim, "ok", true);
    o.env
        .insert("WINEDLLOVERRIDES".into(), "d3dx9_42=n,b".into());
    o.log_file = Some(log.clone());
    let mut h = s.launch_detached(&o).unwrap();
    assert!(h.steam_helper());
    ended(&mut h);
    assert_eq!(
        h.steam_helper_status(),
        HelperStatus::Started { pid: 236, ms: 300 }
    );
    assert!(h.notes().is_empty(), "{:?}", h.notes());
    assert_eq!(h.wait().unwrap(), LaunchExit::Exited(0));

    let env = child_env(&pfx);
    for name in ["SteamAppId", "SteamGameId"] {
        assert_eq!(env[name], "489830", "{name}");
    }
    let client = &env["STEAM_COMPAT_CLIENT_INSTALL_PATH"];
    assert!(
        Path::new(client).is_dir() && client.ends_with("steam-on-steam"),
        "the client directory the prefix was set up with: {client}"
    );
    assert_eq!(
        env["VFS_INJECT_STEAM_HELPER"],
        r"C:\windows\system32\steam.exe C:\windows\system32\rundll32.exe"
    );
    assert_eq!(
        env["WINEDLLOVERRIDES"],
        "mscoree=d;mshtml=d;steam.exe=b;d3dx9_42=n,b"
    );
    for name in [
        "SteamClientLaunch",
        "SteamEnv",
        "SteamOverlayGameId",
        "SteamUser",
        "STEAM_COMPAT_APP_ID",
        "STEAM_COMPAT_DATA_PATH",
    ] {
        assert!(
            !env.contains_key(name),
            "{name} is not this launch's to claim"
        );
    }

    // `wine` still runs the injector, with its positional argv: the helper
    // is the injector's to start, not a wrapper around it.
    let args = std::fs::read_to_string(pfx.join("fake-wine.args")).unwrap();
    let args: Vec<&str> = args.split_whitespace().collect();
    assert_eq!(args.len(), 6, "{args:?}");
    assert!(args[0].ends_with(vfs_proton::artifacts::INJECTOR), "{args:?}");
    assert_eq!(args[1], r"C:\Games\Fake\game.exe");
    assert!(args[2].ends_with(vfs_proton::artifacts::SHIM_DLL), "{args:?}");
    assert!(args[3].ends_with(vfs_proton::artifacts::PAYLOAD_DLL), "{args:?}");
    assert!(args[4].ends_with("shim.cfg"), "{args:?}");
    assert!(args[5].ends_with("ready.flag"), "{args:?}");

    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "fake wine stdout\nfake wine stderr\n",
        "nothing is said when the helper is asked for"
    );
}

#[test]
fn without_a_running_steam_client_the_launch_is_as_before_and_says_so_once() {
    let home = fake_home("steam-off");
    let (mut s, pfx, shim) = session("steam-off", &home);
    // A Steam client that is not running: its pid file names a process that
    // has exited.
    let state = {
        let steam = FakeSteam::start("steam-off");
        steam.state.clone()
    };
    s.set_steam_helper(true);
    s.set_steam_state_dir(&state);
    let log = tmp("steam-off-log").join("wine.log");
    let mut o = opts(&shim, "ok", true);
    o.log_file = Some(log.clone());
    let mut h = s.launch_detached(&o).unwrap();
    assert!(!h.steam_helper());
    ended(&mut h);
    assert_eq!(h.steam_helper_status(), HelperStatus::Cleared);
    let notes = h.notes();
    assert_eq!(h.wait().unwrap(), LaunchExit::Exited(0), "no new failure");
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains("the Steam client is not running")
            && notes[0].contains("runs without Steam")
            && notes[0].contains(&*state.join("steam.pid").to_string_lossy()),
        "{notes:?}"
    );

    let env = child_env(&pfx);
    for name in STEAM_NAMES {
        assert!(!env.contains_key(name), "{name}");
    }
    assert_eq!(
        env["VFS_INJECT_STEAM_HELPER"], "off",
        "the injector clears the pid an earlier helper left"
    );
    assert_eq!(env["WINEDLLOVERRIDES"], "mscoree=d;mshtml=d");
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        format!("{}\nfake wine stdout\nfake wine stderr\n", notes[0]),
        "the note leads the log"
    );
}

#[test]
fn the_steam_helper_can_be_turned_off_and_needs_an_app_id() {
    let home = fake_home("steam-none");
    let (mut s, pfx, shim) = session("steam-none", &home);
    let steam = FakeSteam::start("steam-none");
    s.set_steam_state_dir(&steam.state);
    // Off (as `session` leaves it), with a client running.
    let h = s.launch_detached(&opts(&shim, "ok", true)).unwrap();
    assert!(!h.steam_helper() && h.notes().is_empty());
    h.wait().unwrap();
    assert_eq!(child_env(&pfx)["VFS_INJECT_STEAM_HELPER"], "off");

    // On, but the launch is not a Steam game's: no app id anywhere.
    s.set_steam_helper(true);
    s.set_prefix_init(PrefixInit::Proton {
        steam_client: tmp("steam-none-client"),
        app_id: None,
    });
    let h = s.launch_detached(&opts(&shim, "ok", true)).unwrap();
    assert!(!h.steam_helper() && h.notes().is_empty());
    h.wait().unwrap();

    // The host's own `SteamAppId` is an app id.
    let mut o = opts(&shim, "ok", true);
    o.env.insert("SteamAppId".into(), "72850".into());
    let h = s.launch_detached(&o).unwrap();
    assert!(h.steam_helper());
    h.wait().unwrap();
    let env = child_env(&pfx);
    assert_eq!(env["SteamAppId"], "72850");
    assert_eq!(env["SteamGameId"], "72850");
}

#[test]
fn a_helper_that_is_not_running_or_an_injector_that_does_not_report_is_noted() {
    let home = fake_home("steam-report");
    let (mut s, _pfx, shim) = session("steam-report", &home);
    let steam = FakeSteam::start("steam-report");
    s.set_steam_helper(true);
    s.set_steam_state_dir(&steam.state);

    let mut o = opts(&shim, "ok", true);
    o.env.insert(
        "FAKE_STEAM_REPORT".into(),
        "failed:it (process 236) exited before publishing itself".into(),
    );
    let mut h = s.launch_detached(&o).unwrap();
    ended(&mut h);
    assert_eq!(
        h.steam_helper_status(),
        HelperStatus::NotRunning("it (process 236) exited before publishing itself".into())
    );
    let notes = h.notes();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains("exited before publishing itself") && notes[0].contains("without Steam"),
        "{notes:?}"
    );
    h.wait().unwrap();

    // An injector built before the helper existed says nothing at all.
    o.env.insert("FAKE_STEAM_REPORT".into(), "none".into());
    let mut h = s.launch_detached(&o).unwrap();
    ended(&mut h);
    assert_eq!(h.steam_helper_status(), HelperStatus::Unreported);
    let notes = h.notes();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains("Windows artifacts are older") && notes[0].contains("rebuild"),
        "{notes:?}"
    );
    h.wait().unwrap();
}

/// Polls `h` until `wine` has exited, so the injector's report is final.
fn ended(h: &mut vfs_embed::LaunchHandle) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while h.is_running() {
        assert!(Instant::now() < deadline, "the launch did not end");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for(p: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !p.exists() {
        assert!(Instant::now() < deadline, "{} never appeared", p.display());
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_detached_launch_is_held_by_the_session_and_stopped_by_stop_launch() {
    let home = fake_home("detach");
    let (s, pfx, shim) = session("detach", &home);
    assert!(!s.stop_launch().unwrap(), "nothing running yet");
    assert_eq!(s.launch(&opts(&shim, "sleep", false)).unwrap(), 0);
    wait_for(&pfx.join("fake-wine.pid"));
    let t = Instant::now();
    assert!(s.stop_launch().unwrap());
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    assert!(wineserver_was_killed(&pfx), "the stop must reach this prefix's own wineserver");
    assert!(!s.stop_launch().unwrap(), "already stopped");
    // The prefix lock went with it: another launch can start.
    assert_eq!(s.launch(&opts(&shim, "ok", true)).unwrap(), 0);
}

#[test]
fn a_second_launch_while_one_is_running_is_refused_before_staging_can_clobber_it() {
    let home = fake_home("second");
    let mut s = Session::new();
    s.set_home(&home);
    s.set_root(tmp("second-root"));
    s.set_state_dir(tmp("second-state"));
    s.set_overlay(tmp("second-overlay"));
    s.declare_root(0, ROOT0);
    s.set_prefix_name("fake").unwrap();
    s.set_prefix_init(PrefixInit::Proton {
        steam_client: tmp("second-steam"),
        app_id: Some(489830),
    });
    // `game.exe` is VFS content only — no real file under the virtual root —
    // so launching it stages it out: the path finding #1 was about. A real
    // on-disk file (as `session()` uses for the other tests) never stages,
    // so it could not reproduce the bug this guards against.
    let content = tmp("second-content");
    std::fs::write(content.join("game.exe"), bare_pe()).unwrap();
    s.mount("", Arc::new(DiskProvider::new(&content))).unwrap();
    let art = tmp("second-art");
    for f in vfs_proton::artifacts::LAUNCH {
        std::fs::write(art.join(f), b"MZ").unwrap();
    }
    s.serve().unwrap();
    let shim = art.join(vfs_proton::artifacts::SHIM_DLL).to_string_lossy().into_owned();
    let pfx = home.join("sessions").join("fake").join("compat").join("pfx");

    assert_eq!(s.launch(&opts(&shim, "sleep", false)).unwrap(), 0);
    wait_for(&pfx.join("fake-wine.pid"));
    let pid: u32 = std::fs::read_to_string(pfx.join("fake-wine.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let staged = s.virtual_root().join("game.exe");
    assert!(staged.is_file(), "the first launch staged game.exe");

    let e = s.launch(&opts(&shim, "ok", true)).unwrap_err();
    assert!(e.contains("already running"), "{e}");
    assert!(Path::new(&format!("/proc/{pid}")).exists(), "the first launch is still running");
    assert!(staged.is_file(), "the refused second launch must not delete the running one's staging");

    assert!(s.stop_launch().unwrap());
}

#[test]
fn a_waiting_launch_is_stopped_from_another_thread() {
    let home = fake_home("waiting");
    let (s, pfx, shim) = session("waiting", &home);
    let s = Arc::new(s);
    let launcher = {
        let (s, o) = (Arc::clone(&s), opts(&shim, "sleep", true));
        std::thread::spawn(move || s.launch(&o))
    };
    wait_for(&pfx.join("fake-wine.pid"));
    assert!(s.stop_launch().unwrap());
    assert_eq!(launcher.join().unwrap().unwrap(), STOPPED_EXIT_CODE);
    assert!(wineserver_was_killed(&pfx), "the stop must reach this prefix's own wineserver");
}

#[test]
fn a_launch_handle_reports_its_end_and_stops_when_dropped() {
    let home = fake_home("handle");
    let (s, pfx, shim) = session("handle", &home);
    let h = s.launch_detached(&opts(&shim, "ok", true)).unwrap();
    assert_eq!(h.wait().unwrap(), LaunchExit::Exited(0));

    std::fs::remove_file(pfx.join("fake-wine.pid")).unwrap();
    let mut h = s.launch_detached(&opts(&shim, "sleep", true)).unwrap();
    wait_for(&pfx.join("fake-wine.pid"));
    assert_eq!(h.try_wait().unwrap(), None);
    let pid = h.pid();
    drop(h);
    assert!(!Path::new(&format!("/proc/{pid}")).exists(), "dropping a running handle stops it");

    // Remove the stale pid file and wait for the *new* launch's own pid
    // before stopping it — otherwise `stop` could observe the old (already
    // dead) pid still on disk and report success without the new process
    // ever having been reached.
    std::fs::remove_file(pfx.join("fake-wine.pid")).unwrap();
    let h = s.launch_detached(&opts(&shim, "sleep", true)).unwrap();
    wait_for(&pfx.join("fake-wine.pid"));
    assert_eq!(h.stop().unwrap(), LaunchExit::Stopped);
}

/// A stopper kept past its own launch's life must be a no-op instead of
/// reaching whatever the same prefix runs next — the [`Session::stop_launch`]
/// half of this is covered by the `Session`-level tests above; this is the
/// bare [`vfs_embed::LaunchStopper`] a caller might hold directly (from
/// [`vfs_embed::LaunchHandle::stopper`]).
#[test]
fn a_stale_stopper_cannot_reach_a_later_launch_in_the_same_prefix() {
    let home = fake_home("stale");
    let (s, pfx, shim) = session("stale", &home);
    let h = s.launch_detached(&opts(&shim, "ok", true)).unwrap();
    let stale = h.stopper();
    assert_eq!(h.wait().unwrap(), LaunchExit::Exited(0));

    std::fs::remove_file(pfx.join("fake-wine.pid")).unwrap();
    let mut h2 = s.launch_detached(&opts(&shim, "sleep", true)).unwrap();
    wait_for(&pfx.join("fake-wine.pid"));

    stale.stop().unwrap();
    assert_eq!(h2.try_wait().unwrap(), None, "the second launch keeps running");

    assert_eq!(h2.stop().unwrap(), LaunchExit::Stopped);
}

/// `Session` is shared with a launcher thread (`Arc<Session>`), so it must be
/// `Send + Sync` — a `LaunchHandle` inside it included.
#[test]
fn session_is_send_and_sync() {
    fn check<T: Send + Sync>() {}
    check::<Session>();
    check::<vfs_embed::LaunchStopper>();
}

/// The fake game's pid, once the fake launcher has exited having started it.
fn launcher_exited(pfx: &Path) -> u32 {
    wait_for(&pfx.join("fake-wine.exited"));
    // `fake-wine.exited` is written just before the launcher's `exit`.
    std::thread::sleep(Duration::from_millis(200));
    std::fs::read_to_string(pfx.join("fake-game.pid")).unwrap().trim().parse().unwrap()
}

fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn wait_gone(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(pid) {
        assert!(Instant::now() < deadline, "pid {pid} is still running");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `skse64_loader.exe` starts `SkyrimSE.exe` and exits: the launch is the
/// game's, so it runs until the prefix is quiet, and reports the launcher's
/// exit code once it is.
#[test]
fn a_launch_runs_until_its_prefix_is_quiet_not_until_its_launcher_exits() {
    let home = fake_home("quiet");
    let (s, pfx, shim) = session("quiet", &home);
    let mut h = s.launch_detached(&opts(&shim, "game", true)).unwrap();
    let game = launcher_exited(&pfx);
    assert!(alive(game));
    for _ in 0..5 {
        assert_eq!(h.try_wait().unwrap(), None, "the game still runs in the prefix");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(h.is_running());
    let e = s.launch(&opts(&shim, "ok", true)).unwrap_err();
    assert!(e.contains("already running"), "{e}");

    std::fs::remove_file(pfx.join("fake-game.run")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let exit = loop {
        if let Some(exit) = h.try_wait().unwrap() {
            break exit;
        }
        assert!(Instant::now() < deadline, "the launch never ended");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(exit, LaunchExit::Exited(5), "the launcher's exit code");
    assert!(!alive(game));
    assert!(!wineserver_was_killed(&pfx), "a natural end stops nothing");
    drop(h);
    assert_eq!(s.launch(&opts(&shim, "ok", true)).unwrap(), 0, "the prefix is free again");
}

#[test]
fn a_waited_launch_returns_when_the_game_ends_not_the_launcher() {
    let home = fake_home("waitgame");
    let (s, pfx, shim) = session("waitgame", &home);
    let s = Arc::new(s);
    let launcher = {
        let (s, o) = (Arc::clone(&s), opts(&shim, "game", true));
        std::thread::spawn(move || s.launch(&o))
    };
    let game = launcher_exited(&pfx);
    std::thread::sleep(Duration::from_millis(200));
    assert!(!launcher.is_finished(), "launch returned while the game still ran");
    std::fs::remove_file(pfx.join("fake-game.run")).unwrap();
    assert_eq!(launcher.join().unwrap().unwrap(), 5);
    assert!(!alive(game));
}

#[test]
fn stop_launch_stops_the_game_after_its_launcher_exited() {
    let home = fake_home("stopgame");
    let (s, pfx, shim) = session("stopgame", &home);
    assert_eq!(s.launch(&opts(&shim, "game", false)).unwrap(), 0);
    let game = launcher_exited(&pfx);
    std::thread::sleep(Duration::from_millis(100));
    assert!(s.stop_launch().unwrap(), "the launch is running while the game is");
    assert!(wineserver_was_killed(&pfx), "the stop must reach this prefix's own wineserver");
    wait_gone(game);
    assert!(!s.stop_launch().unwrap(), "already stopped");
    assert_eq!(s.launch(&opts(&shim, "ok", true)).unwrap(), 0);
}

#[test]
fn a_kept_handle_is_stopped_by_stop_launch_after_its_launcher_exited() {
    let home = fake_home("keptgame");
    let (s, pfx, shim) = session("keptgame", &home);
    let mut h = s.launch_detached(&opts(&shim, "game", true)).unwrap();
    let game = launcher_exited(&pfx);
    assert_eq!(h.try_wait().unwrap(), None);
    assert!(s.stop_launch().unwrap(), "the session sees the handle its caller kept");
    assert_eq!(h.wait().unwrap(), LaunchExit::Stopped);
    assert!(wineserver_was_killed(&pfx));
    assert!(!alive(game));
}

#[test]
fn dropping_a_handle_after_its_launcher_exited_stops_the_game() {
    let home = fake_home("dropgame");
    let (s, pfx, shim) = session("dropgame", &home);
    let mut h = s.launch_detached(&opts(&shim, "game", true)).unwrap();
    let game = launcher_exited(&pfx);
    assert_eq!(h.try_wait().unwrap(), None);
    drop(h);
    assert!(wineserver_was_killed(&pfx));
    wait_gone(game);
    drop(s);
}

/// A handle `launch_detached` handed back and its caller kept is invisible
/// to `detached`/`waiting`; the session must still refuse a second launch
/// before staging (which would delete the first one's staged files) and
/// stop it on `stop_launch`.
#[test]
fn a_kept_detached_handle_blocks_a_second_launch_and_is_stopped_by_stop_launch() {
    let home = fake_home("kept");
    let mut s = Session::new();
    s.set_home(&home);
    s.set_root(tmp("kept-root"));
    s.set_state_dir(tmp("kept-state"));
    s.set_overlay(tmp("kept-overlay"));
    s.declare_root(0, ROOT0);
    s.set_prefix_name("fake").unwrap();
    s.set_prefix_init(PrefixInit::Proton { steam_client: tmp("kept-steam"), app_id: None });
    let content = tmp("kept-content");
    std::fs::write(content.join("game.exe"), bare_pe()).unwrap();
    s.mount("", Arc::new(DiskProvider::new(&content))).unwrap();
    let art = tmp("kept-art");
    for f in vfs_proton::artifacts::LAUNCH {
        std::fs::write(art.join(f), b"MZ").unwrap();
    }
    s.serve().unwrap();
    let shim = art.join(vfs_proton::artifacts::SHIM_DLL).to_string_lossy().into_owned();
    let pfx = home.join("sessions").join("fake").join("compat").join("pfx");

    let mut h = s.launch_detached(&opts(&shim, "sleep", true)).unwrap();
    wait_for(&pfx.join("fake-wine.pid"));
    let staged = s.virtual_root().join("game.exe");
    assert!(staged.is_file(), "the first launch staged game.exe");

    for o in [opts(&shim, "ok", true), opts(&shim, "ok", false)] {
        let e = s.launch(&o).unwrap_err();
        assert!(e.contains("already running"), "{e}");
    }
    let e = s.launch_detached(&opts(&shim, "ok", true)).err().expect("refused");
    assert!(e.contains("already running"), "{e}");
    assert!(staged.is_file(), "the refused launches must not delete the running one's staging");
    assert_eq!(h.try_wait().unwrap(), None);

    assert!(s.stop_launch().unwrap());
    assert!(wineserver_was_killed(&pfx));
    assert_eq!(h.wait().unwrap(), LaunchExit::Stopped);
}

/// The prefix is locked before it is set up: Proton's setup (and a re-setup
/// by a different runtime) must not run under a program another process is
/// running in that prefix.
#[test]
fn a_prefix_in_use_elsewhere_is_not_set_up_under_it() {
    let home = fake_home("inuse");
    let (s, pfx, shim) = session("inuse", &home);
    let held = vfs_proton::prefix::Prefix { dir: pfx.clone() }.lock().unwrap();
    let e = s.launch(&opts(&shim, "ok", true)).unwrap_err();
    assert!(e.contains("in use"), "{e}");
    assert!(
        !pfx.join("drive_c").exists(),
        "Proton's setup ran on a prefix another process holds"
    );
    drop(held);
    assert_eq!(s.launch(&opts(&shim, "ok", true)).unwrap(), 0);
}

#[test]
fn prepare_prefix_sets_the_prefix_up_as_a_launch_would_without_serving() {
    let home = fake_home("prepare");
    let mut s = Session::new();
    s.set_home(&home);
    s.set_state_dir(tmp("prepare-state"));
    s.set_prefix_name("fake").unwrap();
    s.set_prefix_init(PrefixInit::Proton {
        steam_client: tmp("prepare-steam"),
        app_id: Some(489830),
    });
    let pfx = home.join("sessions").join("fake").join("compat").join("pfx");
    assert_eq!(s.prepare_prefix().unwrap(), pfx);
    assert!(pfx.join("drive_c").join("windows").join("system32").is_dir());
    // A prefix the runtime already set up is left as it is, and the lock is
    // not held after the call.
    assert_eq!(s.prepare_prefix().unwrap(), pfx);
}

#[test]
fn prepare_prefix_without_a_runtime_says_so_and_is_not_a_launch_error() {
    let home = tmp("prepare-none");
    let mut s = Session::new();
    s.set_home(&home);
    s.set_state_dir(tmp("prepare-none-state"));
    s.set_prefix_name("fake").unwrap();
    let e = s.prepare_prefix().unwrap_err();
    assert!(e.starts_with("no verified GE-Proton runtime under "), "{e}");
}

#[test]
fn only_a_launch_adds_the_install_hint_to_the_no_runtime_error() {
    let home = tmp("hint-none");
    let (s, _pfx, shim) = session("hint", &home);
    let e = s.launch(&opts(&shim, "ok", true)).unwrap_err();
    assert!(
        e.starts_with("launch: no verified GE-Proton runtime under ")
            && e.contains("vfs-proton install"),
        "{e}"
    );
    let e = s.prepare_prefix().unwrap_err();
    assert!(!e.contains("vfs-proton install"), "{e}");
}

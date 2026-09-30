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
//! the injector's failure report and exits 3 as `vfs-injector` does.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use vfs_embed::{DiskProvider, LaunchExit, LaunchOpts, PrefixInit, Session, STOPPED_EXIT_CODE};

const ROOT0: &str = r"C:\Games\Fake";

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("vfs-fake-rt-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
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
echo "$@" > "$WINEPREFIX/fake-wine.args"
echo $$ > "$WINEPREFIX/fake-wine.pid"
case "$FAKE_WINE_MODE" in
  sleep) exec sleep 30 ;;
  fail) echo "target-exited:0xc0000135" > "$6.injector-error"; exit 3 ;;
esac
exit 0"#,
    );
    script(
        &rt.join("files").join("bin").join("wineserver"),
        r#"if [ "$1" = "-k" ]; then
  echo "$WINEPREFIX" >> "$WINEPREFIX/fake-wineserver-k.log"
  if [ -f "$WINEPREFIX/fake-wine.pid" ]; then
    kill "$(cat "$WINEPREFIX/fake-wine.pid")" 2>/dev/null
  fi
fi
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
    std::fs::write(s.virtual_root().join("game.exe"), b"MZ").unwrap();
    let art = tmp(&format!("{tag}-art"));
    for f in ["vfs-injector.exe", "vfs_shim_dll.dll", "vfs_payload.dll"] {
        std::fs::write(art.join(f), b"MZ").unwrap();
    }
    s.serve().unwrap();
    let shim = art.join("vfs_shim_dll.dll").to_string_lossy().into_owned();
    let pfx = home.join("sessions").join("fake").join("compat").join("pfx");
    (s, pfx, shim)
}

fn opts(shim: &str, mode: &str, wait: bool) -> LaunchOpts {
    LaunchOpts {
        image: "game.exe".into(),
        wait,
        shim_dll: Some(shim.to_string()),
        payload_dll: Some(shim.replace("vfs_shim_dll", "vfs_payload")),
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
    for f in ["vfs-injector.exe", "vfs_shim_dll.dll", "vfs_payload.dll"] {
        std::fs::write(art.join(f), b"MZ").unwrap();
    }
    s.serve().unwrap();
    let shim = art.join("vfs_shim_dll.dll").to_string_lossy().into_owned();
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

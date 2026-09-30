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

use vfs_embed::{LaunchOpts, PrefixInit, Session};

const ROOT0: &str = r"C:\Games\Fake";

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("vfs-fake-rt-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
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
        r#"if [ "$1" = "-k" ] && [ -f "$WINEPREFIX/fake-wine.pid" ]; then
  kill "$(cat "$WINEPREFIX/fake-wine.pid")" 2>/dev/null
fi
exit 0"#,
    );
    home
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
    assert_eq!(s.launch(&o).unwrap(), 0);

    let env = child_env(&pfx);
    assert_eq!(env["SteamAppId"], "489830");
    assert_eq!(env["WINEDLLOVERRIDES"], "mscoree=d;mshtml=d;d3dx9_42=n,b");
    assert_eq!(env["WINEDEBUG"], "-all");
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

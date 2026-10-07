//! The environment of a launch: what the shim and the injector read, and
//! what the host may add to it.

use std::collections::BTreeMap;

use super::{absolute, path_string, LaunchError, WineLaunch};
use crate::runtime::runtime_lib_env;
use crate::steam::{SteamSide, STEAM_HELPER, STEAM_HELPER_OVERRIDE};

/// `WINEDLLOVERRIDES` every launch carries: Mono and Gecko prompts would
/// otherwise block a launch on a fresh prefix.
pub const BASE_DLL_OVERRIDES: &str = "mscoree=d;mshtml=d";

/// `WINEDEBUG` unless the host sets its own.
pub const DEFAULT_WINEDEBUG: &str = "-all";

/// The environment for a launch: Wine's own three, plus exactly the `VFS_*`
/// names the shim's `try_init_from_env` consults in file-backed mode.
///
/// Mined from `vfs-shim/src/director.rs` rather than from memory:
/// `VFS_RING_PATH` (which *wins* over `VFS_RING_SECTION`), `VFS_RING_BYTES`,
/// `VFS_RING_PAYLOAD_CAP`, `VFS_ARENA_LEN` and `VFS_VIRTUAL_DIR` — the last
/// being the only one with no default, because "which tree is virtualised"
/// has no sensible guess.
///
/// Deliberately **not** set:
/// - `VFS_RING_SECTION` — no named section exists in file-backed mode, and
///   `VFS_RING_PATH` would shadow it anyway.
/// - `VFS_SERVER_EV` / `VFS_CLIENT_EV` — a Wine event cannot wake a native
///   Linux Director, so `connect_source` does not even consult them on the
///   file path (it passes a null event on purpose; the Director spins).
///
/// `VFS_VIRTUAL_ROOTS` is set iff there are extra roots.
///
/// With [`SteamSide::Helper`], also what Steam's own launcher gives a game
/// and something on this path reads: `SteamAppId` and `SteamGameId`
/// (`steam_api64.dll`, `lsteamclient` and the helper) and
/// `STEAM_COMPAT_CLIENT_INSTALL_PATH` (the helper), plus
/// [`vfs_env::INJECT_STEAM_HELPER`] for the injector and
/// [`STEAM_HELPER_OVERRIDE`] in `WINEDLLOVERRIDES`. [`WineLaunch::extra_env`]
/// still wins for the `Steam*` and `STEAM_*` names. Deliberately **not**
/// set: `STEAM_COMPAT_APP_ID` and `STEAM_COMPAT_DATA_PATH` (only the `proton`
/// script reads them, and it does not run here), `SteamClientLaunch` and
/// `SteamEnv` (they say the client started the program, and it did not),
/// `SteamOverlayGameId` (no overlay is loaded), and `SteamUser`/`SteamAppUser`
/// (the account name is in the client's own files, which nothing here
/// reads). With [`SteamSide::Off`], only `VFS_INJECT_STEAM_HELPER=off`.
///
/// `VFS_ARENA_OFFSET` *is* exported even though today's client derives the
/// offset from the ring header: it is what the working (since deleted) `vfs-serve-fb` run
/// published, it is what the Windows `IpcServe::apply_env_roots` sets, and a
/// geometry field that exists at one end and not the other is exactly the
/// drift `vfs-env` was created to stop.
pub fn launch_env(l: &WineLaunch) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("WINEPREFIX".to_string(), path_string(&l.prefix));
    // Absolutized, not passed through: a relative `PROTONPATH` resolves to
    // UMU-Proton (stock Valve Proton), and that downgrade produces no error.
    let runtime_abs = absolute(&l.runtime);
    env.insert("PROTONPATH".to_string(), path_string(&runtime_abs));
    // What `proton`'s `init_wine` sets and a direct `files/bin/wine` run lacks:
    // without Proton's lib dirs `winedmo.so` cannot load its FFmpeg and no mp4
    // opens. An `extra_env` `LD_LIBRARY_PATH` goes after the runtime dirs, in
    // front of the host's value, so it can never displace them.
    // `WINEDLLPATH` the same way: an `extra_env` value goes after the
    // runtime's (and wine-nvml's) directories, in front of the host's.
    let ld_in = caller_then_host(l, "LD_LIBRARY_PATH");
    let dll_in = caller_then_host(l, "WINEDLLPATH");
    for (k, v) in runtime_lib_env(
        &runtime_abs,
        ld_in.as_deref(),
        std::env::var_os("ORIG_LD_LIBRARY_PATH").as_deref(),
        dll_in.as_deref(),
    ) {
        env.insert(k, v);
    }
    // Mono and Gecko prompts would otherwise block a launch on a fresh prefix.
    let mut base_overrides = match &l.steam {
        SteamSide::Helper(_) => merge_dll_overrides(BASE_DLL_OVERRIDES, STEAM_HELPER_OVERRIDE),
        SteamSide::Untouched | SteamSide::Off => BASE_DLL_OVERRIDES.to_string(),
    };
    // NVAPI's own: part of the base, so a caller's entry for one of these
    // DLLs still wins, and `extra_env` can turn any of it back off.
    if let Some(nv) = &l.nvapi {
        base_overrides = merge_dll_overrides(&base_overrides, crate::nvapi::NVAPI_OVERRIDES);
        let inherited_debug = std::env::var_os("DXVK_NVAPI_SET_NGX_DEBUG_OPTIONS").is_some();
        for (k, v) in nv.env(inherited_debug) {
            env.insert(k, v);
        }
        if let (Some(nvml), Some(dll)) = (&nv.nvml_dir, env.get_mut("WINEDLLPATH")) {
            *dll = format!("{}:{dll}", nvml.to_string_lossy());
        }
    }
    env.insert("WINEDLLOVERRIDES".to_string(), base_overrides.clone());
    env.insert("WINEDEBUG".to_string(), DEFAULT_WINEDEBUG.to_string());

    env.insert(vfs_env::RING_PATH.to_string(), path_string(&l.ring_path));
    env.insert(vfs_env::RING_BYTES.to_string(), l.ring_bytes.to_string());
    env.insert(
        vfs_env::RING_PAYLOAD_CAP.to_string(),
        l.payload_cap.to_string(),
    );
    env.insert(
        vfs_env::ARENA_OFFSET.to_string(),
        l.arena_offset.to_string(),
    );
    env.insert(vfs_env::ARENA_LEN.to_string(), l.arena_len.to_string());
    env.insert(vfs_env::VIRTUAL_DIR.to_string(), l.virtual_dir.clone());
    if let Some(spec) = vfs_env::handshake::encode_roots(&l.virtual_roots) {
        env.insert(vfs_env::VIRTUAL_ROOTS.to_string(), spec);
    }

    if l.registry {
        env.insert(vfs_env::REGISTRY.to_string(), "1".to_string());
    }

    if let Some(cwd) = &l.cwd {
        env.insert(vfs_env::INJECT_CWD.to_string(), cwd.clone());
    }

    match &l.steam {
        SteamSide::Untouched => {}
        SteamSide::Off => {
            env.insert(
                vfs_env::INJECT_STEAM_HELPER.to_string(),
                vfs_env::INJECT_STEAM_HELPER_OFF.to_string(),
            );
        }
        SteamSide::Helper(steam) => {
            let app = steam.app_id.to_string();
            env.insert("SteamAppId".to_string(), app.clone());
            env.insert("SteamGameId".to_string(), app);
            env.insert(
                "STEAM_COMPAT_CLIENT_INSTALL_PATH".to_string(),
                path_string(&absolute(&steam.client)),
            );
            env.insert(
                vfs_env::INJECT_STEAM_HELPER.to_string(),
                STEAM_HELPER.to_string(),
            );
        }
    }

    for (k, v) in &l.extra_env {
        if is_reserved_env(k) {
            continue; // refused by `check_extra_env` before any spawn
        }
        if k == "LD_LIBRARY_PATH" || k == "WINEDLLPATH" {
            continue; // merged after the runtime dirs above
        }
        let v = if k == "WINEDLLOVERRIDES" {
            merge_dll_overrides(&base_overrides, v)
        } else {
            v.clone()
        };
        env.insert(k.clone(), v);
    }

    // After `extra_env`: an explicit timeout on the launch beats an inherited
    // or host-supplied one.
    if let Some(secs) = l.ready_timeout_secs {
        env.insert(
            vfs_env::READY_TIMEOUT_SECS.to_string(),
            secs.max(1).to_string(),
        );
    }
    env
}

/// A search-path variable's inherited part: [`WineLaunch::extra_env`]'s value
/// in front of this process's, either alone when the other is unset or empty.
fn caller_then_host(l: &WineLaunch, name: &str) -> Option<std::ffi::OsString> {
    let host = std::env::var_os(name);
    match (l.extra_env.get(name), &host) {
        (Some(x), Some(h)) if !h.is_empty() => Some(std::ffi::OsString::from(format!(
            "{x}:{}",
            h.to_string_lossy()
        ))),
        (Some(x), _) => Some(std::ffi::OsString::from(x)),
        (None, h) => h.clone(),
    }
}

/// Whether `name` is one the launch sets (or clears) itself, and so one
/// [`WineLaunch::extra_env`] may not: the prefix, the runtime, and every
/// handshake name the shim or injector reads to find this session.
///
/// ASCII case-insensitive: Wine hands the Windows side an environment whose
/// names compare without case, so `vfs_virtual_dir` would reach the shim as
/// the same variable.
pub fn is_reserved_env(name: &str) -> bool {
    ["WINEPREFIX", "PROTONPATH"]
        .iter()
        .any(|r| r.eq_ignore_ascii_case(name))
        || vfs_env::handshake::is_handshake(name)
}

/// Refuses an `extra_env` that names a reserved variable ([`is_reserved_env`]).
pub fn check_extra_env(extra: &BTreeMap<String, String>) -> Result<(), LaunchError> {
    match extra.keys().find(|k| is_reserved_env(k)) {
        Some(k) => Err(LaunchError::ReservedEnv(k.clone())),
        None => Ok(()),
    }
}

/// `base` and `extra` as one `WINEDLLOVERRIDES` value, `extra` winning per
/// DLL. Entries are `;`-separated `names=mode`, where `names` may list
/// several DLLs with `,` and `mode` may itself contain `,` (`n,b`); each DLL
/// becomes its own `dll=mode` entry, in first-seen order, matched ASCII
/// case-insensitively. An entry without `=` is kept verbatim. Wine gives no
/// order guarantee between two entries for one DLL, so the merge must leave
/// exactly one.
pub fn merge_dll_overrides(base: &str, extra: &str) -> String {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut put = |name: &str, entry: String| {
        let key = name.to_ascii_lowercase();
        match out.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = entry,
            None => out.push((key, entry)),
        }
    };
    for src in [base, extra] {
        for entry in src.split(';').map(str::trim).filter(|e| !e.is_empty()) {
            match entry.split_once('=') {
                Some((names, mode)) => {
                    for name in names.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                        put(name, format!("{name}={}", mode.trim()));
                    }
                }
                None => put(entry, entry.to_string()),
            }
        }
    }
    out.into_iter()
        .map(|(_, e)| e)
        .collect::<Vec<_>>()
        .join(";")
}

/// The inherited variables a launch clears because it did not set them itself,
/// given the environment [`launch_env`] built: every
/// [`vfs_env::handshake`] transport and injector name it left unset. That
/// includes `VFS_REGISTRY`, so a host that has it set cannot turn the registry
/// hooks on for a launch with no registry layer.
pub(super) fn stale_env(env: &BTreeMap<String, String>) -> Vec<&'static str> {
    vfs_env::handshake::stale(|n| env.contains_key(n)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::tests::{abs, sample};
    use crate::launch::*;

    #[test]
    fn launch_env_puts_the_runtime_libs_first_on_ld_library_path() {
        let l = sample();
        let env = launch_env(&l);
        let rt = path_string(&l.runtime);
        assert!(
            env["LD_LIBRARY_PATH"].starts_with(&format!(
                "{rt}/files/lib/x86_64-linux-gnu:{rt}/files/lib/i386-linux-gnu"
            )),
            "{}",
            env["LD_LIBRARY_PATH"]
        );
        assert!(
            env["WINEDLLPATH"].starts_with(&format!("{rt}/files/lib/vkd3d:{rt}/files/lib/wine"))
        );
        assert!(
            env.contains_key("ORIG_LD_LIBRARY_PATH")
                || std::env::var_os("ORIG_LD_LIBRARY_PATH").is_some()
        );
    }

    #[test]
    fn an_extra_env_ld_library_path_goes_after_the_runtime_dirs() {
        let mut l = sample();
        l.extra_env
            .insert("LD_LIBRARY_PATH".to_string(), "/mine/lib".to_string());
        let ld = launch_env(&l).remove("LD_LIBRARY_PATH").unwrap();
        let rt = path_string(&l.runtime);
        let rt_dirs = format!("{rt}/files/lib/x86_64-linux-gnu:{rt}/files/lib/i386-linux-gnu");
        assert!(ld.starts_with(&rt_dirs), "{ld}");
        let rest = &ld[rt_dirs.len()..];
        assert!(rest.starts_with(":/mine/lib"), "{ld}");
    }

    #[test]
    fn extra_roots_travel_in_the_windows_format() {
        let mut l = sample();
        l.virtual_roots = vec![(1, r"C:\users\steamuser\Saves".into()), (2, r"C:\x".into())];
        let env = launch_env(&l);
        assert_eq!(
            env.get("VFS_VIRTUAL_ROOTS").map(String::as_str),
            Some(r"1=C:\users\steamuser\Saves;2=C:\x")
        );
    }

    #[test]
    fn an_inherited_registry_flag_is_cleared_unless_the_launch_sets_it() {
        let mut l = sample();
        assert!(stale_env(&launch_env(&l)).contains(&"VFS_REGISTRY"));
        l.registry = true;
        assert!(!stale_env(&launch_env(&l)).contains(&"VFS_REGISTRY"));
    }

    #[test]
    fn every_transport_name_the_launch_does_not_set_is_cleared() {
        let l = sample();
        let env = launch_env(&l);
        let stale = stale_env(&env);
        for n in vfs_env::handshake::TRANSPORT
            .iter()
            .chain(vfs_env::handshake::INJECT)
        {
            assert!(
                env.contains_key(*n) != stale.contains(n),
                "{n} must be exactly one of set or cleared"
            );
        }
        assert!(
            stale.contains(&vfs_env::FUSE_CFG),
            "VFS_FUSE_CFG was missing from the old list"
        );
    }

    #[test]
    fn registry_flag_is_set_only_for_a_registry_launch() {
        let mut l = sample();
        assert!(!launch_env(&l).contains_key(vfs_env::REGISTRY));
        l.registry = true;
        assert_eq!(
            launch_env(&l).get("VFS_REGISTRY").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn no_extra_roots_means_no_root_map() {
        assert!(!launch_env(&sample()).contains_key("VFS_VIRTUAL_ROOTS"));
    }

    #[test]
    fn env_carries_the_real_ring_size_not_a_default() {
        let mut l = sample();
        l.ring_bytes = 33_751_040;
        let env = launch_env(&l);
        assert_eq!(
            env.get("VFS_RING_BYTES").map(String::as_str),
            Some("33751040")
        );
        assert!(env.contains_key("VFS_ARENA_LEN"));
        assert!(env.contains_key("VFS_ARENA_OFFSET"));
        assert!(env.contains_key("VFS_RING_PAYLOAD_CAP"));
    }

    #[test]
    fn protonpath_is_absolute_because_a_relative_one_silently_means_stock() {
        let env = launch_env(&sample());
        let p = env.get("PROTONPATH").expect("PROTONPATH");
        assert!(std::path::Path::new(p).is_absolute(), "{p}");
    }

    #[test]
    fn a_relative_runtime_is_absolutized_rather_than_passed_through() {
        // The reason `launch_env` absolutizes at all: a caller holding a
        // relative runtime path would otherwise export a `PROTONPATH` that
        // resolves to stock Proton, with nothing to observe.
        let mut l = sample();
        l.runtime = PathBuf::from("runtimes/GE-Proton11-6-x86_64");
        let env = launch_env(&l);
        let p = env.get("PROTONPATH").expect("PROTONPATH");
        assert!(Path::new(p).is_absolute(), "{p}");
        assert!(p.ends_with("GE-Proton11-6-x86_64"), "{p}");
    }

    #[test]
    fn the_ring_is_named_by_its_wine_path_and_no_section_is_offered() {
        let env = launch_env(&sample());
        assert_eq!(
            env.get(vfs_env::RING_PATH).map(String::as_str),
            Some(r"C:\probe\ring.bin"),
            "the shim maps the ring inside Wine, so this must be the C: form"
        );
        // `VFS_RING_PATH` wins over `VFS_RING_SECTION` in the shim, and no
        // section exists here; setting one would only mislead a reader.
        assert!(!env.contains_key(vfs_env::RING_SECTION));
        // A Wine event cannot wake a native Linux director, and the shim's
        // file-backed path does not consult these at all.
        assert!(!env.contains_key(vfs_env::SERVER_EV));
        assert!(!env.contains_key(vfs_env::CLIENT_EV));
    }

    #[test]
    fn the_managed_root_is_always_set_because_the_shim_has_no_default_for_it() {
        let env = launch_env(&sample());
        assert_eq!(
            env.get(vfs_env::VIRTUAL_DIR).map(String::as_str),
            Some(r"C:\probe\managed"),
        );
        assert_eq!(
            env.get("WINEPREFIX").map(String::as_str),
            Some(path_string(&sample().prefix).as_str()),
        );
        assert_eq!(
            env.get("WINEDLLOVERRIDES").map(String::as_str),
            Some("mscoree=d;mshtml=d"),
        );
        assert_eq!(env.get("WINEDEBUG").map(String::as_str), Some("-all"));
        assert!(!env.contains_key(vfs_env::INJECT_CWD));
        assert!(!env.contains_key(vfs_env::READY_TIMEOUT_SECS));
    }

    #[test]
    fn dll_overrides_merge_with_the_caller_winning_per_dll() {
        assert_eq!(
            merge_dll_overrides(BASE_DLL_OVERRIDES, ""),
            "mscoree=d;mshtml=d"
        );
        assert_eq!(
            merge_dll_overrides(BASE_DLL_OVERRIDES, "d3dx9_42=n,b"),
            "mscoree=d;mshtml=d;d3dx9_42=n,b",
            "a mode with a comma is one mode"
        );
        assert_eq!(
            merge_dll_overrides(BASE_DLL_OVERRIDES, "MSHTML=n;dxgi,d3d11=n; ;"),
            "mscoree=d;MSHTML=n;dxgi=n;d3d11=n",
            "the caller's entry replaces ours in place; a group splits per DLL"
        );
        assert_eq!(
            merge_dll_overrides("a=d", "winemenubuilder"),
            "a=d;winemenubuilder"
        );
    }

    #[test]
    fn extra_env_reaches_the_child_env_with_overrides_merged() {
        let mut l = sample();
        l.extra_env = BTreeMap::from([
            ("WINEDLLOVERRIDES".to_string(), "d3dx9_42=n,b".to_string()),
            ("WINEDEBUG".to_string(), "+loaddll".to_string()),
            ("SteamAppId".to_string(), "489830".to_string()),
        ]);
        let env = launch_env(&l);
        assert_eq!(env["WINEDLLOVERRIDES"], "mscoree=d;mshtml=d;d3dx9_42=n,b");
        assert_eq!(env["WINEDEBUG"], "+loaddll");
        assert_eq!(env["SteamAppId"], "489830");
    }

    #[test]
    fn nvapi_adds_protons_env_and_overrides_under_the_callers() {
        let plain = launch_env(&sample());
        for k in ["DXVK_ENABLE_NVAPI", "NVIDIA_WINE_DLL_DIR"] {
            assert!(!plain.contains_key(k), "{k} without nvapi");
        }
        let mut l = sample();
        l.nvapi = Some(crate::nvapi::Setup {
            copies: Vec::new(),
            ngx_dir: Some(PathBuf::from("/usr/lib/nvidia/wine")),
            nvml_dir: Some(PathBuf::from("/rt/nvml/wine")),
        });
        l.extra_env = BTreeMap::from([(
            "WINEDLLOVERRIDES".to_string(),
            "dxgi=n;nvapi64=b".to_string(),
        )]);
        let env = launch_env(&l);
        assert_eq!(
            env["WINEDLLOVERRIDES"],
            "mscoree=d;mshtml=d;nvapi64=b;nvofapi64=n;nvapi=n;nvcuda=b;dxgi=n",
            "the caller's nvapi64 entry wins"
        );
        assert_eq!(env["DXVK_ENABLE_NVAPI"], "1");
        assert_eq!(env["NVIDIA_WINE_DLL_DIR"], "/usr/lib/nvidia/wine");
        assert!(
            env["WINEDLLPATH"].starts_with("/rt/nvml/wine:"),
            "{}",
            env["WINEDLLPATH"]
        );
        assert!(env["WINEDLLPATH"].ends_with(&plain["WINEDLLPATH"]));

        // A caller's WINEDLLPATH keeps the runtime's and nvml's in front.
        l.extra_env = BTreeMap::from([("WINEDLLPATH".to_string(), "/mine".to_string())]);
        let dll = launch_env(&l)["WINEDLLPATH"].clone();
        assert!(dll.starts_with("/rt/nvml/wine:"), "{dll}");
        assert!(dll.contains("/files/lib/wine:/mine"), "{dll}");

        l.extra_env = BTreeMap::from([("DXVK_ENABLE_NVAPI".to_string(), "0".to_string())]);
        assert_eq!(
            launch_env(&l)["DXVK_ENABLE_NVAPI"],
            "0",
            "the caller can turn it off"
        );
    }

    fn steam_sample() -> WineLaunch {
        let mut l = sample();
        l.prefix = abs("compat/pfx");
        l.steam = SteamSide::Helper(crate::steam::SteamLaunch {
            client: abs("Steam"),
            app_id: 489830,
        });
        l
    }

    #[test]
    fn a_steam_launch_carries_the_helper_and_what_steams_launcher_sets() {
        let l = steam_sample();
        let env = launch_env(&l);
        for name in ["SteamAppId", "SteamGameId"] {
            assert_eq!(env[name], "489830", "{name}");
        }
        assert_eq!(
            env["STEAM_COMPAT_CLIENT_INSTALL_PATH"],
            path_string(&abs("Steam"))
        );
        assert_eq!(
            env[vfs_env::INJECT_STEAM_HELPER],
            r"C:\windows\system32\steam.exe C:\windows\system32\rundll32.exe",
            "the helper sets nothing up unless it is given a program to run"
        );
        assert_eq!(env["WINEDLLOVERRIDES"], "mscoree=d;mshtml=d;steam.exe=b");
        // The client did not start the program, no overlay is loaded, and the
        // account name is not this crate's to read.
        for name in [
            "SteamClientLaunch",
            "SteamEnv",
            "STEAM_COMPAT_APP_ID",
            "STEAM_COMPAT_DATA_PATH",
            "SteamOverlayGameId",
            "SteamUser",
        ] {
            assert!(!env.contains_key(name), "{name}");
        }
        // The helper is started by the injector, so `wine` still runs the
        // injector with its positional argv and nothing else.
        assert_eq!(command_line(&l), command_line(&sample()));
    }

    #[test]
    fn with_the_helper_off_the_injector_is_only_asked_to_clear_the_stale_pid() {
        let mut l = sample();
        l.steam = SteamSide::Off;
        let env = launch_env(&l);
        assert_eq!(env[vfs_env::INJECT_STEAM_HELPER], "off");
        assert_eq!(env["WINEDLLOVERRIDES"], BASE_DLL_OVERRIDES);
        for name in [
            "SteamAppId",
            "SteamGameId",
            "STEAM_COMPAT_CLIENT_INSTALL_PATH",
        ] {
            assert!(!env.contains_key(name), "{name}");
        }
    }

    #[test]
    fn without_steam_the_environment_is_what_it_was() {
        let env = launch_env(&sample());
        for name in [
            "SteamAppId",
            "SteamGameId",
            "STEAM_COMPAT_APP_ID",
            "STEAM_COMPAT_CLIENT_INSTALL_PATH",
            "STEAM_COMPAT_DATA_PATH",
            vfs_env::INJECT_STEAM_HELPER,
        ] {
            assert!(!env.contains_key(name), "{name}");
        }
        assert_eq!(env["WINEDLLOVERRIDES"], BASE_DLL_OVERRIDES);
    }

    #[test]
    fn the_hosts_steam_names_and_overrides_win_over_the_steam_launchs() {
        let mut l = steam_sample();
        l.extra_env = BTreeMap::from([
            ("SteamGameId".to_string(), "12345".to_string()),
            ("WINEDLLOVERRIDES".to_string(), "dxgi=n".to_string()),
        ]);
        let env = launch_env(&l);
        assert_eq!(env["SteamGameId"], "12345");
        assert_eq!(env["SteamAppId"], "489830");
        assert_eq!(
            env["WINEDLLOVERRIDES"],
            "mscoree=d;mshtml=d;steam.exe=b;dxgi=n"
        );
        l.extra_env
            .insert("WINEDLLOVERRIDES".to_string(), "steam.exe=n".to_string());
        assert_eq!(
            launch_env(&l)["WINEDLLOVERRIDES"],
            "mscoree=d;mshtml=d;steam.exe=n"
        );
        assert!(
            is_reserved_env(vfs_env::INJECT_STEAM_HELPER),
            "only the launch asks for the helper"
        );
    }
}

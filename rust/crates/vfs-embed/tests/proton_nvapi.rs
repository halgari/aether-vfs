//! A program launched through `Session::launch` on an NVIDIA machine can
//! initialise NVAPI — what DLSS and DLAA go through — because the launch puts
//! DXVK-NVAPI and the driver's NGX DLLs into the prefix and sets what the
//! `proton` script sets (`vfs_proton::nvapi`). With `LaunchOpts::nvapi` off,
//! `nvapi64.dll` is gone from the prefix and the same program cannot load it.
//!
//! The program is `vfs-fixture-nvapi.exe`, run with the shim injected like any
//! launch, in a Proton-initialized prefix (NVAPI needs DXVK's DXGI).
//!
//! Needs, and cannot provide for itself: a verified GE-Proton runtime and
//! `python3` (Proton's prefix setup); the Windows artifacts from
//! `bin/build-windows` for this test's profile; a Steam install
//! (`VFS_TEST_STEAM_CLIENT`, else `~/.local/share/Steam`) for Proton's setup;
//! and an NVIDIA GPU with the driver's Wine NGX DLLs. A missing one prints
//! `SKIP ...` and passes (`tests/support/mod.rs` has the policy).
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

mod support;

use vfs_embed::{
    DiskProvider, LaunchOpts, PrefixInit, Provider, Session, PROTON_GRAPHICS_OVERRIDES,
};

fn tmp(tag: &str) -> PathBuf {
    support::scratch("vfs-proton-nvapi", tag)
}

/// The `nvapi-probe: key=value` lines of a launch's log.
fn probe(log: &str) -> BTreeMap<String, String> {
    support::probe_lines(log, "nvapi-probe: ")
}

#[test]
#[ignore = "needs a GE-Proton runtime, the Windows artifacts from \
            bin/build-windows, a Steam install, and an NVIDIA GPU with its driver"]
fn a_launched_program_initialises_nvapi_and_not_when_turned_off() {
    const TEST: &str = "proton_nvapi::a_launched_program_initialises_nvapi_and_not_when_turned_off";
    let Some(rig) = support::rig(TEST, "nvapi", &["vfs-fixture-nvapi.exe"]) else {
        return;
    };
    let runtime = support::runtime_dir().expect("rig found a runtime");
    let status = vfs_proton::nvapi::status(&runtime);
    eprintln!("nvapi status: {status:?}");
    if !status.dlss.available {
        support::skip(
            TEST,
            format!("no NVIDIA/NGX support here: {}", status.dlss.reason),
        );
        return;
    }
    let client = match support::steam_client() {
        Ok(c) => c,
        Err(why) => {
            support::skip(TEST, why);
            return;
        }
    };

    let root = tmp("root");
    std::fs::copy(
        rig.art.path("vfs-fixture-nvapi.exe"),
        root.join("probe.exe"),
    )
    .unwrap();
    let mut s = Session::new();
    s.set_home(&rig.home);
    s.set_root(&root);
    s.set_state_dir(tmp("state"));
    s.set_overlay(tmp("overlay"));
    s.set_prefix_name("nvapi-probe").unwrap();
    s.set_prefix_init(PrefixInit::Proton {
        steam_client: client.clone(),
        app_id: None,
    });
    s.mount("", Arc::new(DiskProvider::new(&root)) as Arc<dyn Provider>)
        .unwrap();
    s.serve().unwrap();

    let log = tmp("log").join("wine.log");
    let mut opts = LaunchOpts {
        image: "probe.exe".into(),
        wait: true,
        shim_dll: Some(rig.art.shim_dll()),
        payload_dll: Some(rig.art.payload_dll()),
        env: BTreeMap::from([(
            "WINEDLLOVERRIDES".to_string(),
            PROTON_GRAPHICS_OVERRIDES.to_string(),
        )]),
        log_file: Some(log.clone()),
        nvapi: false,
        ..Default::default()
    };

    // Off first: Proton's own prefix setup copies NVAPI in on an NVIDIA
    // machine, so turning it off must take it back out for the "on" run below
    // to show this launch's copy.
    let code = s.launch(&opts).expect("launch with nvapi off");
    let text = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        code, 11,
        "nvapi64.dll must not load with nvapi off.\n{text}"
    );
    assert!(probe(&text)["nvapi64"].starts_with("error"), "{text}");

    // Proton's setup also copied the NGX DLLs, and turning NVAPI off leaves
    // them (so does the script): take them out, so "loaded" below can only be
    // this launch's copy.
    let pfx = vfs_proton::prefix::prefix_dir(
        &vfs_proton::Root::at(rig.home.clone()),
        "nvapi-probe",
        &PrefixInit::Proton {
            steam_client: client,
            app_id: None,
        },
    )
    .unwrap();
    let sys32 = pfx.join("drive_c/windows/system32");
    for dll in vfs_proton::nvapi::NGX_DLLS {
        let _ = std::fs::remove_file(sys32.join(dll));
    }

    opts.nvapi = true;
    let code = s.launch(&opts).expect("launch with nvapi on");
    let text = std::fs::read_to_string(&log).unwrap();
    eprintln!("{text}");
    let seen = probe(&text);
    assert_eq!(code, 0, "NvAPI_Initialize must succeed.\n{text}");
    assert_eq!(
        seen.get("initialize").map(String::as_str),
        Some("0"),
        "{text}"
    );
    assert_eq!(
        seen.get("nvngx").map(String::as_str),
        Some("loaded"),
        "{text}"
    );
    for dll in vfs_proton::nvapi::NGX_DLLS {
        assert!(sys32.join(dll).is_file(), "{dll} put back by the launch");
    }
    assert_eq!(
        seen.get("nvidia_wine_dll_dir").map(PathBuf::from),
        status.ngx_dir,
        "{text}"
    );
    assert!(
        seen.get("gpus")
            .is_some_and(|n| n != "0" && !n.starts_with("error")),
        "{text}"
    );
}

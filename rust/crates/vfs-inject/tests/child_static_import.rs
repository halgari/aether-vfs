//! Child dual-layer + static-import proof: a dual-layer parent spawns
//! vfs-staticimp from an isolated app dir (no vproxy.dll on disk). The child
//! only succeeds if CPIW dual-layer inject armed the early payload with the
//! config static-import table — classic LoadLibrary inject cannot virtualize
//! the child EXE's own PE static imports.
mod common;

use std::time::Duration;
use vfs_inject::{run_target_with_shim, RunConfig};
use vfs_shim::{encode_config_full, StaticImport};

#[test]
fn child_static_import_via_dual_layer_cpiw() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-child-stimp-{pid}"));
    let app_dir = base.join("app");
    let mods = base.join("mods");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::create_dir_all(&mods).unwrap();

    let backing = mods.join("vproxy_backing.dll");
    std::fs::copy(common::locate_artifact("vproxy.dll"), &backing).unwrap();

    let child_exe = app_dir.join("vfs-staticimp.exe");
    std::fs::copy(common::locate_artifact("vfs-staticimp.exe"), &child_exe).unwrap();
    assert!(!app_dir.join("vproxy.dll").exists());
    let child_result = app_dir.join("child_result.txt");
    let _ = std::fs::remove_file(&child_result);

    let snapshot = {
        use vfs_core::{build, Layer, LayerId};
        vfs_shared::bridge::flatten(
            &build(vec![Layer {
                id: LayerId(0),
                entries: vec![],
            }])
            .unwrap(),
        )
    };
    let config_bytes = encode_config_full(
        app_dir.to_str().unwrap(),
        "",
        &[StaticImport {
            dll_name: "vproxy.dll".into(),
            backing_path: backing.to_str().unwrap().to_string(),
        }],
        &snapshot,
    );
    let config_path = base.join("shim.cfg");
    std::fs::write(&config_path, &config_bytes).unwrap();

    let ready = base.join("ready.flag");
    let _ = std::fs::remove_file(&ready);

    let (dll, payload) = common::locate_shim_and_payload();
    let spawner = env!("CARGO_BIN_EXE_vfs-spawn-child").to_string();
    let exit = run_target_with_shim(RunConfig {
        target_exe: spawner,
        current_dir: None,
        args: vec![
            child_exe.to_str().unwrap().to_string(),
            app_dir.to_str().unwrap().to_string(),
            child_result.to_str().unwrap().to_string(),
        ],
        dll_path: dll,
        config_path: config_path.to_str().unwrap().to_string(),
        ready_path: ready.to_str().unwrap().to_string(),
        ready_timeout: Duration::from_secs(20),
        payload_path: payload,
        preinit_redirects: vec![],
        detach: false,
        target_pe_bytes: None,
    })
    .expect("run dual-layer parent spawner");

    assert_eq!(
        exit, 0,
        "child staticimp must exit 0 (vproxy_value=4242) via pre-init redirect"
    );
    let out = std::fs::read_to_string(&child_result).expect("child result file");
    assert_eq!(out.trim(), "vproxy_value=4242");
    assert!(
        !app_dir.join("vproxy.dll").exists(),
        "must not write proxy DLL into the child app directory"
    );
}

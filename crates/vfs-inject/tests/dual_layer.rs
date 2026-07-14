//! Dual-layer proof: static-import redirect (config table) + virtual file
//! (Engine secondary) under the unified inject path.
mod common;

use std::time::Duration;
use vfs_inject::{run_target_with_shim, RunConfig};
use vfs_shim::{encode_config_full, StaticImport};

#[test]
fn dual_layer_static_import_and_virtual_file() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-dual-{pid}"));
    let app_dir = base.join("app");
    let mods = base.join("mods");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::create_dir_all(&mods).unwrap();

    let backing_src = common::locate_artifact("vproxy.dll");
    let backing = base.join("backing_vproxy.dll");
    std::fs::copy(&backing_src, &backing).unwrap();

    let data_backing = mods.join("asset.dat");
    std::fs::write(&data_backing, b"DUAL-LAYER-MOD").unwrap();
    let root = app_dir.clone();
    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "asset.dat".into(),
                kind: EntryKind::File,
                source: data_backing.to_str().unwrap().into(),
                size: 14,
                mtime: 1,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };
    let config_bytes = encode_config_full(
        root.to_str().unwrap(),
        "",
        &[StaticImport {
            dll_name: "vproxy.dll".into(),
            backing_path: backing.to_str().unwrap().to_string(),
        }],
        &snapshot,
    );
    let config_path = base.join("shim.cfg");
    std::fs::write(&config_path, &config_bytes).unwrap();

    let (dll, payload) = common::locate_shim_and_payload();
    let ready = base.join("ready.flag");

    let tgt_src = common::locate_artifact("vfs-staticimp.exe");
    let tgt = app_dir.join("vfs-staticimp.exe");
    std::fs::copy(&tgt_src, &tgt).unwrap();
    assert!(!app_dir.join("vproxy.dll").exists());
    let result_path = app_dir.join("result.txt");
    let _ = std::fs::remove_file(&result_path);
    let _ = std::fs::remove_file(&ready);

    let exit = run_target_with_shim(RunConfig {
        target_exe: tgt.to_str().unwrap().to_string(),
        current_dir: None,
        args: vec![result_path.to_str().unwrap().to_string()],
        dll_path: dll.clone(),
        config_path: config_path.to_str().unwrap().to_string(),
        ready_path: ready.to_str().unwrap().to_string(),
        ready_timeout: Duration::from_secs(15),
        payload_path: payload.clone(),
        preinit_redirects: vec![],
        detach: false,
    })
    .expect("dual-layer staticimp");
    assert_eq!(exit, 0, "staticimp under dual-layer");
    let out = std::fs::read_to_string(&result_path).expect("result");
    assert_eq!(out.trim(), "vproxy_value=4242");
    assert!(!app_dir.join("vproxy.dll").exists());

    let probe = env!("CARGO_BIN_EXE_vfs-probe").to_string();
    let virtual_path = app_dir.join("asset.dat");
    let output_path = base.join("probe-out.bin");
    let _ = std::fs::remove_file(&output_path);
    let _ = std::fs::remove_file(&ready);

    let exit = run_target_with_shim(RunConfig {
        target_exe: probe,
        current_dir: None,
        args: vec![
            virtual_path.to_str().unwrap().to_string(),
            output_path.to_str().unwrap().to_string(),
        ],
        dll_path: dll,
        config_path: config_path.to_str().unwrap().to_string(),
        ready_path: ready.to_str().unwrap().to_string(),
        ready_timeout: Duration::from_secs(15),
        payload_path: payload,
        preinit_redirects: vec![],
        detach: false,
    })
    .expect("dual-layer probe");
    assert_eq!(exit, 0, "probe under dual-layer");
    let got = std::fs::read(&output_path).expect("probe out");
    assert_eq!(got, b"DUAL-LAYER-MOD");
}

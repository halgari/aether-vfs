//! Static-import redirects come from the **config file** (not only RunConfig),
//! so director and children share one source of truth.
mod common;

use std::time::Duration;
use vfs_inject::{run_target_with_preinit, run_target_with_shim, PreinitConfig, RunConfig};
use vfs_shim::{encode_config_full, StaticImport};

#[test]
fn config_static_import_via_dual_layer() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-stcfg-{pid}"));
    let app_dir = base.join("app");
    let mods = base.join("mods");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::create_dir_all(&mods).unwrap();

    let backing_src = common::locate_artifact("vproxy.dll");
    let backing = mods.join("vproxy_backing.dll");
    std::fs::copy(&backing_src, &backing).unwrap();

    let snapshot = {
        use vfs_core::{build, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
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

    let tgt_src = common::locate_artifact("vfs-staticimp.exe");
    let tgt = app_dir.join("vfs-staticimp.exe");
    std::fs::copy(&tgt_src, &tgt).unwrap();
    assert!(!app_dir.join("vproxy.dll").exists());

    let result_path = app_dir.join("result.txt");
    let ready = base.join("ready.flag");
    let _ = std::fs::remove_file(&result_path);
    let _ = std::fs::remove_file(&ready);

    let (dll, payload) = common::locate_shim_and_payload();
    let exit = run_target_with_shim(RunConfig {
        target_exe: tgt.to_str().unwrap().to_string(),
        args: vec![result_path.to_str().unwrap().to_string()],
        dll_path: dll,
        config_path: config_path.to_str().unwrap().to_string(),
        ready_path: ready.to_str().unwrap().to_string(),
        ready_timeout: Duration::from_secs(15),
        payload_path: payload,
        preinit_redirects: vec![],
        detach: false,
    })
    .expect("run_target_with_shim");

    assert_eq!(exit, 0);
    let out = std::fs::read_to_string(&result_path).expect("result");
    assert_eq!(out.trim(), "vproxy_value=4242");
    assert!(!app_dir.join("vproxy.dll").exists());
}

#[test]
fn merge_preinit_loads_config_statics() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-merge-{pid}"));
    std::fs::create_dir_all(&base).unwrap();
    let bak = base.join("real.dll");
    std::fs::write(&bak, b"x").unwrap();
    let cfg = encode_config_full(
        r"C:\Game",
        "",
        &[StaticImport {
            dll_name: "d3d11.dll".into(),
            backing_path: bak.to_str().unwrap().to_string(),
        }],
        &[0u8, 1, 2],
    );
    let path = base.join("c.cfg");
    std::fs::write(&path, &cfg).unwrap();
    let merged = vfs_inject::merge_preinit_redirects(path.to_str().unwrap(), &[]);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].suffix, "d3d11.dll");
    assert!(merged[0].backing_nt.starts_with(r"\??\"));
    assert_eq!(merged[0].backing_size, 1);
}

#[test]
fn preinit_only_still_accepts_explicit_redirects() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-pre-{pid}"));
    let app_dir = base.join("app");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&app_dir).unwrap();
    let backing = base.join("b.dll");
    std::fs::copy(common::locate_artifact("vproxy.dll"), &backing).unwrap();
    let size = std::fs::metadata(&backing).unwrap().len();
    let tgt = app_dir.join("vfs-staticimp.exe");
    std::fs::copy(common::locate_artifact("vfs-staticimp.exe"), &tgt).unwrap();
    let result = app_dir.join("r.txt");
    let exit = run_target_with_preinit(PreinitConfig {
        target_exe: tgt.to_str().unwrap().to_string(),
        args: vec![result.to_str().unwrap().to_string()],
        current_dir: Some(app_dir.to_str().unwrap().to_string()),
        payload_path: common::locate_artifact("vfs_payload.dll"),
        redirects: vec![vfs_inject::PreinitRedirect {
            suffix: "vproxy.dll".into(),
            backing_nt: format!(r"\??\{}", backing.to_string_lossy()),
            backing_size: size,
        }],
    })
    .expect("preinit");
    assert_eq!(exit, 0);
    assert_eq!(
        std::fs::read_to_string(&result).unwrap().trim(),
        "vproxy_value=4242"
    );
}

//! Lightweight asserts for dual-layer machinery side-effects.
mod common;

use std::time::Duration;
use vfs_inject::{merge_preinit_redirects, run_target_with_shim, RunConfig};
use vfs_shim::{encode_config_full, StaticImport};

#[test]
fn dual_layer_writes_ready_marker_and_payload_cfg_file() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-mach-{pid}"));
    let root = base.join("root");
    let mods = base.join("mods");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&mods).unwrap();

    let backing = mods.join("asset.dat");
    std::fs::write(&backing, b"SMOKE").unwrap();
    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        vfs_shared::bridge::flatten(
            &build(vec![Layer {
                id: LayerId(0),
                entries: vec![InputEntry {
                    vpath: "asset.dat".into(),
                    kind: EntryKind::File,
                    source: backing.to_str().unwrap().into(),
                    size: 5,
                    mtime: 1,
                }],
            }])
            .unwrap(),
        )
    };
    let config_path = base.join("shim.cfg");
    std::fs::write(
        &config_path,
        encode_config_full(root.to_str().unwrap(), "", &[], &snapshot),
    )
    .unwrap();

    let ready = base.join("ready.flag");
    let _ = std::fs::remove_file(&ready);

    let out = base.join("out.bin");
    let virtual_path = root.join("asset.dat");
    let (dll, payload) = common::locate_shim_and_payload();
    let exit = run_target_with_shim(RunConfig {
        target_exe: env!("CARGO_BIN_EXE_vfs-probe").to_string(),
        current_dir: None,
        args: vec![
            virtual_path.to_str().unwrap().to_string(),
            out.to_str().unwrap().to_string(),
        ],
        dll_path: dll,
        config_path: config_path.to_str().unwrap().to_string(),
        ready_path: ready.to_str().unwrap().to_string(),
        ready_timeout: Duration::from_secs(15),
        payload_path: payload,
        preinit_redirects: vec![],
        detach: false,
        target_pe_bytes: None,
    })
    .expect("dual-layer probe");

    assert_eq!(exit, 0);
    assert_eq!(std::fs::read(&out).unwrap(), b"SMOKE");
    assert!(
        ready.exists(),
        "VFS_SHIM_READY marker must be written (full shim bootstrap completed)"
    );
}

#[test]
fn explicit_preinit_overrides_config_same_suffix() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-ov-{pid}"));
    std::fs::create_dir_all(&base).unwrap();
    let cfg_bak = base.join("from_config.dll");
    let extra_bak = base.join("from_extra.dll");
    std::fs::write(&cfg_bak, b"aa").unwrap();
    std::fs::write(&extra_bak, b"bbbb").unwrap();
    let cfg = encode_config_full(
        r"C:\G",
        "",
        &[StaticImport {
            dll_name: "vproxy.dll".into(),
            backing_path: cfg_bak.to_str().unwrap().to_string(),
        }],
        &[0],
    );
    let path = base.join("c.cfg");
    std::fs::write(&path, cfg).unwrap();

    let merged = merge_preinit_redirects(
        path.to_str().unwrap(),
        &[vfs_inject::PreinitRedirect {
            suffix: "vproxy.dll".into(),
            backing_nt: format!(r"\??\{}", extra_bak.to_string_lossy()),
            backing_size: 4,
        }],
    );
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].backing_size, 4);
    assert!(merged[0].backing_nt.contains("from_extra"));
}

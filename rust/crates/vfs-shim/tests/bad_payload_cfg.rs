//! Dual-layer cfg validation: a garbage VFS_PAYLOAD_CFG_FILE must not crash
//! bootstrap — fall back to full `install()` instead of install_late on a bad
//! pointer. Standalone process (install is process-global).

use vfs_shim::{bootstrap_from_config_path, encode_config};

#[test]
fn bad_payload_cfg_file_falls_back_to_full_install() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-badcfg-{pid}"));
    std::fs::create_dir_all(&base).unwrap();

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
    let root = base.join("root");
    std::fs::create_dir_all(&root).unwrap();
    let cfg_bytes = encode_config(root.to_str().unwrap(), &snapshot);
    let cfg_path = base.join("shim.cfg");
    std::fs::write(&cfg_path, &cfg_bytes).unwrap();

    // Point at a hex address that is almost certainly not a live payload Config
    // (low canonical null-page-ish / invalid for usable nt_protect match).
    let bad_ptr_file = base.join("bad_payload_cfg.txt");
    std::fs::write(&bad_ptr_file, "1").unwrap(); // address 0x1

    std::env::set_var("VFS_PAYLOAD_CFG_FILE", bad_ptr_file.to_str().unwrap());
    std::env::set_var("VFS_DUAL_LAYER", "1");

    let result = bootstrap_from_config_path(cfg_path.to_str().unwrap());

    std::env::remove_var("VFS_PAYLOAD_CFG_FILE");
    std::env::remove_var("VFS_DUAL_LAYER");

    // Do not Debug-format Result: HookGuard does not implement Debug.
    assert!(
        result.is_ok(),
        "bootstrap must succeed via full install fallback (bad cfg pointer rejected)"
    );
    // Keep guard alive for the rest of the process (test binary exits after).
    let _guard = result.ok();
}

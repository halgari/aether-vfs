//! Lightweight asserts for dual-layer machinery side-effects.
//!
//! `dual_layer_writes_ready_marker_and_payload_cfg_file` was removed here for
//! gate 3, Task 3 ("retire standalone mode"): it launched a full dual-layer
//! `run_target_with_shim` process with no `VFS_RING_SECTION` set and asserted
//! the ready-marker plus a virtual-file read, both through the now-retired
//! no-director bootstrap path (`bootstrap_from_config_path_with_payload`
//! aborts on `FuseInitError::NotConfigured`). The remaining test below is a
//! pure-function check of `merge_preinit_redirects` and does not touch
//! bootstrap at all.
mod common;

use vfs_inject::merge_preinit_redirects;
use vfs_shim::{encode_config_full, StaticImport};

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

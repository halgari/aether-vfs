//! Single-test binary: launch the probe, inject the shim, and verify the probe's
//! read of a VIRTUAL path was redirected to the backing file.
mod common;

use std::time::Duration;
use vfs_inject::{run_target_with_shim, RunConfig};

#[test]
fn injected_shim_redirects_target_file_open() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-e2e-{pid}"));
    let root = base.join("gameroot");
    let backing_dir = base.join("mods");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&backing_dir).unwrap();

    let backing = backing_dir.join("asset.dat");
    std::fs::write(&backing, b"REDIRECTED MOD CONTENT").unwrap();
    let virtual_path = root.join("asset.dat");
    assert!(std::fs::read(&virtual_path).is_err());

    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let source = backing.to_str().unwrap();
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "asset.dat".into(),
                kind: EntryKind::File,
                source: source.into(),
                size: 22,
                mtime: 1,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };

    let config_bytes = vfs_shim::encode_config(root.to_str().unwrap(), &snapshot);
    let config_path = base.join("shim.cfg");
    std::fs::write(&config_path, &config_bytes).unwrap();

    let ready_path = base.join("ready.flag");
    let _ = std::fs::remove_file(&ready_path);
    let output_path = base.join("probe-out.bin");
    let _ = std::fs::remove_file(&output_path);

    let probe = env!("CARGO_BIN_EXE_vfs-probe").to_string();
    let (dll, payload) = common::locate_shim_and_payload();

    let exit = run_target_with_shim(RunConfig {
        target_exe: probe,
        args: vec![
            virtual_path.to_str().unwrap().to_string(),
            output_path.to_str().unwrap().to_string(),
        ],
        dll_path: dll,
        config_path: config_path.to_str().unwrap().to_string(),
        ready_path: ready_path.to_str().unwrap().to_string(),
        ready_timeout: Duration::from_secs(10),
        payload_path: payload,
        preinit_redirects: vec![],
    })
    .expect("run_target_with_shim");

    assert_eq!(exit, 0, "probe exit code");
    let got = std::fs::read(&output_path).expect("probe output");
    assert_eq!(got, b"REDIRECTED MOD CONTENT", "redirect did not deliver mod bytes");
}

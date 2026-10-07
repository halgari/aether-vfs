//! A ready timeout so short that it expires while the target is still parked
//! (before the payload's install sentinel, or before the shim reports) must
//! kill the target. Either wait ends in `Timeout`; neither may leave the
//! process alive or let it run.
//!
//! Single-test binary: `run_target_with_shim` mutates process-global env vars.
mod common;

use std::time::Duration;
use vfs_inject::{run_target_with_shim, InjectError, RunConfig};

#[test]
fn an_immediate_timeout_kills_the_parked_process() {
    let base = std::env::temp_dir().join(format!("vfs-sentinel-to-{}", std::process::id()));
    let root = base.join("gameroot");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&root).unwrap();
    let config_path = base.join("shim.cfg");
    std::fs::write(&config_path, vfs_shim::encode_config(root.to_str().unwrap())).unwrap();
    let output_path = base.join("probe-out.bin");
    let (dll, payload) = common::locate_shim_and_payload();

    let result = run_target_with_shim(RunConfig {
        target_exe: env!("CARGO_BIN_EXE_vfs-probe").to_string(),
        current_dir: None,
        args: vec![
            root.join("x.bin").to_str().unwrap().to_string(),
            output_path.to_str().unwrap().to_string(),
        ],
        dll_path: dll,
        config_path: config_path.to_str().unwrap().to_string(),
        ready_path: base.join("ready.flag").to_str().unwrap().to_string(),
        ready_timeout: Duration::from_millis(1),
        payload_path: payload,
        preinit_redirects: vec![],
        detach: false,
    });

    assert!(matches!(result, Err(InjectError::Timeout)), "got {result:?}");
    for _ in 0..40 {
        if output_path.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!output_path.exists(), "the target ran after the timeout");
    let _ = std::fs::remove_dir_all(&base);
}

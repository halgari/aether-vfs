//! A shim config from another build must reach the injector as a killed
//! process carrying the message, not as a timeout and not as a running game.
//!
//! The hop under test: `decode_config` refuses the version, the shim DLL's
//! `DllMain` writes `bootstrap-failed:<message>` to the ready file, and the
//! injector reads it while the primary thread is still suspended and
//! terminates the process. Mirrors `fuse_not_configured.rs`, whose config is
//! valid and whose failure is the missing ring.
//!
//! Single-test binary: `run_target_with_shim` mutates process-global env vars.
#![allow(unsafe_code)]
mod common;

use std::time::Duration;
use vfs_inject::{run_target_with_shim, InjectError, RunConfig};

#[test]
fn a_config_version_mismatch_kills_the_process_with_the_message() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-cfg-refused-{pid}"));
    let root = base.join("gameroot");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&root).unwrap();

    // A well-formed config from "another build": same shape, version 3.
    let mut config_bytes = vfs_shim::encode_config(root.to_str().unwrap());
    config_bytes[4..8].copy_from_slice(&3u32.to_le_bytes());
    let config_path = base.join("shim.cfg");
    std::fs::write(&config_path, &config_bytes).unwrap();

    let ready_path = base.join("ready.flag");
    let output_path = base.join("probe-out.bin");
    let probe = env!("CARGO_BIN_EXE_vfs-probe").to_string();
    let dll = common::locate_shim();

    let result = run_target_with_shim(RunConfig {
        target_exe: probe,
        current_dir: None,
        args: vec![
            root.join("does-not-matter.bin")
                .to_str()
                .unwrap()
                .to_string(),
            output_path.to_str().unwrap().to_string(),
        ],
        dll_path: dll,
        config_path: config_path.to_str().unwrap().to_string(),
        ready_path: ready_path.to_str().unwrap().to_string(),
        ready_timeout: Duration::from_secs(30),
        detach: false,
    });

    match result {
        Err(InjectError::Bootstrap(msg)) => {
            assert!(
                msg.contains("version 3") && msg.contains("version 2"),
                "expected the message to name both config versions, got: {msg}"
            );
        }
        other => panic!(
            "expected Err(InjectError::Bootstrap(_)) for a config version mismatch; got {other:?}"
        ),
    }

    // The outcome that matters: the probe writes its output file as its one
    // act, so its absence proves the process never ran.
    assert!(
        !output_path.exists(),
        "probe output exists at {output_path:?}: the process ran after a refused config"
    );
    // Not merely unrun: gone. A parked process satisfies the check above.
    common::assert_no_child_processes();
    let _ = std::fs::remove_dir_all(&base);
}

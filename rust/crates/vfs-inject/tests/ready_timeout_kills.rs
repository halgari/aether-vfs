//! When the shim never writes a ready file, the injector must kill the parked
//! process, not release it. Releasing would let the game run with nothing
//! virtualising it, its writes reaching the real disk.
//!
//! The "shim" here is `vproxy.dll`, a fixture DLL with no `DllMain`: the
//! remote `LoadLibrary` succeeds and nothing ever touches the ready file,
//! which is what a shim that dies or hangs before bootstrap looks like.
//!
//! Single-test binary: `run_target_with_shim` mutates process-global env vars.
mod common;

use std::time::Duration;
use vfs_inject::{run_target_with_shim, InjectError, RunConfig};

#[test]
fn a_shim_that_never_reports_ready_gets_the_process_killed() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-ready-timeout-{pid}"));
    let root = base.join("gameroot");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&root).unwrap();

    let config_path = base.join("shim.cfg");
    std::fs::write(&config_path, vfs_shim::encode_config(root.to_str().unwrap())).unwrap();
    let ready_path = base.join("ready.flag");
    let output_path = base.join("probe-out.bin");
    let probe = env!("CARGO_BIN_EXE_vfs-probe").to_string();
    let (_real_shim, payload) = common::locate_shim_and_payload();
    let silent_shim = common::locate_artifact("vproxy.dll");

    let result = run_target_with_shim(RunConfig {
        target_exe: probe,
        current_dir: None,
        args: vec![
            root.join("does-not-matter.bin").to_str().unwrap().to_string(),
            output_path.to_str().unwrap().to_string(),
        ],
        dll_path: silent_shim,
        config_path: config_path.to_str().unwrap().to_string(),
        ready_path: ready_path.to_str().unwrap().to_string(),
        ready_timeout: Duration::from_secs(3),
        payload_path: payload,
        preinit_redirects: vec![],
        detach: false,
    });

    assert!(
        matches!(result, Err(InjectError::Timeout)),
        "expected Err(InjectError::Timeout) from a shim that never wrote the ready file; got {result:?}"
    );
    assert!(!ready_path.exists(), "the silent shim must not have written a ready file");
    // Before the fix the gate was released on timeout and the probe ran to
    // completion and wrote this file — but after `run_target_with_shim` had
    // already returned, so give a released process time to get there.
    for _ in 0..40 {
        if output_path.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !output_path.exists(),
        "probe output exists at {output_path:?}: the process was released after the ready timeout"
    );
    // Not merely unrun: gone. A parked process satisfies the check above.
    common::assert_no_child_processes();
    let _ = std::fs::remove_dir_all(&base);
}

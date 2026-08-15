//! Task 2 (gate 3): when the shim's FUSE client fails to attach to a
//! configured director, the launch must abort loudly — not log the failure
//! and let the game run fully un-virtualized, which is what `fuse_client`
//! returning `None` used to mean and nothing reported.
//!
//! `run_target_with_shim` is the exact function `vfs_director::Session::launch`
//! calls (it only wraps the error into a `String`), so exercising it here
//! proves the production launch path aborts, not just some lower-level detail.
mod common;

use std::time::Duration;
use vfs_inject::{run_target_with_shim, InjectError, RunConfig};

/// Forcing `try_init_from_env` to fail (via the test-only
/// `VFS_TEST_FUSE_INIT_FAIL` switch — see its doc comment in `vfs-env`) must
/// make the launch return `Err(InjectError::FuseInit(_))`, and the target
/// process must never have run: it is killed while still parked behind the
/// pre-init spin gate, before `RtlUserThreadStart`, before a single byte of
/// game code executes.
///
/// Before this task, `bootstrap_from_config_path_with_payload` discarded this
/// exact error (`let _ = fuse_client::try_init_from_env();`), hooks installed
/// anyway over an empty local snapshot, the ready file was written "ready"
/// regardless, and the launch returned `Ok` with the process running fully
/// un-virtualized. Nothing anywhere reported it.
#[test]
fn fuse_init_failure_aborts_the_launch() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-fuse-init-fail-{pid}"));
    let root = base.join("gameroot");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&root).unwrap();

    // Content is irrelevant: the decisive assertion is that the process never
    // runs at all, so it never gets far enough to read anything.
    let snapshot = {
        use vfs_core::{build, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![],
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

    // This is a single-test binary (one process), so mutating process env
    // around this one call cannot race another test the way it would in a
    // multi-test file (see `vfs-directord`'s `LAUNCH_LOCK` for that case).
    std::env::set_var(vfs_env::TEST_FUSE_INIT_FAIL, "1");
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
        ready_timeout: Duration::from_secs(10),
        payload_path: payload,
        preinit_redirects: vec![],
        detach: false,
    });
    std::env::remove_var(vfs_env::TEST_FUSE_INIT_FAIL);

    match result {
        Err(InjectError::FuseInit(msg)) => {
            assert!(
                msg.contains("VFS_TEST_FUSE_INIT_FAIL"),
                "expected the forced-failure reason in the error message, got: {msg}"
            );
        }
        other => panic!(
            "expected Err(InjectError::FuseInit(_)) when FUSE init is forced to fail \
             (a director was configured and its client could not attach); \
             got {other:?} instead — the launch must fail on its own, distinct terms, \
             not run the process anyway or time out generically"
        ),
    }

    // The outcome that actually matters, not a log line: vfs-probe writes its
    // output file unconditionally as its very first and only externally
    // visible act (see its `main`). Its absence proves the process was killed
    // before it ever ran — not merely that some message string matched.
    assert!(
        !output_path.exists(),
        "probe output file exists at {output_path:?} — the un-virtualized process \
         was allowed to run to completion, which is exactly the bypass this test exists \
         to close"
    );
}

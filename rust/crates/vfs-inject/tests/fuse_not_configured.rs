//! Task 3 (gate 3): a launch that names no director ring at all
//! (`VFS_RING_SECTION` unset — `FuseInitError::NotConfigured`) must abort just
//! like one whose named ring failed to attach — not run the game completely
//! un-virtualised while looking like a normal launch. That standalone mode is
//! retired: see `bootstrap.rs`'s doc comment on `bootstrap_from_config_path_with_payload`.
//!
//! Single-test binary, like `fuse_init_failure.rs`: `run_target_with_shim`
//! mutates process-global env vars (`SHIM_CONFIG`/`SHIM_READY`/…) itself, so a
//! second test in the same binary could race it.
mod common;

use std::time::Duration;
use vfs_inject::{run_target_with_shim, InjectError, RunConfig};

/// No director, no ring — `VFS_RING_SECTION` is simply never set here. Before
/// this task, `bootstrap_from_config_path_with_payload` matched
/// `FuseInitError::NotConfigured` alongside `Ok(())` and swallowed it: hooks
/// installed anyway over the local snapshot, the ready file was written
/// "ready" regardless, and the launch returned `Ok` with the process fully
/// un-virtualised. That is exactly the failure mode this whole programme
/// exists to eliminate — a game that can run completely un-virtualised while
/// appearing to work.
#[test]
fn no_ring_configured_aborts_the_launch() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-not-configured-{pid}"));
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

    // Make sure the harness itself did not inherit a ring from some other
    // process — this test's whole point is that none is configured.
    assert!(
        std::env::var_os(vfs_env::RING_SECTION).is_none(),
        "test environment must not already have {} set",
        vfs_env::RING_SECTION
    );

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

    match result {
        Err(InjectError::FuseInit(msg)) => {
            assert!(
                msg.to_ascii_lowercase().contains("ring"),
                "expected a message naming the missing ring configuration, got: {msg}"
            );
        }
        other => panic!(
            "expected Err(InjectError::FuseInit(_)) when no director ring is configured at all; \
             got {other:?} instead — standalone (no-director) launches must fail on their own, \
             distinct terms, not run the process un-virtualised or time out generically"
        ),
    }

    // The outcome that actually matters, not a log line: vfs-probe writes its
    // output file unconditionally as its very first and only externally
    // visible act. Its absence proves the process was killed before it ever
    // ran — not merely that some message string matched.
    assert!(
        !output_path.exists(),
        "probe output file exists at {output_path:?} — the un-virtualized process \
         was allowed to run to completion, which is exactly the bypass this test exists \
         to close"
    );
}

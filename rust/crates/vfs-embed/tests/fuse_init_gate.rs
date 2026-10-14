//! The FUSE-init gate at the actual production entrypoint: `Session::launch` is
//! what a real host calls, and it must return an error — not run the process
//! un-virtualized — when the shim's FUSE client fails to attach to this
//! session's own, correctly-configured director.
//!
//! `Session::launch` delegates straight to `vfs_inject::run_target_with_shim`
//! (see its own doc comment), which is exercised more directly, and against
//! more scenarios, by `vfs-inject`'s `fuse_init_failure` test. This test
//! exists to prove the same guarantee holds at the layer a caller actually
//! uses, through a real `serve()` + `launch()` session rather than a
//! hand-built `RunConfig`.
//!
//! **Whole target is Windows-only.** It needs a real `CreateProcess` and DLL
//! injection by `vfs-inject`, and the shim and `vfs-probe` artifacts that
//! `ensure_inject_artifacts` builds. (The Proton path's launch failure modes
//! are in `proton_launch.rs` and `proton_fake_runtime.rs`.) Gated at the crate
//! root of the target rather than per item, since there is only the one test.
#![cfg(windows)]

use std::sync::Arc;

use vfs_embed::{DiskProvider, LaunchOpts, Session};

mod support;
use support::artifacts::{ensure_inject_artifacts, locate_artifact};

fn tmp(name: &str) -> std::path::PathBuf {
    let d = vfs_testkit::scratch_path(&format!("vfs-fuse-gate-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Forcing FUSE init to fail (`VFS_TEST_FUSE_INIT_FAIL`, scoped to just this
/// child via `LaunchOpts.env`) against a session with a real, working
/// director ring must make `launch()` return `Err`, and the target — which
/// unconditionally writes its output file as its one and only act — must
/// never have run.
///
/// Before this task this returned `Ok`: `vfs_shim::director::try_init_from_env`'s
/// error was discarded, hooks installed anyway with nothing to serve from, the ready file was written regardless, and the launched
/// process ran to completion fully un-virtualized with nothing anywhere
/// reporting it.
#[test]
fn launch_returns_err_when_fuse_client_fails_to_attach() {
    ensure_inject_artifacts();

    let content_dir = tmp("content");
    let state_dir = tmp("state");
    std::fs::write(content_dir.join("hello.txt"), b"hello").unwrap();

    let mut s = Session::new();
    s.set_root(&content_dir);
    s.set_state_dir(&state_dir);
    s.mount("", Arc::new(DiskProvider::new(&content_dir)))
        .unwrap();
    s.serve().expect("serve");

    let probe = locate_artifact("vfs-probe.exe");
    let virtual_path = content_dir.join("hello.txt");
    let output_path = state_dir.join("probe-out.bin");
    let _ = std::fs::remove_file(&output_path);

    let mut env = std::collections::BTreeMap::new();
    env.insert(vfs_env::TEST_FUSE_INIT_FAIL.to_string(), "1".to_string());

    let opts = LaunchOpts {
        image: probe.to_string_lossy().into_owned(),
        args: vec![
            virtual_path.to_string_lossy().into_owned(),
            output_path.to_string_lossy().into_owned(),
        ],
        wait: true,
        shim_dll: None,
        env,
        ..Default::default()
    };

    let result = s.launch(&opts);

    match &result {
        Err(msg) => {
            assert!(
                msg.to_ascii_lowercase().contains("fuse"),
                "expected a FUSE-specific error message, got: {msg}"
            );
        }
        Ok(code) => panic!(
            "expected launch() to return Err when the FUSE client is forced to fail to \
             attach; got Ok({code}) instead — the process ran un-virtualized"
        ),
    }

    // The decisive outcome, not a log line: vfs-probe writes its output file
    // unconditionally as its one and only act (see `vfs-inject`'s
    // `src/bin/vfs-probe.rs`). Its absence proves the process was killed
    // before it ever ran — `wait: true` means `launch` already blocked for as
    // long as that would have taken, so this is not a race.
    assert!(
        !output_path.exists(),
        "probe output file exists at {output_path:?} — the un-virtualized process \
         was allowed to run to completion"
    );

    s.stop_serve();
    let _ = std::fs::remove_dir_all(&content_dir);
    let _ = std::fs::remove_dir_all(&state_dir);
}

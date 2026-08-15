//! Task 2 (gate 3), at the actual production entrypoint: `Session::launch` is
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

use std::sync::Arc;

use vfs_director::{DiskProvider, LaunchOpts, Session};

fn profile_dir() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let dir = exe.parent().unwrap();
    if dir.file_name().and_then(|s| s.to_str()) == Some("deps") {
        dir.parent().unwrap().to_path_buf()
    } else {
        dir.to_path_buf()
    }
}

fn locate_artifact(name: &str) -> std::path::PathBuf {
    let profile = profile_dir();
    for cand in [profile.join(name), profile.join("deps").join(name)] {
        if cand.is_file() {
            return cand;
        }
    }
    panic!("{name} not found near {profile:?} after ensure_fixtures()");
}

/// Build the shim DLL, the (separate-workspace) payload DLL, and `vfs-probe`
/// (a `vfs-inject` test target that unconditionally writes its output file —
/// the decisive "did this process actually run" signal below) once per test
/// process, then co-locate them beside the test binary so `Session::launch`'s
/// own DLL search (near `current_exe()`) finds them — the same convention
/// `vfs-inject`'s and `vfs-directord`'s test harnesses use.
fn ensure_fixtures() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let workspace = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .canonicalize()
            .expect("workspace root");

        let status = std::process::Command::new(&cargo)
            .current_dir(&workspace)
            .args([
                "build", "-p", "vfs-shim-dll", "-p", "vfs-inject", "--bin", "vfs-probe", "--quiet",
            ])
            .status()
            .expect("spawn cargo to build shim + vfs-probe");
        assert!(status.success(), "shim/vfs-probe build failed: {status}");

        let target_dir = workspace.join("target");
        let status = std::process::Command::new(&cargo)
            .current_dir(&workspace)
            .env("CARGO_TARGET_DIR", &target_dir)
            .args([
                "build",
                "--manifest-path",
                "crates/vfs-payload/Cargo.toml",
                "--quiet",
            ])
            .status()
            .expect("spawn cargo to build vfs-payload");
        assert!(status.success(), "vfs-payload build failed: {status}");

        let profile = profile_dir();
        for name in ["vfs_shim_dll.dll", "vfs_payload.dll", "vfs-probe.exe"] {
            let dest = profile.join(name);
            if dest.is_file() {
                continue;
            }
            let src = profile.join("deps").join(name);
            if src.is_file() {
                let _ = std::fs::copy(&src, &dest);
            }
        }
    });
}

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("vfs-fuse-gate-{}-{name}", std::process::id()));
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
/// Before this task this returned `Ok`: `fuse_client::try_init_from_env`'s
/// error was discarded, hooks installed anyway over an empty local snapshot
/// (`Session::serve` deliberately ships one — real content only ever came
/// from the ring), the ready file was written regardless, and the launched
/// process ran to completion fully un-virtualized with nothing anywhere
/// reporting it.
#[test]
fn launch_returns_err_when_fuse_client_fails_to_attach() {
    ensure_fixtures();

    let content_dir = tmp("content");
    let state_dir = tmp("state");
    std::fs::write(content_dir.join("hello.txt"), b"hello").unwrap();

    let mut s = Session::new();
    s.set_root(&content_dir);
    s.set_state_dir(&state_dir);
    s.mount("", Arc::new(DiskProvider::new(&content_dir))).unwrap();
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
        payload_dll: None,
        env,
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

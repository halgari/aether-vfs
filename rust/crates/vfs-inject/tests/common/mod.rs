//! Shared helpers for vfs-inject integration tests.
//!
//! Builds PE fixtures once per test process (nested `cargo` at **runtime** is
//! safe — the outer compile has finished). Co-locates payload beside the shim.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;

static FIXTURES: Once = Once::new();

/// Ensure dual-layer / static-import PE fixtures are built (once per process).
pub fn ensure_fixtures() {
    FIXTURES.call_once(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace = manifest
            .join("..")
            .join("..")
            .canonicalize()
            .expect("workspace root");
        let mut cmd = Command::new(&cargo);
        cmd.current_dir(&workspace).args([
            "build",
            "-p",
            "vfs-payload",
            "-p",
            "vfs-shim-dll",
            "-p",
            "vfs-fixture-vproxy",
            "-p",
            "vfs-fixture-staticimp",
            "--quiet",
        ]);
        // Match the profile of the test binary when possible.
        if cfg!(debug_assertions) {
            // default dev/debug
        } else {
            cmd.arg("--release");
        }
        let status = cmd.status().expect("spawn cargo to build fixtures");
        assert!(
            status.success(),
            "fixture cargo build failed: {status}\n\
             packages: vfs-payload vfs-shim-dll vfs-fixture-vproxy vfs-fixture-staticimp"
        );
        // Co-locate under profile dir for child inject + locate.
        colocate_profile_artifacts();
    });
}

fn profile_dir_from_test_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // …/target/debug/deps/testname-hash.exe → debug/
    let dir = exe.parent()?;
    if dir.file_name().and_then(|s| s.to_str()) == Some("deps") {
        return dir.parent().map(|p| p.to_path_buf());
    }
    Some(dir.to_path_buf())
}

fn colocate_profile_artifacts() {
    let Some(profile_dir) = profile_dir_from_test_exe() else {
        return;
    };
    for name in [
        "vfs_payload.dll",
        "vfs_shim_dll.dll",
        "vproxy.dll",
        "vfs-staticimp.exe",
    ] {
        let dest = profile_dir.join(name);
        if dest.is_file() {
            continue;
        }
        let src = profile_dir.join("deps").join(name);
        if src.is_file() {
            let _ = std::fs::copy(&src, &dest);
        }
    }
}

/// Locate a built PE next to the test binary (profile dir or deps/).
pub fn locate_artifact(name: &str) -> String {
    ensure_fixtures();
    let exe = std::env::current_exe().expect("current_exe");
    if let Some(p) = find_near(&exe, name) {
        return p.to_string_lossy().into_owned();
    }
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        let root = PathBuf::from(manifest)
            .join("..")
            .join("..")
            .join("target");
        for profile in ["debug", "release"] {
            let base = root.join(profile);
            for cand in [base.join(name), base.join("deps").join(name)] {
                if cand.is_file() {
                    return cand.to_string_lossy().into_owned();
                }
            }
        }
    }
    panic!(
        "{name} not found after fixture build near {:?}.",
        exe.parent()
    );
}

fn find_near(reference: &Path, name: &str) -> Option<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = reference.parent() {
        dirs.push(d.to_path_buf());
        dirs.push(d.join("deps"));
        if let Some(p) = d.parent() {
            dirs.push(p.to_path_buf());
            dirs.push(p.join("deps"));
        }
    }
    for d in dirs {
        let c = d.join(name);
        if c.is_file() {
            return Some(c);
        }
    }
    None
}

/// Shim + payload paths, with payload co-located beside the shim when possible.
#[allow(dead_code)] // used by some test binaries, not all
pub fn locate_shim_and_payload() -> (String, String) {
    ensure_fixtures();
    let dll = locate_artifact("vfs_shim_dll.dll");
    let payload = locate_artifact("vfs_payload.dll");
    let resolved =
        vfs_inject::ensure_payload_beside_shim(&dll, Some(&payload)).unwrap_or(payload);
    (dll, resolved)
}

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
        let target_dir = workspace.join("target");

        // Main-workspace fixtures.
        let mut cmd = Command::new(&cargo);
        cmd.current_dir(&workspace).args([
            "build",
            "-p",
            "vfs-shim-dll",
            "-p",
            "vfs-fixture-vproxy",
            "-p",
            "vfs-fixture-staticimp",
            "--quiet",
        ]);
        if !cfg!(debug_assertions) {
            cmd.arg("--release");
        }
        let status = cmd.status().expect("spawn cargo to build fixtures");
        assert!(status.success(), "fixture cargo build failed: {status}");

        // vfs-payload lives in its own workspace (panic = "abort"). Build it
        // into the same target dir so `locate_artifact` finds it unchanged.
        let mut pay = Command::new(&cargo);
        pay.current_dir(&workspace)
            .env("CARGO_TARGET_DIR", &target_dir)
            .args([
                "build",
                "--manifest-path",
                "crates/vfs-payload/Cargo.toml",
                "--quiet",
            ]);
        if !cfg!(debug_assertions) {
            pay.arg("--release");
        }
        let status = pay.status().expect("spawn cargo to build vfs-payload");
        assert!(status.success(), "vfs-payload cargo build failed: {status}");
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
        let root = PathBuf::from(manifest).join("..").join("..").join("target");
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
    let resolved = vfs_inject::ensure_payload_beside_shim(&dll, Some(&payload)).unwrap_or(payload);
    (dll, resolved)
}

/// Wait up to ~2 s for this process to have no child processes left, and
/// panic naming the survivors if it still has some.
///
/// A target that was killed stays in the process list for a moment while the
/// kernel tears it down, so this polls. It is the assertion behind every "the
/// launch killed the target" test: a process that was merely left parked (not
/// running, not dead) satisfies "the output file never appeared" and still
/// leaks, which this catches.
#[allow(dead_code)] // used by some test binaries, not all
pub fn assert_no_child_processes() {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let me = std::process::id();
    let children = || -> Vec<(u32, String)> {
        let mut found = Vec::new();
        // SAFETY: a process snapshot walked with a correctly sized entry; the
        // handle is closed before returning.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return found;
            }
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut ok = Process32FirstW(snap, &mut e);
            while ok != 0 {
                if e.th32ParentProcessID == me {
                    let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                    found.push((e.th32ProcessID, String::from_utf16_lossy(&e.szExeFile[..n])));
                }
                ok = Process32NextW(snap, &mut e);
            }
            CloseHandle(snap);
        }
        found
    };
    let mut left = children();
    for _ in 0..40 {
        if left.is_empty() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        left = children();
    }
    assert!(left.is_empty(), "child processes still alive after the launch failed: {left:?}");
}

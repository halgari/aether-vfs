//! Best-effort co-location of PE artifacts under the profile dir (copy only).
//! Fixture *builds* happen at test runtime (see `tests/common`) — nested
//! `cargo build` from this script deadlocks on the package lock.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    colocate_only();
}

fn profile_dir() -> PathBuf {
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".into());
    if let Ok(td) = std::env::var("CARGO_TARGET_DIR") {
        let td = PathBuf::from(td);
        if td.ends_with(&profile) {
            return td;
        }
        return td.join(&profile);
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default());
    out.ancestors()
        .nth(3)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("target").join(profile))
}

fn colocate_only() {
    let profile_dir = profile_dir();
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

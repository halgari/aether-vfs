//! Static-import redirects come from the **config file** (not only RunConfig),
//! so director and children share one source of truth.
//!
//! `config_static_import_via_dual_layer` was removed here for gate 3, Task 3
//! ("retire standalone mode"): it drove a full `run_target_with_shim` launch
//! with no `VFS_RING_SECTION`, so it aborts on `FuseInitError::NotConfigured`
//! before the early-payload static-import redirect it wanted to prove ever
//! gets a chance to run. That is a genuine, unmitigated coverage gap, not
//! merely a retired assertion: static-import (PE import table) redirection
//! via dual-layer preinit is a real, still-supported mechanism unrelated to
//! the FUSE virtualisation bypass this gate closes, and nothing today proves
//! it end-to-end through a real director — `vfs_director::Session::launch`
//! (`crates/vfs-director/src/session.rs`) hardcodes `preinit_redirects:
//! vec![]` and has no config-file static-import plumbing either. Restoring
//! this coverage needs either a director-mediated static-import path (a
//! `vfs-director` feature change) or a minimal in-process test-only ring
//! (e.g. `vfs_director::ipc::IpcServe` as a dev-dependency here, which Cargo
//! permits despite the reverse production dependency) — both are follow-up
//! decisions beyond this task, not something to force through here.
//! `merge_preinit_loads_config_statics` below still directly covers the
//! config-file parsing this test also exercised, and
//! `preinit_only_still_accepts_explicit_redirects` still proves the
//! preinit-only redirect path end-to-end (it never calls
//! `bootstrap_from_config_path` / `run_target_with_shim` at all).
mod common;

use vfs_inject::{run_target_with_preinit, PreinitConfig};
use vfs_shim::{encode_config_full, StaticImport};

#[test]
fn merge_preinit_loads_config_statics() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-merge-{pid}"));
    std::fs::create_dir_all(&base).unwrap();
    let bak = base.join("real.dll");
    std::fs::write(&bak, b"x").unwrap();
    let cfg = encode_config_full(
        r"C:\Game",
        "",
        &[StaticImport {
            dll_name: "d3d11.dll".into(),
            backing_path: bak.to_str().unwrap().to_string(),
        }],
        &[0u8, 1, 2],
    );
    let path = base.join("c.cfg");
    std::fs::write(&path, &cfg).unwrap();
    let merged = vfs_inject::merge_preinit_redirects(path.to_str().unwrap(), &[]);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].suffix, "d3d11.dll");
    assert!(merged[0].backing_nt.starts_with(r"\??\"));
    assert_eq!(merged[0].backing_size, 1);
}

#[test]
fn preinit_only_still_accepts_explicit_redirects() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-pre-{pid}"));
    let app_dir = base.join("app");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&app_dir).unwrap();
    let backing = base.join("b.dll");
    std::fs::copy(common::locate_artifact("vproxy.dll"), &backing).unwrap();
    let size = std::fs::metadata(&backing).unwrap().len();
    let tgt = app_dir.join("vfs-staticimp.exe");
    std::fs::copy(common::locate_artifact("vfs-staticimp.exe"), &tgt).unwrap();
    let result = app_dir.join("r.txt");
    let exit = run_target_with_preinit(PreinitConfig {
        target_exe: tgt.to_str().unwrap().to_string(),
        args: vec![result.to_str().unwrap().to_string()],
        current_dir: Some(app_dir.to_str().unwrap().to_string()),
        payload_path: common::locate_artifact("vfs_payload.dll"),
        redirects: vec![vfs_inject::PreinitRedirect {
            suffix: "vproxy.dll".into(),
            backing_nt: format!(r"\??\{}", backing.to_string_lossy()),
            backing_size: size,
        }],
    })
    .expect("preinit");
    assert_eq!(exit, 0);
    assert_eq!(
        std::fs::read_to_string(&result).unwrap().trim(),
        "vproxy_value=4242"
    );
}

//! Prove pre-init reflective-map + RIP-redirect virtualizes the target EXE's
//! own static PE import of vproxy.dll with zero files in the app directory.
mod common;

use vfs_inject::{run_target_with_preinit, PreinitConfig, PreinitRedirect};

#[test]
fn preinit_virtualizes_exe_static_import() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-staticimp-{pid}"));
    let app_dir = base.join("app");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&app_dir).unwrap();

    let backing_src = common::locate_artifact("vproxy.dll");
    let backing = base.join("backing_vproxy.dll");
    std::fs::copy(&backing_src, &backing).unwrap();
    let backing_size = std::fs::metadata(&backing).unwrap().len();
    let backing_nt = format!(r"\??\{}", backing.to_string_lossy());

    let tgt_src = common::locate_artifact("vfs-staticimp.exe");
    let tgt = app_dir.join("vfs-staticimp.exe");
    std::fs::copy(&tgt_src, &tgt).unwrap();
    assert!(
        !app_dir.join("vproxy.dll").exists(),
        "app dir must not contain vproxy.dll on disk"
    );

    let result_path = app_dir.join("result.txt");
    let _ = std::fs::remove_file(&result_path);

    let payload = common::locate_artifact("vfs_payload.dll");

    let exit = run_target_with_preinit(PreinitConfig {
        target_exe: tgt.to_str().unwrap().to_string(),
        args: vec![result_path.to_str().unwrap().to_string()],
        current_dir: Some(app_dir.to_str().unwrap().to_string()),
        payload_path: payload,
        redirects: vec![PreinitRedirect {
            suffix: "vproxy.dll".into(),
            backing_nt,
            backing_size,
        }],
    })
    .expect("run_target_with_preinit");

    assert_eq!(exit, 0, "staticimp exit code");
    let out = std::fs::read_to_string(&result_path).expect("result.txt");
    assert_eq!(
        out.trim(),
        "vproxy_value=4242",
        "EXE static import did not resolve through pre-init redirect"
    );
    assert!(
        !app_dir.join("vproxy.dll").exists(),
        "must not write proxy DLL into the app directory"
    );
}

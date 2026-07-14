//! Cross-process acceptance: build a managed VFS layout, launch the acceptance
//! exerciser injected with the shim, and assert every feature check passes.
use std::time::Duration;
use vfs_inject::{run_target_with_shim, RunConfig};

fn locate_dll() -> String {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().to_path_buf();
    for cand in [dir.join("vfs_shim_dll.dll"), dir.parent().unwrap().join("vfs_shim_dll.dll")] {
        if cand.exists() {
            return cand.to_str().unwrap().to_string();
        }
    }
    panic!("vfs_shim_dll.dll not found near {dir:?}");
}

#[test]
fn injected_shim_passes_full_acceptance_suite() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-accept-{pid}"));
    let root = base.join("gameroot");
    let mods = base.join("mods"); // backing files, OUTSIDE the root
    let overlay = base.join("overlay"); // write overlay, OUTSIDE the root
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&mods).unwrap();
    std::fs::create_dir_all(&overlay).unwrap();

    // Real on-disk contents under the root.
    std::fs::write(root.join("override.txt"), b"REAL-OVERRIDE").unwrap();
    std::fs::write(root.join("real_only.txt"), b"REAL-ONLY-BYTES").unwrap();
    std::fs::write(root.join("deleted.txt"), b"SHOULD-BE-HIDDEN").unwrap();
    std::fs::write(root.join("del_target.txt"), b"DELETE-ME-RUNTIME").unwrap();
    std::fs::create_dir_all(root.join("real_dir")).unwrap();

    // Backing files for the mod add + override + copy-on-write target.
    let added_backing = mods.join("added_backing.dat");
    std::fs::write(&added_backing, b"MOD-ADDED-BYTES").unwrap();
    let override_backing = mods.join("override_backing.dat");
    std::fs::write(&override_backing, b"MOD-OVERRIDE-BYTES").unwrap();
    let cow_backing = mods.join("cow_backing.dat");
    std::fs::write(&cow_backing, b"COW-ORIG").unwrap();

    // A real, loadable system DLL stands in for a mod plugin DLL.
    let plugin_backing = r"C:\Windows\System32\version.dll";
    let plugin_size = std::fs::metadata(plugin_backing).unwrap().len();

    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let e = |vpath: &str, kind: EntryKind, source: &str, size: u64| InputEntry {
            vpath: vpath.into(),
            kind,
            source: source.into(),
            size,
            mtime: 0,
        };
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![
                e("mod_added.txt", EntryKind::File, added_backing.to_str().unwrap(), 15),
                e("override.txt", EntryKind::File, override_backing.to_str().unwrap(), 18),
                e("deleted.txt", EntryKind::Tombstone, "", 0),
                e("virtual_dir", EntryKind::Dir, "", 0),
                e("plugin.dll", EntryKind::File, plugin_backing, plugin_size),
                e("cow_target.esp", EntryKind::File, cow_backing.to_str().unwrap(), 8),
            ],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };

    let config_bytes = vfs_shim::encode_config_with_overlay(
        root.to_str().unwrap(),
        overlay.to_str().unwrap(),
        &snapshot,
    );
    let config_path = base.join("shim.cfg");
    std::fs::write(&config_path, &config_bytes).unwrap();

    let ready_path = base.join("ready.flag");
    let _ = std::fs::remove_file(&ready_path);
    let report_path = base.join("report.txt");
    let _ = std::fs::remove_file(&report_path);

    let exerciser = env!("CARGO_BIN_EXE_vfs-acceptance").to_string();
    let dll = locate_dll();

    let exit = run_target_with_shim(RunConfig {
        target_exe: exerciser,
        args: vec![root.to_str().unwrap().to_string(), report_path.to_str().unwrap().to_string()],
        dll_path: dll,
        config_path: config_path.to_str().unwrap().to_string(),
        ready_path: ready_path.to_str().unwrap().to_string(),
        ready_timeout: Duration::from_secs(15),
    })
    .expect("run_target_with_shim");

    let report = std::fs::read_to_string(&report_path).unwrap_or_default();
    // Surface the full report on failure for diagnosis.
    assert_eq!(exit, 0, "exerciser reported failures:\n{report}");
    assert!(!report.is_empty(), "exerciser wrote no report");
    for line in report.lines() {
        assert!(line.ends_with("=PASS"), "check failed: {line}\nfull report:\n{report}");
    }
    // Guard against silently skipping checks.
    assert_eq!(report.lines().count(), 14, "expected 14 checks:\n{report}");
}

//! Runs in its own process: a directory listing under a managed root is exactly what the
//! director serves. Real, on-disk files beneath the root are never drained into it.
//!
//! This was `read_dir_without_a_director_is_denied_outright`, which installed the shim with no
//! director and showed the root could not even be opened. Task C8 removed the no-director
//! configuration (bootstrap never installs a detour without a director), and the claim that
//! survives it is the one this file always existed for: a listing is only ever authoritative
//! when a director backs it. So the real directory holds files the director does not serve,
//! the director serves one the real directory does not hold, and the listing must be the
//! director's alone (`hook/dirquery.rs::serve_dir_query`).
use crate::fakedirector;

use fakedirector::{Fake, ReadStyle};
use vfs_shim::{Engine, install};

#[test]
fn read_dir_lists_only_what_the_director_serves() {
    isolate!();
    let pid = std::process::id();
    let root = std::env::temp_dir().join(format!("vfs-shim-direnum-{pid}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    // Real on-disk contents of the enumerated directory, none of them served.
    std::fs::write(root.join("real_a.txt"), b"a").unwrap();
    std::fs::write(root.join("real_b.txt"), b"b").unwrap();
    std::fs::create_dir_all(root.join("realdir")).unwrap();

    fakedirector::install(
        &root,
        Fake::new()
            .with("added.esp", vec![0u8; 10], ReadStyle::Whole)
            .with_dir("served_dir"),
        0,
    );
    let snapshot = vfs_shared::bridge::flatten(
        &vfs_core::build(vec![vfs_core::Layer {
            id: vfs_core::LayerId(0),
            entries: Vec::new(),
        }])
        .unwrap(),
    );
    let _guard = install(Engine::new(root.to_str().unwrap(), snapshot).unwrap()).expect("install");

    let mut names: Vec<String> = std::fs::read_dir(&root)
        .expect("the managed root must open and list through the director")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["added.esp", "served_dir"],
        "the listing must be the director's: no real, unserved file may appear in it"
    );
}

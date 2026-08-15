//! The write seal with **no overlay configured** (gate 4, Task 5).
//!
//! `write_seal.rs` proves the refusal in the live-shaped configuration, where
//! a fall-through would have been captured by the shim-local overlay. This
//! binary removes the overlay, which is the configuration where the same
//! fall-through is not a misplacement but a genuine escape:
//! `Engine::decide_open` answers `PassThrough` for a write when there is no
//! overlay, so before this task the create was carried out by the real
//! `NtCreateFile` and a real file appeared **physically under the managed
//! root** — the one thing the root's whole contract says cannot happen.
//!
//! It is a separate binary because `ENGINE` is a `OnceLock`: one engine per
//! process, so "with an overlay" and "without one" cannot be the same test
//! run.

mod fakedirector;

use vfs_shim::{install, Engine};

#[test]
fn a_refused_write_creates_nothing_on_the_real_filesystem_under_the_root() {
    let base = std::env::temp_dir().join(format!("vfs-write-seal-noov-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    // The physical directory exists and is writable by this process — so if
    // the create below reaches the real filesystem, it *succeeds*, which is
    // exactly the escape being ruled out. A non-existent directory would make
    // this test pass for the wrong reason.
    std::fs::create_dir_all(root.join("data")).unwrap();

    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "unrelated.txt".into(),
                kind: EntryKind::File,
                source: r"D:\nowhere\unrelated.txt".into(),
                size: 0,
                mtime: 0,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };

    // A graph with no writable mount at all: every create under the root is
    // refused with `ST_NOT_FOUND`.
    fakedirector::install(&root, fakedirector::Fake::new(), 0);

    let engine = Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let hooks = install(engine).expect("install");

    let escaped = root.join("data").join("escaped.bin");
    let result = std::fs::write(&escaped, b"content the provider graph never agreed to");

    // The real filesystem under the root is only observable with the detours
    // down — a hooked `exists()` asks the engine, which answers "no" for
    // anything the VFS does not serve, and would pass vacuously.
    drop(hooks);

    // The substantive claim first: whatever status the call returned, no file
    // may have appeared.
    assert!(
        !escaped.exists(),
        "a write the director refused was carried out by the real filesystem instead: \
         {escaped:?} now physically exists under the managed root. That file is invisible \
         to every reader (the root seals what the provider graph does not serve), so it is \
         both an escape and a silent data loss"
    );
    let err = result.expect_err("a write under a managed root that no provider serves must fail");
    assert_eq!(
        err.raw_os_error(),
        Some(3), // ERROR_PATH_NOT_FOUND
        "expected ERROR_PATH_NOT_FOUND from the refused create, got {err:?}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

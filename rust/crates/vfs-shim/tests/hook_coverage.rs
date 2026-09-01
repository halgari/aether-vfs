//! Single-test binary: **every hook this build knows about is live.**
//!
//! `install` builds each detour with `make_detour`, which fails when
//! `GetProcAddress` finds no such export in ntdll, and records the name in
//! `SKIPPED_DETOURS` instead of failing the install. That tolerance exists for
//! Wine, whose ntdll omits a couple of exports Windows has — but on Windows
//! every one of them is present, so a non-empty `skipped_detours()` here means
//! a hook was silently passed over.
//!
//! Nothing asserted that until this test. It is the machine check behind the
//! "no Windows behaviour change" claim of the Wine-transport work: an
//! unhooked NT entry point does not error, it quietly serves the real
//! directory or the real file, which reads exactly like a mod list that is
//! simply empty — the failure mode this project has already spent a debugging
//! session on. A future `if let Ok(..)` added for Wine's benefit that also
//! disables a hook on Windows fails here rather than in a game.
//!
//! Its own test binary, not a second `#[test]` inside an existing one:
//! `install` patches process-global ntdll trampolines, so two tests installing
//! concurrently in one process would fight. Every hook test in this crate is a
//! single-test binary for that reason.

use vfs_shim::{install, skipped_detours, Engine};

#[test]
fn a_successful_install_skips_no_detour_on_windows() {
    let pid = std::process::id();
    let root = std::env::temp_dir().join(format!("vfs-shim-hookcov-{pid}"));
    let backing_dir = std::env::temp_dir().join(format!("vfs-shim-hookcov-backing-{pid}"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&backing_dir).unwrap();

    let backing = backing_dir.join("backing_blob.dat");
    std::fs::write(&backing, b"the-real-bytes").unwrap();

    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "mod.esp".into(),
                kind: EntryKind::File,
                source: backing.to_str().unwrap().into(),
                size: 14,
                mtime: 0,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };
    let engine = Engine::new(root.to_str().unwrap(), snapshot).unwrap();

    // Assert *after* a successful install: `SKIPPED_DETOURS` is only written by
    // `install`, so checking it before would pass vacuously.
    let guard = install(engine).expect("install");

    let skipped = skipped_detours();
    assert!(
        skipped.is_empty(),
        "every hook must be live on Windows; these were skipped because ntdll \
         had no such export (or the detour would not build): {skipped:?}"
    );

    // Prove the install is the real thing and not a no-op that trivially skips
    // nothing: the virtual path resolves through the engine, not through disk,
    // where `mod.esp` does not exist.
    assert_eq!(
        std::fs::read(root.join("mod.esp")).unwrap(),
        b"the-real-bytes",
        "hooks reported as installed must actually be intercepting"
    );

    drop(guard);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&backing_dir);
}

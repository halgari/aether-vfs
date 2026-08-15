//! Single-test binary: the same write path as `hook_write.rs`, one root over.
//!
//! `hook_write.rs` proves writes land in the overlay for root 0. This proves
//! the *root* survives the trip, which is a different claim and the one gate
//! 4 Task 3 turns on: every `Engine` entry point resolves the path's own
//! `RootId` and hands that id to the overlay, rather than the `RootId::DEFAULT`
//! every call site used to pass.
//!
//! Why a hooked test and not more unit tests in `engine.rs`: a defaulted root
//! is invisible to a unit test that only ever declares one root, and Task 2's
//! two defects were both live-only for exactly that reason. Here the real
//! `NtCreateFile`/`NtSetInformationFile` detours run, so the assertions cover
//! the actual chain — `create_hook` -> `decision_for` -> `Engine::decide_open`,
//! `qattr_hook` -> `Engine::overlay_state`, and
//! `set_information_hook` -> `Engine::whiteout`/`rename` — with a second root
//! declared. Every one of them would still compile, and every single-root test
//! would still pass, if any of those sites had kept `RootId::DEFAULT`.
//!
//! The load-bearing shape throughout is *the same relative path under both
//! roots*: `shared.txt` exists in both roots' overlay subtrees with different
//! bytes, so a root that got lost anywhere in the chain shows up as one root
//! reading, modifying, or deleting the other's file.
use std::io::Write;
use vfs_shim::{install, overlay_layer_dir, Engine};
use vfs_redirect::RootId;

#[test]
fn writes_under_a_second_root_stay_in_that_root_s_overlay() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-shim-write-2root-{pid}"));
    let _ = std::fs::remove_dir_all(&base);
    let root0 = base.join("root0");
    // A separate location, not nested under root 0 — the
    // `Documents\My Games\…` shape `skyrim-live` declares as root 1.
    let root1 = base.join("root1");
    let overlay = base.join("overlay");
    std::fs::create_dir_all(&root0).unwrap();
    std::fs::create_dir_all(&root1).unwrap();
    std::fs::create_dir_all(&overlay).unwrap();

    // Physical overlay subtrees, from the one function that defines the
    // naming scheme — not spelled out by hand here.
    let ovl0 = overlay_layer_dir(&overlay, RootId::DEFAULT);
    let ovl1 = overlay_layer_dir(&overlay, RootId(1));
    std::fs::create_dir_all(&ovl0).unwrap();
    std::fs::create_dir_all(&ovl1).unwrap();
    // Seeded into the overlay rather than written onto the real roots: since
    // gate 3 a bare on-disk file under a managed root is not visible through
    // the VFS at all (see `hook_write.rs`'s note and
    // `real_on_disk_file_under_root_not_in_snapshot_is_denied`).
    std::fs::write(ovl0.join("shared.txt"), b"ROOT0").unwrap();
    std::fs::write(ovl1.join("shared.txt"), b"ROOT1").unwrap();

    // One snapshot entry, and it is root 0's — the point being that root 1
    // must never be answered out of it (`Engine::decide` gates the snapshot
    // to root 0). Nothing in this test opens `snap-only.esp`; it is here so
    // the snapshot is a real tree rather than a degenerate empty one.
    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "snap-only.esp".into(),
                kind: EntryKind::File,
                source: base.join("nonexistent-backing.esp").to_string_lossy().as_ref().into(),
                size: 0,
                mtime: 0,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };
    let engine = Engine::with_roots_and_overlay(
        &[
            (RootId::DEFAULT, root0.to_string_lossy().into_owned()),
            (RootId(1), root1.to_string_lossy().into_owned()),
        ],
        overlay.to_str().unwrap(),
        snapshot,
    )
    .unwrap();
    let guard = install(engine).expect("install");

    // --- Reads resolve against the root the path actually lies under ---
    assert_eq!(std::fs::read(root0.join("shared.txt")).unwrap(), b"ROOT0");
    assert_eq!(
        std::fs::read(root1.join("shared.txt")).unwrap(),
        b"ROOT1",
        "root 1's read came back with root 0's bytes — the same relative path \
         resolved under the wrong root"
    );

    // --- Create under root 1 lands in root 1's overlay subtree ---
    let created = root1.join("created.txt");
    {
        let mut f = std::fs::File::create(&created).expect("create under root 1");
        f.write_all(b"NEW").unwrap();
    }
    assert_eq!(std::fs::read(&created).unwrap(), b"NEW", "readable back through root 1");
    assert_eq!(
        std::fs::read(ovl1.join("created.txt")).unwrap(),
        b"NEW",
        "a write under root 1 must land in root 1's overlay subtree"
    );
    assert!(
        !ovl0.join("created.txt").exists(),
        "root 1's write landed in ROOT 0's overlay subtree — the RootId was \
         defaulted somewhere between create_hook and Overlay::file_path"
    );

    // --- Copy-on-write modify under root 1 leaves root 0's file alone ---
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(root1.join("shared.txt"))
            .expect("append-open under root 1");
        f.write_all(b"!").unwrap();
    }
    assert_eq!(std::fs::read(root1.join("shared.txt")).unwrap(), b"ROOT1!");
    assert_eq!(
        std::fs::read(root0.join("shared.txt")).unwrap(),
        b"ROOT0",
        "modifying root 1's copy changed root 0's"
    );

    // --- Delete under root 1 whites out root 1's copy only ---
    std::fs::remove_file(root1.join("shared.txt")).expect("delete under root 1");
    assert!(std::fs::read(root1.join("shared.txt")).is_err(), "deleted under root 1");
    assert!(
        ovl1.join("shared.txt.__vfs_wh__").exists(),
        "whiteout marker must be written under root 1's subtree"
    );
    assert!(
        !ovl0.join("shared.txt.__vfs_wh__").exists(),
        "root 1's delete wrote a whiteout into root 0's subtree"
    );
    assert_eq!(
        std::fs::read(root0.join("shared.txt")).unwrap(),
        b"ROOT0",
        "root 1's delete hid root 0's file at the same relative path"
    );

    // --- Rename within root 1 stays within root 1 ---
    std::fs::rename(root1.join("created.txt"), root1.join("renamed.txt")).expect("rename");
    assert_eq!(std::fs::read(root1.join("renamed.txt")).unwrap(), b"NEW");
    assert!(std::fs::read(root1.join("created.txt")).is_err(), "source hidden after rename");
    assert!(
        ovl1.join("renamed.txt").exists(),
        "the renamed file must live in root 1's overlay subtree"
    );
    assert!(!ovl0.join("renamed.txt").exists(), "rename crossed into root 0's subtree");

    // --- A rename ACROSS roots fails, and moves nothing (gate 4, Task 5) ---
    //
    // The overlay has no cross-root move and neither does the provider
    // contract, so `Engine::rename` refuses. It used to refuse by returning
    // the same `false` it uses for "not ours", and `setinfo_hook` reads that
    // as permission to run the real `NtSetInformationFile` — against a handle
    // that, for an overlay-captured file, points at the *overlay copy*. The
    // kernel then physically moved that copy onto real disk under root 0,
    // where it reads back as missing because root 0 seals every path the
    // provider graph does not serve. Same-volume it silently succeeded;
    // cross-volume the game got a bare STATUS_NOT_SAME_DEVICE instead.
    //
    // `renamed.txt` is a live overlay capture with real bytes behind it, so
    // this is that exact shape rather than a rename of something absent.
    let escape_target = root0.join("escaped-across-roots.txt");
    let result = std::fs::rename(root1.join("renamed.txt"), &escape_target);
    // The detours have to come down before the filesystem can be inspected:
    // `escape_target` is under root 0, so a hooked `exists()` asks the engine
    // (which answers "no" for anything the VFS does not serve) rather than the
    // filesystem — it would report the escaped file as absent and pass
    // vacuously.
    drop(guard);
    // Asserted before the error itself, because this is the substantive
    // claim: the bytes must still be where the VFS put them. A run that gets
    // here with the file present has already lost the content, whatever
    // status the call returned.
    assert!(
        !escape_target.exists(),
        "the cross-root rename physically moved the overlay copy to {escape_target:?}, on \
         real disk under root 0 — content that has now left the VFS and reads as missing"
    );
    assert!(
        result.is_err(),
        "a rename whose two sides land under different managed roots must fail rather than \
         report a success it did not perform"
    );
    assert!(
        !ovl0.join("escaped-across-roots.txt").exists(),
        "the cross-root rename landed in root 0's overlay subtree — refusing must not mean \
         picking one of the two roots"
    );
    // The source survives the refusal intact: a failed rename must not be a
    // half-done one. Read through the overlay's own on-disk copy, since the
    // detours (and with them the virtual spelling of this file) are down.
    assert_eq!(
        std::fs::read(ovl1.join("renamed.txt")).unwrap_or_default(),
        b"NEW",
        "the refused rename destroyed or moved its source out of root 1's overlay subtree"
    );

    let _ = std::fs::remove_dir_all(&base);
}

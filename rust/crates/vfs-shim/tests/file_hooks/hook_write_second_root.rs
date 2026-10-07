//! Runs in its own process: the same write path as `hook_write.rs`, one root over.
//!
//! `hook_write.rs` proves writes reach the director for root 0. This proves the *root* survives
//! the trip, which is a different claim (gate 4 Task 3): every request a hook sends carries the
//! `RootId` the path resolved under, rather than `RootId::DEFAULT`. A defaulted root is
//! invisible to anything that only ever declares one root.
//!
//! The test used to assert this against the shim-local overlay's per-root subtrees (task C8
//! removed that overlay). The fake director keys a second root's files apart from root 0's
//! (`fakedirector::key`), so the same claims are now made about the director's table: a root
//! that got lost anywhere in the chain shows up as one root reading, modifying, deleting or
//! renaming the other's file. The delete and rename half never ran before C8: the test stopped
//! at the overlay's whiteout-marker assertion.
//!
//! The load-bearing shape throughout is *the same relative path under both roots*:
//! `shared.txt` is served under both, with different bytes.
use crate::fakedirector;

use fakedirector::{key, Fake, ReadStyle};
use std::io::Write;
use vfs_shim::install;

#[test]
fn writes_under_a_second_root_stay_in_that_root_s_overlay() {
    isolate!();
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-shim-write-2root-{pid}"));
    let _ = std::fs::remove_dir_all(&base);
    let root0 = base.join("root0");
    // A separate location, not nested under root 0 — the `Documents\My Games\…` shape
    // `skyrim-live` declares as root 1.
    let root1 = base.join("root1");
    std::fs::create_dir_all(&root0).unwrap();
    std::fs::create_dir_all(&root1).unwrap();

    // Root 1 is declared the way a session declares it, before the client connects.
    std::env::set_var(
        vfs_env::VIRTUAL_ROOTS,
        format!("1={}", root1.to_string_lossy()),
    );
    let fake = fakedirector::install(
        &root0,
        Fake::new()
            .with(&key(0, "shared.txt"), b"ROOT0".to_vec(), ReadStyle::Whole)
            .with(&key(1, "shared.txt"), b"ROOT1".to_vec(), ReadStyle::Whole)
            .writable_under(&key(0, ""))
            .writable_under(&key(1, "")),
        0,
    );
    let guard = install().expect("install");

    // --- Reads resolve against the root the path actually lies under ---
    let read0 = std::fs::read(root0.join("shared.txt"));
    let read1 = std::fs::read(root1.join("shared.txt"));

    // --- Create under root 1 lands under root 1 ---
    {
        let mut f = std::fs::File::create(root1.join("created.txt")).expect("create under root 1");
        f.write_all(b"NEW").unwrap();
    }
    let created_read = std::fs::read(root1.join("created.txt"));

    // --- A preserving write under root 1 leaves root 0's file alone ---
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(root1.join("shared.txt"))
            .expect("append-open under root 1");
        f.write_all(b"!").unwrap();
    }
    let modified1 = std::fs::read(root1.join("shared.txt"));
    let still0 = std::fs::read(root0.join("shared.txt"));

    // --- Delete under root 1 deletes root 1's copy only ---
    let delete_result = std::fs::remove_file(root1.join("shared.txt"));
    let deleted1_gone = std::fs::read(root1.join("shared.txt")).is_err();
    let after_delete0 = std::fs::read(root0.join("shared.txt"));

    // --- Rename within root 1 stays within root 1 ---
    let rename_result = std::fs::rename(root1.join("created.txt"), root1.join("renamed.txt"));
    let renamed_read = std::fs::read(root1.join("renamed.txt"));
    let rename_src_gone = std::fs::read(root1.join("created.txt")).is_err();

    // --- A rename ACROSS roots fails, and moves nothing (gate 4, Task 5) ---
    //
    // `OP_RENAME` carries one root for both sides and the provider contract has no cross-root
    // move, so the shim refuses. It must never fall to the real `NtSetInformationFile`, which
    // would move whatever the handle is on onto real disk under root 0.
    let escape_target = root0.join("escaped-across-roots.txt");
    let cross_result = std::fs::rename(root1.join("renamed.txt"), &escape_target);

    // The detours have to come down before the filesystem can be inspected.
    drop(guard);

    assert_eq!(read0.ok().as_deref(), Some(&b"ROOT0"[..]));
    assert_eq!(
        read1.ok().as_deref(),
        Some(&b"ROOT1"[..]),
        "root 1's read came back with root 0's bytes — the same relative path resolved under \
         the wrong root"
    );

    assert_eq!(
        created_read.ok().as_deref(),
        Some(&b"NEW"[..]),
        "readable back through root 1"
    );
    assert_eq!(
        fake.contents(&key(0, "created.txt")),
        None,
        "root 1's create landed under ROOT 0 — the RootId was defaulted somewhere between \
         create_hook and the ring"
    );

    assert_eq!(modified1.ok().as_deref(), Some(&b"ROOT1!"[..]));
    assert_eq!(
        still0.ok().as_deref(),
        Some(&b"ROOT0"[..]),
        "modifying root 1's copy changed root 0's"
    );

    delete_result.expect("delete under root 1");
    assert!(deleted1_gone, "deleted under root 1");
    assert_eq!(fake.tally.deletes(&key(1, "shared.txt")), 1);
    assert_eq!(
        fake.tally.deletes(&key(0, "shared.txt")),
        0,
        "root 1's delete was sent as root 0's"
    );
    assert_eq!(
        after_delete0.ok().as_deref(),
        Some(&b"ROOT0"[..]),
        "root 1's delete hid root 0's file at the same relative path"
    );
    assert_eq!(
        fake.contents(&key(0, "shared.txt")).as_deref(),
        Some(&b"ROOT0"[..])
    );

    rename_result.expect("rename");
    assert_eq!(renamed_read.ok().as_deref(), Some(&b"NEW"[..]));
    assert!(rename_src_gone, "source hidden after rename");
    assert_eq!(
        fake.tally.renames(&key(1, "created.txt")),
        1,
        "the rename must reach the director under root 1"
    );
    assert_eq!(
        fake.contents(&key(0, "renamed.txt")),
        None,
        "rename crossed into root 0"
    );

    // The substantive claim of the cross-root case first: nothing on real disk.
    assert!(
        !escape_target.exists(),
        "the cross-root rename physically created {escape_target:?} on real disk under root 0"
    );
    assert!(
        cross_result.is_err(),
        "a rename whose two sides land under different managed roots must fail rather than \
         report a success it did not perform"
    );
    assert_eq!(
        fake.contents(&key(0, "escaped-across-roots.txt")),
        None,
        "the cross-root rename landed under root 0 — refusing must not mean picking one of the \
         two roots"
    );
    assert_eq!(
        fake.contents(&key(1, "renamed.txt")).as_deref(),
        Some(&b"NEW"[..]),
        "the refused rename destroyed or moved its source"
    );

    // Neither real root directory gained anything.
    for dir in [&root0, &root1] {
        assert_eq!(
            std::fs::read_dir(dir).unwrap().count(),
            0,
            "a write under {dir:?} reached the real filesystem"
        );
    }

    let _ = std::fs::remove_dir_all(&base);
}

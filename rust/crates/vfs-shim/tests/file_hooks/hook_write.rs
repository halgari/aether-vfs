//! Runs in its own process: writes under a managed root land in the director's provider graph,
//! a preserving write keeps the provider's bytes, and the real file under the root is never
//! read from or written to.
//!
//! The name is historical (it tracked a documented flakiness allowance in several task
//! reports). The test used to drive the shim-local overlay with no director attached: a create
//! landed in the overlay directory, a delete wrote a `<name>.__vfs_wh__` marker beside it and a
//! rename moved the overlay copy. Task C8 removed that overlay, and every one of those
//! operations is now the director's, so the same four claims are made about the director's
//! table instead. The delete and rename half never ran before C8: the test stopped at the
//! whiteout-marker assertion, which is what the overlay's handle-based delete got wrong.
//!
//! Every claim is checked on both sides. The real file under the root holds bytes the director
//! does not (`HOST_MOD`), so a write that reached it, or a read that came from it, is visible.
use crate::fakedirector;

use fakedirector::{Fake, ReadStyle};
use std::io::Write;
use vfs_shim::install;

/// What the director serves for `mod.esp`.
const DIR_MOD: &[u8] = b"ORIG";
/// What is physically on disk at `<root>\mod.esp`. Nothing may read or change it.
const HOST_MOD: &[u8] = b"host: mod.esp, never served";

#[test]
fn writes_land_in_overlay_with_cow() {
    isolate!();
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-shim-write-{pid}"));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("mod.esp"), HOST_MOD).unwrap();

    // One writable mount over the whole root, serving a mod file and a file to delete.
    let fake = fakedirector::install(
        &root,
        Fake::new()
            .with("mod.esp", DIR_MOD.to_vec(), ReadStyle::Whole)
            .with("to_delete.txt", b"DELETE-ME".to_vec(), ReadStyle::Whole)
            .writable_under(""),
        0,
    );
    let hooks = install().expect("install");

    // --- Create a brand-new file under the root ---
    let newfile = root.join("created.txt");
    {
        let mut f = std::fs::File::create(&newfile).expect("create new");
        f.write_all(b"NEW").unwrap();
    }
    let created_read = std::fs::read(&newfile);

    // --- A preserving write to a mod file ---
    //
    // `append(true)` without `create` is `FILE_OPEN`: the file must already exist, and its
    // existing content is kept. That content is the provider's, never the real file's.
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(root.join("mod.esp"))
            .expect("a preserving write to a served file must open");
        f.write_all(b"!").unwrap();
    }
    let modified_read = std::fs::read(root.join("mod.esp"));

    // --- Delete ---
    let visible_before = std::fs::read(root.join("to_delete.txt"));
    let delete_result = std::fs::remove_file(root.join("to_delete.txt"));
    let gone_after = std::fs::read(root.join("to_delete.txt")).is_err();
    // Deleting the mod file removes it from the composed view, and leaves the real file alone.
    let delete_mod_result = std::fs::remove_file(root.join("mod.esp"));
    let mod_gone_after = std::fs::read(root.join("mod.esp")).is_err();

    // --- Rename within the root ---
    std::fs::write(root.join("rename_src.txt"), b"RENAMEME").unwrap();
    let rename_result = std::fs::rename(root.join("rename_src.txt"), root.join("rename_dst.txt"));
    let renamed_read = std::fs::read(root.join("rename_dst.txt"));
    let src_gone_after = std::fs::read(root.join("rename_src.txt")).is_err();

    // The real filesystem under the root is only visible with the detours down.
    drop(hooks);

    assert_eq!(
        created_read.ok().as_deref(),
        Some(&b"NEW"[..]),
        "new file readable via VFS"
    );
    assert_eq!(
        fake.contents("created.txt").as_deref(),
        Some(&b"NEW"[..]),
        "the created file must be in the director's table"
    );

    assert_eq!(
        modified_read.ok().as_deref(),
        Some(&b"ORIG!"[..]),
        "a preserving write keeps the provider's bytes and adds to them"
    );
    assert_eq!(
        fake.contents("mod.esp").as_deref(),
        None,
        "mod.esp was deleted later, so the director must no longer serve it"
    );

    assert_eq!(
        visible_before.ok().as_deref(),
        Some(&b"DELETE-ME"[..]),
        "visible pre-delete"
    );
    delete_result.expect("delete");
    assert!(gone_after, "deleted file hidden");
    assert_eq!(
        fake.tally.deletes("to_delete.txt"),
        1,
        "the delete must reach the director as one OP_DELETE"
    );
    assert_eq!(
        fake.contents("to_delete.txt"),
        None,
        "the director still serves it"
    );
    delete_mod_result.expect("delete mod");
    assert!(mod_gone_after, "deleted mod hidden");

    rename_result.expect("rename");
    assert_eq!(
        renamed_read.ok().as_deref(),
        Some(&b"RENAMEME"[..]),
        "renamed to dst"
    );
    assert!(src_gone_after, "source hidden after rename");
    assert_eq!(
        fake.tally.renames("rename_src.txt"),
        1,
        "the rename must reach the director as one OP_RENAME"
    );
    assert_eq!(fake.contents("rename_src.txt"), None);
    assert_eq!(
        fake.contents("rename_dst.txt").as_deref(),
        Some(&b"RENAMEME"[..])
    );

    // The real tree under the root: the one file it started with, unchanged, and nothing else.
    assert_eq!(
        std::fs::read(root.join("mod.esp")).ok().as_deref(),
        Some(HOST_MOD),
        "the real file under the root was modified or deleted"
    );
    let mut on_disk: Vec<String> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    on_disk.sort();
    assert_eq!(
        on_disk,
        ["mod.esp"],
        "a write under the root reached the real filesystem"
    );

    let _ = std::fs::remove_dir_all(&base);
}

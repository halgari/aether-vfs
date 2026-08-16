//! The three ways a **handle**-based delete or rename still reached the real
//! file under a managed root (gate 5, Task 5, review round 2).
//!
//! All three live in `setinfo_hook`'s non-synthetic branch, and all three end
//! the same way: `tramp`, and the kernel acting on the real file. They are only
//! reachable with no director attached — which is not a hypothetical
//! configuration (`vfs_env::READY_FUSE_FAILED_PREFIX` exists because a session
//! can be released past a `FuseClient` that failed to attach) and is exactly
//! the shape this binary installs: an `Engine` with its roots and its write
//! overlay, and no ring at all.
//!
//! 1. **A handle the shim never saw opened.** `record_path` populates
//!    `PATH_TABLE` only from an intercepted open, so a handle inherited across
//!    `CreateProcess`, duplicated in, or opened before injection is in no table
//!    of ours. That miss was read as "not ours" and the delete went to the real
//!    file. It is now recovered with `GetFinalPathNameByHandleW`, so the
//!    overlay absorbs it exactly as it does for a handle the shim did see —
//!    this one is *routed*, not merely refused.
//!
//! 2. **A rename out of a managed root** to a target outside every root.
//!    `Engine::rename` answers `Declined` because its `to` side resolves
//!    nowhere, and the kernel then performed the move — **unlinking a real file
//!    under a managed root**. That the destination was legitimately outside
//!    does not make the source side any less of a breach.
//!
//! 3. **A delete the overlay declines** — here the managed root directory
//!    itself, which resolves with an empty remainder. `delete_hook` has had a
//!    `path_is_ours` backstop for this since it was written; its handle-based
//!    sibling did not, and fell to `tramp`.
//!
//! The control case matters as much as the three: a delete of a file outside
//! every root must still really happen. The fix adds an OS consult on a
//! `PATH_TABLE` miss, and a version of it that over-claimed would pass all
//! three assertions above and break every unrelated delete in the process.
//!
//! Every claim is about filesystem state, and the file contents are distinct
//! per path so a survivor can be named.

mod ntapi;

use vfs_redirect::RootId;
use vfs_shim::{install, overlay_layer_dir, Engine};

const HOST_EXPORT: &[u8] = b"host: data/export.esp";
const HOST_UNSEEN: &[u8] = b"host: data/unseen.esp";
const OUTSIDE: &[u8] = b"outside every root";

const STATUS_SUCCESS: i32 = 0;
/// `STATUS_ACCESS_DENIED`.
const STATUS_ACCESS_DENIED: i32 = 0xC000_0022u32 as i32;
const DELETE: u32 = ntapi::DELETE;

#[test]
fn handle_based_deletes_and_out_of_root_renames_never_touch_the_real_file() {
    let base = std::env::temp_dir().join(format!("vfs-handle-ops-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    let outside = base.join("outside");
    let overlay = base.join("overlay");
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::create_dir_all(overlay_layer_dir(&overlay, RootId::DEFAULT)).unwrap();
    std::fs::write(root.join("data").join("export.esp"), HOST_EXPORT).unwrap();
    std::fs::write(root.join("data").join("unseen.esp"), HOST_UNSEEN).unwrap();
    std::fs::write(base.join("control.txt"), OUTSIDE).unwrap();

    // **Opened before `install`, on purpose.** These two handles are the
    // fixture's stand-in for an inherited or pre-injection handle: no detour
    // was in place when they were created, so nothing recorded them, which is
    // precisely the state a `CreateProcess`-inherited handle arrives in.
    let (st, unseen_handle) =
        ntapi::nt_open_abs(&root.join("data").join("unseen.esp").to_string_lossy(), DELETE);
    assert!(st >= 0, "pre-install open of the under-root file failed: {st:#x}");
    let (st, root_dir_handle) =
        ntapi::nt_open_dir_abs(&root.to_string_lossy(), DELETE | ntapi::FILE_LIST_DIRECTORY);
    assert!(st >= 0, "pre-install open of the root directory failed: {st:#x}");

    // Both real files are mapped at their own on-disk paths, so a read/DELETE
    // open of them resolves rather than being denied — see `hook_write.rs` on
    // why a bare real file under the root is invisible to the VFS.
    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let entries = ["export.esp", "unseen.esp"]
            .iter()
            .map(|name| InputEntry {
                vpath: format!("data/{name}"),
                kind: EntryKind::File,
                source: root.join("data").join(name).to_string_lossy().as_ref().into(),
                size: 0,
                mtime: 0,
            })
            .collect();
        let tree = build(vec![Layer { id: LayerId(0), entries }]).unwrap();
        vfs_shared::bridge::flatten(&tree)
    };

    let engine =
        Engine::with_overlay(root.to_str().unwrap(), overlay.to_str().unwrap(), snapshot).unwrap();
    let hooks = install(engine).expect("install");

    // 2. The rename out. `MoveFileExW` opens the source (which the shim *does*
    //    see) and issues a set-info whose target resolves nowhere.
    let export_result =
        std::fs::rename(root.join("data").join("export.esp"), outside.join("export.esp"));

    // 1. The unseen under-root handle.
    let unseen_status = ntapi::nt_set_disposition_delete(unseen_handle);
    ntapi::close(unseen_handle); // the unlink, if any, lands here

    // 3. The overlay-declined delete: the managed root itself.
    let root_dir_status = ntapi::nt_set_disposition_delete(root_dir_handle);
    ntapi::close(root_dir_handle);

    // Control: outside every root, through the ordinary Win32 route.
    let control_result = std::fs::remove_file(base.join("control.txt"));

    drop(hooks);

    // --- filesystem first ---------------------------------------------------
    assert_eq!(
        std::fs::read(root.join("data").join("export.esp")).ok().as_deref(),
        Some(HOST_EXPORT),
        "the real data/export.esp is gone: the kernel performed the rename out of the root, \
         which unlinks a real file under a managed root — the destination being outside does \
         not make the source side any less of a breach"
    );
    assert!(
        !outside.join("export.esp").exists(),
        "the export landed outside the root, so the move really happened"
    );
    assert_eq!(
        std::fs::read(root.join("data").join("unseen.esp")).ok().as_deref(),
        Some(HOST_UNSEEN),
        "the real data/unseen.esp was unlinked — a handle the shim never saw opened is still \
         a handle on a path under a managed root, and a `PATH_TABLE` miss must not be read as \
         `not ours`"
    );
    assert!(root.is_dir(), "the managed root directory itself was deleted");

    // --- then the statuses --------------------------------------------------
    assert!(
        export_result.is_err(),
        "the rename out of the managed root reported success"
    );
    assert_eq!(
        unseen_status, STATUS_SUCCESS,
        "the recovered path resolves under a root with a non-empty remainder, so the overlay \
         whiteout absorbs this delete exactly as it does for a handle the shim did see — this \
         one is routed, not refused; got {unseen_status:#x}"
    );
    let marker = overlay_layer_dir(&overlay, RootId::DEFAULT)
        .join("data")
        .join(vfs_redirect::whiteout_marker("unseen.esp"));
    assert!(
        marker.exists(),
        "no whiteout marker at {}: success without one is a deleted file that comes back",
        marker.display()
    );
    assert_eq!(
        root_dir_status, STATUS_ACCESS_DENIED,
        "the managed root resolves with an empty remainder, which the overlay declines — and a \
         decline under a managed root must fail closed rather than reach the kernel, the same \
         backstop `delete_hook` already has; got {root_dir_status:#x}"
    );

    // --- and the control ----------------------------------------------------
    control_result.expect("a delete outside every managed root must still succeed");
    assert!(
        !base.join("control.txt").exists(),
        "the file outside every root survived — the OS-consult fallback is claiming handles \
         that are none of its business"
    );

    let _ = std::fs::remove_dir_all(&base);
}

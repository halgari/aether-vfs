//! The three ways a **handle**-based delete or rename used to reach the real
//! file under a managed root (gate 5, Task 5, review round 2), now answered by
//! the director.
//!
//! All three live in `setinfo_hook`'s branch for a **real** (non-synthetic)
//! handle: one the kernel issued, whose path is under a root. A director-served
//! open never produces one, so the fixture makes them the way a game gets them:
//! handles opened before the detours were installed, which is the state a handle
//! inherited across `CreateProcess`, duplicated in, or opened before injection
//! arrives in.
//!
//! 1. **A delete through a handle the shim never saw opened.** No table of ours
//!    records it, so its path is recovered with `GetFinalPathNameByHandleW` and
//!    the delete is sent to the director (`OP_DELETE`), as `delete_hook` does
//!    for the same path. It used to write a shim-spelled whiteout marker into
//!    the shim-local overlay instead, which the director never consulted.
//! 2. **A rename out of a managed root** to a target outside every root, by
//!    both handle kinds: `std::fs::rename` (the shim sees the open, so the
//!    handle is synthetic) and `FILE_RENAME_INFORMATION` on a real handle. The
//!    provider contract has no move out of a root, so both are refused, and the
//!    kernel must never perform the move — that would unlink a real file under
//!    a managed root.
//! 3. **A delete of the managed root directory itself.** Its remainder is the
//!    root (`"."` on the wire), which no provider deletes, so it fails, and the
//!    real directory stays.
//!
//! The control case matters as much as the three: a delete of a file outside
//! every root must still really happen. The path recovery is an OS consult on a
//! table miss, and a version of it that over-claimed would pass all three
//! assertions above and break every unrelated delete in the process.
//!
//! Every claim is about filesystem state and the director's table, and the
//! bytes are distinct per path and per side, so a survivor can be named.

use crate::fakedirector;
use crate::ntapi;

use fakedirector::{Fake, ReadStyle};
use vfs_shim::install;

const HOST_EXPORT: &[u8] = b"host: data/export.esp";
const HOST_MOVED: &[u8] = b"host: data/moved.esp";
const HOST_UNSEEN: &[u8] = b"host: data/unseen.esp";
const DIR_EXPORT: &[u8] = b"director: data/export.esp";
const DIR_MOVED: &[u8] = b"director: data/moved.esp";
const DIR_UNSEEN: &[u8] = b"director: data/unseen.esp";
const OUTSIDE: &[u8] = b"outside every root";

const STATUS_SUCCESS: i32 = 0;
const DELETE: u32 = ntapi::DELETE;

#[test]
fn handle_based_deletes_and_out_of_root_renames_never_touch_the_real_file() {
    isolate!();
    let base = std::env::temp_dir().join(format!("vfs-handle-ops-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    let outside = base.join("outside");
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(root.join("data").join("export.esp"), HOST_EXPORT).unwrap();
    std::fs::write(root.join("data").join("moved.esp"), HOST_MOVED).unwrap();
    std::fs::write(root.join("data").join("unseen.esp"), HOST_UNSEEN).unwrap();
    std::fs::write(base.join("control.txt"), OUTSIDE).unwrap();

    // **Opened before `install`, on purpose.** These handles are the
    // fixture's stand-in for an inherited or pre-injection handle: no detour
    // was in place when they were created, so nothing recorded them, which is
    // precisely the state a `CreateProcess`-inherited handle arrives in.
    let (st, unseen_handle) = ntapi::nt_open_abs(
        &root.join("data").join("unseen.esp").to_string_lossy(),
        DELETE,
    );
    assert!(
        st >= 0,
        "pre-install open of the under-root file failed: {st:#x}"
    );
    let (st, moved_handle) = ntapi::nt_open_abs(
        &root.join("data").join("moved.esp").to_string_lossy(),
        DELETE,
    );
    assert!(
        st >= 0,
        "pre-install open of the under-root file failed: {st:#x}"
    );
    let (st, root_dir_handle) =
        ntapi::nt_open_dir_abs(&root.to_string_lossy(), DELETE | ntapi::FILE_LIST_DIRECTORY);
    assert!(
        st >= 0,
        "pre-install open of the root directory failed: {st:#x}"
    );

    // The director serves all three files, inside a writable mount, so a
    // delete or a same-root rename it is asked for succeeds: a refusal below is
    // the shim's, not the provider graph's.
    let fake = fakedirector::install(
        &root,
        Fake::new()
            .with_dir("data")
            .with("data/export.esp", DIR_EXPORT.to_vec(), ReadStyle::Whole)
            .with("data/moved.esp", DIR_MOVED.to_vec(), ReadStyle::Whole)
            .with("data/unseen.esp", DIR_UNSEEN.to_vec(), ReadStyle::Whole)
            .writable_under("data/"),
        0,
    );
    let hooks = install().expect("install");

    // 2a. The rename out, the Win32 way. `MoveFileExW` opens the source (which
    //     the shim *does* see, so the handle is synthetic) and issues a set-info
    //     whose target resolves under no root.
    let export_result = std::fs::rename(
        root.join("data").join("export.esp"),
        outside.join("export.esp"),
    );

    // 2b. The rename out through a real handle.
    let moved_status = ntapi::nt_rename(
        moved_handle,
        &outside.join("moved.esp").to_string_lossy(),
        ntapi::FILE_RENAME_INFORMATION,
    );
    ntapi::close(moved_handle);

    // 1. The unseen under-root handle.
    let unseen_status = ntapi::nt_set_disposition_delete(unseen_handle);
    ntapi::close(unseen_handle); // the unlink, if any, lands here

    // 3. The managed root itself.
    let root_dir_status = ntapi::nt_set_disposition_delete(root_dir_handle);
    ntapi::close(root_dir_handle);

    // Control: outside every root, through the ordinary Win32 route.
    let control_result = std::fs::remove_file(base.join("control.txt"));

    drop(hooks);

    // --- filesystem first ---------------------------------------------------
    assert_eq!(
        std::fs::read(root.join("data").join("export.esp"))
            .ok()
            .as_deref(),
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
        std::fs::read(root.join("data").join("moved.esp"))
            .ok()
            .as_deref(),
        Some(HOST_MOVED),
        "the real data/moved.esp is gone: a rename out of the root through a real handle was \
         performed by the kernel"
    );
    assert!(
        !outside.join("moved.esp").exists(),
        "moved.esp landed outside the root, so the move really happened"
    );
    assert_eq!(
        std::fs::read(root.join("data").join("unseen.esp"))
            .ok()
            .as_deref(),
        Some(HOST_UNSEEN),
        "the real data/unseen.esp was unlinked — a handle the shim never saw opened is still \
         a handle on a path under a managed root, and a `PATH_TABLE` miss must not be read as \
         `not ours`"
    );
    assert!(
        root.is_dir(),
        "the managed root directory itself was deleted"
    );

    // --- then the director's side ----------------------------------------
    assert_eq!(
        fake.tally.deletes("data/unseen.esp"),
        1,
        "the delete through the unseen handle never reached the director as an OP_DELETE: a \
         handle the shim never saw opened is still a handle on a path under a managed root, \
         and a table miss must not be read as `not ours`"
    );
    assert_eq!(
        fake.contents("data/unseen.esp"),
        None,
        "the director still serves data/unseen.esp: the delete was swallowed, not answered"
    );
    for (vpath, bytes) in [
        ("data/export.esp", DIR_EXPORT),
        ("data/moved.esp", DIR_MOVED),
    ] {
        assert_eq!(
            fake.contents(vpath).as_deref(),
            Some(bytes),
            "{vpath} left the director's table: a refused rename out of the root must not be \
             half-done"
        );
        assert_eq!(
            fake.tally.renames(vpath),
            0,
            "{vpath}: a rename out of the root reached the director, which has no operation for \
             it (OP_RENAME carries one root and two vpaths, both inside it)"
        );
    }

    // --- then the statuses --------------------------------------------------
    assert!(
        export_result.is_err(),
        "the rename out of the managed root reported success"
    );
    assert!(
        moved_status < 0,
        "the rename out of the managed root through a real handle reported success; got \
         {moved_status:#x}"
    );
    assert_eq!(
        unseen_status, STATUS_SUCCESS,
        "the recovered path routes under a root and the director deleted it, so the caller's \
         answer is the director's success; got {unseen_status:#x}"
    );
    assert!(
        root_dir_status < 0,
        "a delete of the managed root itself reported success; got {root_dir_status:#x}"
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

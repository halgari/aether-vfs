//! **A rename whose *target* lands under a managed root, from a source
//! outside every root** (gate 5, Task 5).
//!
//! Three predicates had to line up for this to be an escape, and they did:
//!
//! 1. `record_path` inserts into `PATH_TABLE` only when `path_is_ours(path)`,
//!    so a handle on a file outside every root is never recorded.
//! 2. `setinfo_hook`'s non-synthetic branch consults `Engine::rename` only for
//!    a handle that *is* in `PATH_TABLE`, so the engine is never asked.
//! 3. `Engine::rename` answers `CrossRoot` only when **both** sides resolve
//!    under managed roots, so even if it had been asked it would have declined.
//!
//! The call therefore reached `tramp`, and the real `NtSetInformationFile`
//! physically created a file under the destination root — where it then reads
//! back as missing, because the root seals every path the provider graph does
//! not serve. Content crossing *into* the VFS by a route the director never saw
//! is the same containment failure as content crossing out of it; the
//! destination is what decides containment, not the source.
//!
//! **What the fixture has to discriminate.** A rename that is refused and a
//! rename that is performed both leave the source's *bytes* somewhere, so the
//! bytes alone say nothing. What separates them is where the name is:
//!
//! - After a refusal the source is still at its original path outside the root
//!   and there is nothing at the destination — not on real disk, and not in the
//!   director's table either. A rename that landed leaves the source path empty
//!   and a real file under the root.
//! - The status must be a failure. A refusal reported as success is the worst
//!   available outcome: the caller believes the file moved and stops looking
//!   for it at the source.
//!
//! Both spellings of the rename class are driven (`FILE_RENAME_INFORMATION`
//! and its `_EX` form), plus `std::fs::rename`, which is how a real caller gets
//! here — `MoveFileExW` opens the source with `DELETE` and issues exactly this
//! set-info. Testing only the raw NT form would leave the question of whether
//! Win32 even routes through the hooked class unanswered.
//!
//! Its own binary: the detours, the `FuseClient` and the `Engine` are
//! process-global and resolve once.

mod fakedirector;
mod ntapi;

use fakedirector::{Fake, ReadStyle};
use vfs_shim::{install, Engine};

/// Bytes of the file being moved in from outside. Distinct from anything the
/// director holds, so a copy that appears anywhere can be attributed.
const IMPORT_STD: &[u8] = b"outside: imported via MoveFileExW";
const IMPORT_NT: &[u8] = b"outside: imported via FILE_RENAME_INFORMATION";
const IMPORT_NT_EX: &[u8] = b"outside: imported via FILE_RENAME_INFORMATION_EX";
/// Something the director really does serve, so the root is not merely empty.
const DIR_EXISTING: &[u8] = b"director: data/existing.esp";

const DELETE: u32 = ntapi::DELETE;

#[test]
fn a_rename_into_a_managed_root_from_outside_it_never_lands() {
    let base = std::env::temp_dir().join(format!("vfs-rename-in-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    let outside = base.join("outside");
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("import-std.esp"), IMPORT_STD).unwrap();
    std::fs::write(outside.join("import-nt.esp"), IMPORT_NT).unwrap();
    std::fs::write(outside.join("import-nt-ex.esp"), IMPORT_NT_EX).unwrap();

    std::env::set_var(vfs_env::SHIM_STATS_LOG, base.join("shim-stats.log"));
    std::env::set_var(vfs_env::SHIM_STATS_INTERVAL_MS, "3600000");

    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "data/existing.esp".into(),
                kind: EntryKind::File,
                source: root.join("data").join("existing.esp").to_string_lossy().as_ref().into(),
                size: DIR_EXISTING.len() as u64,
                mtime: 0,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };

    // `data/` is a writable mount: the destination is somewhere the director
    // would happily accept a *create*, so a refusal here cannot be explained
    // away as "the root was read-only anyway".
    let fake = fakedirector::install(
        &root,
        Fake::new()
            .with("data/existing.esp", DIR_EXISTING.to_vec(), ReadStyle::Whole)
            .writable_under("data/"),
        0,
    );

    let engine = Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let hooks = install(engine).expect("install");

    // 1. The Win32 route a real caller takes.
    let std_result = std::fs::rename(
        outside.join("import-std.esp"),
        root.join("data").join("import-std.esp"),
    );

    // 2 and 3. Both NT rename classes, driven directly so the test does not
    //    depend on which one `MoveFileExW` happens to pick on this build.
    let nt_status = {
        let (st, h) =
            ntapi::nt_open_abs(&outside.join("import-nt.esp").to_string_lossy(), DELETE);
        assert!(st >= 0, "opening the outside source failed: {st:#x}");
        let r = ntapi::nt_rename(
            h,
            &root.join("data").join("import-nt.esp").to_string_lossy(),
            ntapi::FILE_RENAME_INFORMATION,
        );
        ntapi::close(h);
        r
    };
    let nt_ex_status = {
        let (st, h) =
            ntapi::nt_open_abs(&outside.join("import-nt-ex.esp").to_string_lossy(), DELETE);
        assert!(st >= 0, "opening the outside source failed: {st:#x}");
        let r = ntapi::nt_rename(
            h,
            &root.join("data").join("import-nt-ex.esp").to_string_lossy(),
            ntapi::FILE_RENAME_INFORMATION_EX,
        );
        ntapi::close(h);
        r
    };

    drop(hooks);

    // **The filesystem side first.** A rename that was performed reports
    // success and leaves no other trace at the API; the destination directory
    // is the only witness.
    for name in ["import-std.esp", "import-nt.esp", "import-nt-ex.esp"] {
        let landed = root.join("data").join(name);
        assert!(
            !landed.exists(),
            "{} exists on real disk under the managed root — the rename was performed by the \
             kernel, putting content under a root the director cannot account for",
            landed.display()
        );
        assert_eq!(
            fake.contents(&format!("data/{name}")),
            None,
            "data/{name} is in the director's table: the shim invented an import the \
             provider contract has no operation for (OP_RENAME carries one root and two \
             vpaths, both inside it)"
        );
    }

    // The sources are still where they were, with their own bytes.
    for (name, bytes) in [
        ("import-std.esp", IMPORT_STD),
        ("import-nt.esp", IMPORT_NT),
        ("import-nt-ex.esp", IMPORT_NT_EX),
    ] {
        assert_eq!(
            std::fs::read(outside.join(name)).ok().as_deref(),
            Some(bytes),
            "{name} is gone from outside the root: the move half happened even though the \
             landing half did not, which loses the file outright"
        );
    }

    // Only now the statuses. A refusal reported as success is the failure mode
    // that matters, and it is not visible in the filesystem assertions above.
    assert!(
        std_result.is_err(),
        "std::fs::rename into a managed root from outside it reported success"
    );
    assert!(
        nt_status < 0,
        "FILE_RENAME_INFORMATION into a managed root reported success; got {nt_status:#x}"
    );
    assert!(
        nt_ex_status < 0,
        "FILE_RENAME_INFORMATION_EX into a managed root reported success; got {nt_ex_status:#x}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

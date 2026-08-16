//! **`NtDeleteFile` reaches real disk.** (gate 5, Task 5.)
//!
//! Every other unhooked NT API on this project's list fails *safely* on a
//! synthetic handle: it is handed a handle the kernel does not own and returns
//! `STATUS_INVALID_HANDLE`. `NtDeleteFile` has no handle to be wrong about. Its
//! entire request is an `OBJECT_ATTRIBUTES`, so an unhooked call resolves the
//! path itself, against the real filesystem, and deletes the real file — under
//! a managed root, whose whole contract is that the real filesystem beneath it
//! is unreachable by any spelling.
//!
//! **Why the fixture is shaped the way it is.** A test that only asserted on
//! the returned status would pass against the unhooked code for two of its
//! three cases: an unhooked delete of a file that is really there *succeeds*,
//! and a delete of a path the director does not serve also succeeds. So every
//! claim here is made about filesystem state, and the bytes on disk are
//! deliberately different from the bytes the director holds for the same
//! vpath, so a surviving file can be attributed to the right side:
//!
//! - `data/served.esp` exists on real disk holding `HOST_SERVED`, and the
//!   director holds `DIR_SERVED` for the same vpath. After the delete the
//!   director's copy must be gone and the real file must still hold
//!   `HOST_SERVED`. Either half alone is weak — a refused delete leaves the
//!   real file intact too, and a real delete of a file the director never had
//!   also empties nothing at the director. Together they say the delete was
//!   *answered*, not merely blocked.
//! - `data/unserved.bin` exists on real disk and the director does not serve
//!   it. That is the sealing half: the director's not-found is the caller's
//!   not-found, and the perfectly good file sitting right there stays.
//! - `readonly/locked.esp` is served, but by a layer outside every writable
//!   mount, so the director refuses with `ST_READ_ONLY`.
//! - `outside.txt` lives outside every root and must really be deleted. A hook
//!   that answered every `NtDeleteFile` would pass all the assertions above
//!   and break the rest of the process; this is the assertion that catches it.
//!
//! **The refusals are asserted by exact status, not by "it failed".** Flattening
//! every director refusal to `STATUS_UNSUCCESSFUL` (`ERROR_GEN_FAILURE`) would
//! satisfy a `< 0` check and still break callers: the delete-then-create idiom
//! treats only `ERROR_FILE_NOT_FOUND` as benign and gives up on anything else,
//! so an absent path under a root has to answer `STATUS_OBJECT_NAME_NOT_FOUND`
//! the way the open path already does.
//!
//! **The last three cases are handle-relative**, because Win32 decides on its
//! own whether a name becomes absolute or a (directory handle + leaf) pair, and
//! the `OBJECT_ATTRIBUTES` decode is the part of this hook most likely to
//! regress. All three shapes are covered: a FUSE-synthetic `RootDirectory`
//! (`parent_dir_of_handle` case 1), a real directory handle the shim watched
//! being opened (case 2), and a relative name that escapes the root with `..`,
//! which is genuinely outside and must be trampolined — by way of the absolute
//! rebuild, since the kernel cannot be handed a synthetic root.
//!
//! Its own binary: the detours, the `FuseClient`, the `Engine` and
//! `hookstats::enabled()` are process-global and resolve once.

mod fakedirector;
mod ntapi;

use fakedirector::{Fake, ReadStyle};
use vfs_shim::{install, Engine};

/// Bytes on the real filesystem under the managed root.
const HOST_SERVED: &[u8] = b"host: data/served.esp";
const HOST_UNSERVED: &[u8] = b"host: data/unserved.bin";
const HOST_LOCKED: &[u8] = b"host: readonly/locked.esp";
const HOST_RELATIVE: &[u8] = b"host: data/relative.esp";
/// Bytes only the director has, unreachable by any filesystem route.
const DIR_SERVED: &[u8] = b"director: data/served.esp";
const DIR_LOCKED: &[u8] = b"director: readonly/locked.esp";
const DIR_RELATIVE: &[u8] = b"director: data/relative.esp";
/// Files outside every managed root. Deleting them must still work.
const OUTSIDE: &[u8] = b"outside every root";

/// `STATUS_SUCCESS`.
const STATUS_SUCCESS: i32 = 0;
/// `STATUS_OBJECT_NAME_NOT_FOUND` — `ERROR_FILE_NOT_FOUND`.
const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034u32 as i32;
/// `STATUS_ACCESS_DENIED` — `ERROR_ACCESS_DENIED`.
const STATUS_ACCESS_DENIED: i32 = 0xC000_0022u32 as i32;
/// `STATUS_OBJECT_NAME_INVALID` — the kernel's refusal of a malformed NT name.
const STATUS_OBJECT_NAME_INVALID: i32 = 0xC000_0033u32 as i32;

#[test]
fn a_path_based_delete_under_a_managed_root_never_reaches_the_real_file() {
    let base = std::env::temp_dir().join(format!("vfs-ntdelete-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::create_dir_all(root.join("readonly")).unwrap();
    std::fs::write(root.join("data").join("served.esp"), HOST_SERVED).unwrap();
    std::fs::write(root.join("data").join("unserved.bin"), HOST_UNSERVED).unwrap();
    std::fs::write(root.join("data").join("relative.esp"), HOST_RELATIVE).unwrap();
    std::fs::write(root.join("readonly").join("locked.esp"), HOST_LOCKED).unwrap();
    std::fs::write(base.join("outside.txt"), OUTSIDE).unwrap();
    std::fs::write(base.join("outside-rel.txt"), OUTSIDE).unwrap();
    std::fs::write(base.join("outside-escape.txt"), OUTSIDE).unwrap();

    std::env::set_var(vfs_env::SHIM_STATS_LOG, base.join("shim-stats.log"));
    std::env::set_var(vfs_env::SHIM_STATS_INTERVAL_MS, "3600000");

    // A snapshot that knows every name at its *real* on-disk path, so the
    // engine has somewhere to send a fall-through. Without it a pass here
    // could mean "the path was not recognised" rather than "the delete was
    // contained" — the same reasoning `drm_names_route_to_director` records.
    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let entries = [
            "data/served.esp",
            "data/unserved.bin",
            "data/relative.esp",
            "readonly/locked.esp",
        ]
        .iter()
        .map(|vpath| InputEntry {
            vpath: (*vpath).into(),
            kind: EntryKind::File,
            source: vpath
                .split('/')
                .fold(root.clone(), |a, c| a.join(c))
                .to_string_lossy()
                .as_ref()
                .into(),
            size: 0,
            mtime: 0,
        })
        .collect();
        let tree = build(vec![Layer { id: LayerId(0), entries }]).unwrap();
        vfs_shared::bridge::flatten(&tree)
    };

    // `data/` is the writable mount; `readonly/` is served but by no writable
    // layer, which is how the director produces its two distinct refusals.
    // `data` itself is declared a directory so the relative cases below can get
    // a real FUSE-synthetic directory handle to name children against.
    let fake = fakedirector::install(
        &root,
        Fake::new()
            .with("data/served.esp", DIR_SERVED.to_vec(), ReadStyle::Whole)
            .with("data/relative.esp", DIR_RELATIVE.to_vec(), ReadStyle::Whole)
            .with("readonly/locked.esp", DIR_LOCKED.to_vec(), ReadStyle::Whole)
            .with_dir("data")
            .writable_under("data/"),
        0,
    );

    let engine = Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let hooks = install(engine).expect("install");

    let served_status = ntapi::nt_delete_file(&root.join("data").join("served.esp").to_string_lossy());
    let unserved_status =
        ntapi::nt_delete_file(&root.join("data").join("unserved.bin").to_string_lossy());
    let locked_status =
        ntapi::nt_delete_file(&root.join("readonly").join("locked.esp").to_string_lossy());
    let outside_status = ntapi::nt_delete_file(&base.join("outside.txt").to_string_lossy());

    // --- the handle-relative decode ----------------------------------------
    // A FUSE-synthetic directory handle for `<root>\data`: the shape a caller
    // gets from any directory open under a managed root, and the one the
    // kernel can never be handed.
    let (st, synth_dir) =
        ntapi::nt_open_dir_abs(&root.join("data").to_string_lossy(), ntapi::FILE_LIST_DIRECTORY);
    assert!(st >= 0, "the director's directory open failed: {st:#x}");
    let synth_rel_status = ntapi::nt_delete_relative(synth_dir, "relative.esp");
    // A relative name that climbs back out of the root through that same
    // synthetic handle. It resolves to a path genuinely outside every root, so
    // it must be trampolined — which is only possible after rebuilding the
    // `OBJECT_ATTRIBUTES` absolute, since the synthetic root is not a kernel
    // object (`tramp_delete_abs`).
    let escape_status = ntapi::nt_delete_relative(synth_dir, r"..\..\outside-escape.txt");
    ntapi::close(synth_dir);

    // A *real* directory handle, outside every root, that the shim watched
    // being opened — `parent_dir_of_handle`'s second case.
    let (st, real_dir) = ntapi::nt_open_dir_abs(&base.to_string_lossy(), ntapi::FILE_LIST_DIRECTORY);
    assert!(st >= 0, "the outside directory open failed: {st:#x}");
    let real_rel_status = ntapi::nt_delete_relative(real_dir, "outside-rel.txt");
    ntapi::close(real_dir);

    // Reading the real filesystem under the managed root is only possible with
    // the detours down.
    drop(hooks);

    // **The filesystem side first.** A delete that reached disk reports
    // success and says nothing else; the bytes are the only witness.
    assert_eq!(
        std::fs::read(root.join("data").join("served.esp")).ok().as_deref(),
        Some(HOST_SERVED),
        "the real data/served.esp under the managed root was deleted — a path-based delete \
         must be answered by the director, never performed on the file behind the root"
    );
    assert_eq!(
        std::fs::read(root.join("data").join("unserved.bin")).ok().as_deref(),
        Some(HOST_UNSERVED),
        "the real data/unserved.bin was deleted. The director does not serve it, so the \
         delete had to fail — a real file under a managed root that the provider graph \
         never agreed to is unreachable, deletes included"
    );
    assert_eq!(
        std::fs::read(root.join("readonly").join("locked.esp")).ok().as_deref(),
        Some(HOST_LOCKED),
        "the real readonly/locked.esp was deleted — the director serves it from a layer that \
         accepts no writes, and its refusal is the caller's answer"
    );
    assert_eq!(
        std::fs::read(root.join("data").join("relative.esp")).ok().as_deref(),
        Some(HOST_RELATIVE),
        "the real data/relative.esp was deleted — a delete named against a FUSE-synthetic \
         directory handle decodes to the same path an absolute one does, and must be answered \
         the same way"
    );

    assert_eq!(
        served_status, STATUS_SUCCESS,
        "the director accepted the delete, so the caller must see success; got {served_status:#x}"
    );
    assert_eq!(
        fake.contents("data/served.esp"),
        None,
        "the delete must land in the director's own copy — that is where the path actually \
         disappears, and a caller that sees success and a director that still has the file \
         is a delete that went somewhere else"
    );
    assert_eq!(
        fake.tally.deletes("data/served.esp"),
        1,
        "the delete must have crossed the ring as an OP_DELETE"
    );

    // **By exact status, not by "it failed".** `STATUS_UNSUCCESSFUL` would
    // satisfy a `< 0` check and still break the delete-then-create idiom,
    // which treats only `ERROR_FILE_NOT_FOUND` as benign — see the module doc.
    assert_eq!(
        unserved_status, STATUS_OBJECT_NAME_NOT_FOUND,
        "a delete of a path the director does not have is `ERROR_FILE_NOT_FOUND`, the same \
         answer the open path gives for the same ring status and the only refusal callers \
         routinely proceed past; got {unserved_status:#x}"
    );
    assert_eq!(
        locked_status, STATUS_ACCESS_DENIED,
        "a delete the director refuses as read-only is `ERROR_ACCESS_DENIED`, what a real \
         read-only filesystem answers — not a generic failure; got {locked_status:#x}"
    );
    assert_eq!(
        fake.tally.deletes("data/unserved.bin"),
        1,
        "the refusal must be the director's own answer, asked for over the ring — a shim \
         that refused it locally would leave this at zero"
    );
    assert_eq!(
        fake.tally.deletes("readonly/locked.esp"),
        1,
        "the read-only refusal must also be the director's own answer"
    );

    assert_eq!(
        synth_rel_status, STATUS_SUCCESS,
        "a delete named against a FUSE-synthetic directory handle must decode and route; \
         got {synth_rel_status:#x}"
    );
    assert_eq!(
        fake.contents("data/relative.esp"),
        None,
        "the handle-relative delete must have landed at the director"
    );

    // And the other direction: outside every root the hook must not have an
    // opinion at all.
    assert_eq!(
        outside_status, STATUS_SUCCESS,
        "a delete outside every managed root must still reach the real filesystem; \
         got {outside_status:#x}"
    );
    assert!(
        !base.join("outside.txt").exists(),
        "the file outside every root was not deleted — the hook is answering calls that are \
         none of its business"
    );
    assert_eq!(
        real_rel_status, STATUS_SUCCESS,
        "a delete named against a real directory handle outside every root must reach the \
         kernel unchanged; got {real_rel_status:#x}"
    );
    assert!(
        !base.join("outside-rel.txt").exists(),
        "the handle-relative delete outside every root did not happen"
    );
    // The `..` climb resolves outside every root, so it is trampolined — and
    // the trampoline can only be reached through the absolute rebuild, because
    // the kernel cannot resolve a synthetic `RootDirectory`.
    //
    // **What the status discriminates.** `STATUS_OBJECT_NAME_INVALID` is the
    // kernel refusing an un-normalised `..` in an NT path (Win32 collapses
    // those before it ever calls NT; the object manager does not). Getting it
    // means a *path* reached the kernel. `STATUS_INVALID_HANDLE` would mean the
    // synthetic root was handed over raw, i.e. the rebuild did not happen —
    // which is exactly what this case is here to catch, and what it does catch:
    // deleting `tramp_delete_abs`'s call site turns this into `0xC0000008`.
    //
    // The file therefore survives, and that is the honest outcome: an odd
    // spelling of an outside path fails closed rather than doing something
    // unpredictable. It is not evidence of containment — the path really is
    // outside — which is why the director tally below carries that claim.
    assert_eq!(
        escape_status, STATUS_OBJECT_NAME_INVALID,
        "expected the kernel's own refusal of an un-normalised `..`, which is only reachable \
         once the OA has been rebuilt absolute. {:} would mean the synthetic RootDirectory \
         was passed to the kernel unchanged; got {escape_status:#x}",
        "STATUS_INVALID_HANDLE (0xC0000008)"
    );
    assert!(
        base.join("outside-escape.txt").exists(),
        "the kernel refused the `..` spelling, so the file it names must be untouched"
    );
    assert_eq!(
        fake.tally.deletes("outside-escape.txt"),
        0,
        "the `..` escape must not have been offered to the director: it resolves outside \
         every root, and treating it as under one would be the mirror of the bug this file \
         is about"
    );

    let _ = std::fs::remove_dir_all(&base);
}

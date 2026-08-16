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
//! - `outside.txt` lives outside every root and must really be deleted. A hook
//!   that answered every `NtDeleteFile` would pass all the assertions above
//!   and break the rest of the process; this is the assertion that catches it.
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
/// Bytes only the director has, unreachable by any filesystem route.
const DIR_SERVED: &[u8] = b"director: data/served.esp";
/// A file outside every managed root. Deleting it must still work.
const OUTSIDE: &[u8] = b"outside every root";

/// `STATUS_SUCCESS`.
const STATUS_SUCCESS: i32 = 0;

#[test]
fn a_path_based_delete_under_a_managed_root_never_reaches_the_real_file() {
    let base = std::env::temp_dir().join(format!("vfs-ntdelete-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::write(root.join("data").join("served.esp"), HOST_SERVED).unwrap();
    std::fs::write(root.join("data").join("unserved.bin"), HOST_UNSERVED).unwrap();
    std::fs::write(base.join("outside.txt"), OUTSIDE).unwrap();

    std::env::set_var(vfs_env::SHIM_STATS_LOG, base.join("shim-stats.log"));
    std::env::set_var(vfs_env::SHIM_STATS_INTERVAL_MS, "3600000");

    // A snapshot that knows both names at their *real* on-disk paths, so the
    // engine has somewhere to send a fall-through. Without it a pass here
    // could mean "the path was not recognised" rather than "the delete was
    // contained" — the same reasoning `drm_names_route_to_director` records.
    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let entries = [("data/served.esp", "served.esp"), ("data/unserved.bin", "unserved.bin")]
            .iter()
            .map(|(vpath, name)| InputEntry {
                vpath: (*vpath).into(),
                kind: EntryKind::File,
                source: root.join("data").join(name).to_string_lossy().as_ref().into(),
                size: 0,
                mtime: 0,
            })
            .collect();
        let tree = build(vec![Layer { id: LayerId(0), entries }]).unwrap();
        vfs_shared::bridge::flatten(&tree)
    };

    let fake = fakedirector::install(
        &root,
        Fake::new()
            .with("data/served.esp", DIR_SERVED.to_vec(), ReadStyle::Whole)
            .writable_under("data/"),
        0,
    );

    let engine = Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let hooks = install(engine).expect("install");

    let served_status = ntapi::nt_delete_file(&root.join("data").join("served.esp").to_string_lossy());
    let unserved_status =
        ntapi::nt_delete_file(&root.join("data").join("unserved.bin").to_string_lossy());
    let outside_status = ntapi::nt_delete_file(&base.join("outside.txt").to_string_lossy());

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

    assert!(
        unserved_status < 0,
        "a delete the director refuses must be refused to the caller too, not silently \
         reported as done; got {unserved_status:#x}"
    );
    assert_eq!(
        fake.tally.deletes("data/unserved.bin"),
        1,
        "the refusal must be the director's own answer, asked for over the ring — a shim \
         that refused it locally would leave this at zero"
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

    let _ = std::fs::remove_dir_all(&base);
}

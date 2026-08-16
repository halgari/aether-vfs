//! `NtDeleteFile` when there is **no director** — the same containment, by the
//! shim's own overlay (gate 5, Task 5).
//!
//! This is not a hypothetical configuration. `FuseClient` is initialised from
//! the environment and can fail to attach; `vfs_env::READY_FUSE_FAILED_PREFIX`
//! exists precisely because a session can be released past that point. When it
//! happens the `Engine` still has every root the session declared and still
//! has its write overlay, and `setinfo_hook` has always converted a
//! *handle-based* delete under a root into an overlay whiteout on exactly that
//! basis. A path-based delete has to reach the same answer through the same
//! machinery, or the shim holds two predicates that disagree about the same
//! path — the failure this project has already paid for twice.
//!
//! Three cases, and each is distinguishable from "the hook did nothing":
//!
//! - `data/mod.esp` is under the root: the delete succeeds, the *real* file is
//!   untouched, and a whiteout marker appears in the overlay. The marker is
//!   what makes success mean "answered" rather than "swallowed" — the path is
//!   genuinely gone from the composed view afterwards.
//! - The **root directory itself** resolves under a root with an empty
//!   remainder, which `Engine::whiteout` declines. It must not fall through to
//!   the kernel on that account: a decline under a managed root is the escape,
//!   not a fallback, so the answer is `STATUS_ACCESS_DENIED`.
//! - A file outside every root must really be deleted.
//!
//! Its own binary, and deliberately with **no** `fakedirector`: this test's
//! whole subject is the branch taken when `fuse_client::global()` is `None`,
//! and that client is process-global and initialise-once.

mod ntapi;

use vfs_redirect::RootId;
use vfs_shim::{install, overlay_layer_dir, Engine};

const HOST_MOD: &[u8] = b"host: data/mod.esp";
const OUTSIDE: &[u8] = b"outside every root";

const STATUS_SUCCESS: i32 = 0;
/// `STATUS_ACCESS_DENIED`.
const STATUS_ACCESS_DENIED: i32 = 0xC000_0022u32 as i32;

#[test]
fn a_path_based_delete_is_contained_by_the_overlay_when_no_director_answers() {
    let base = std::env::temp_dir().join(format!("vfs-ntdelete-nodir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    let overlay = base.join("overlay");
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::create_dir_all(overlay_layer_dir(&overlay, RootId::DEFAULT)).unwrap();
    std::fs::write(root.join("data").join("mod.esp"), HOST_MOD).unwrap();
    std::fs::write(base.join("outside.txt"), OUTSIDE).unwrap();

    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "data/mod.esp".into(),
                kind: EntryKind::File,
                source: root.join("data").join("mod.esp").to_string_lossy().as_ref().into(),
                size: HOST_MOD.len() as u64,
                mtime: 0,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };

    let engine = Engine::with_overlay(
        root.to_str().unwrap(),
        overlay.to_str().unwrap(),
        snapshot,
    )
    .unwrap();
    let hooks = install(engine).expect("install");

    let under = ntapi::nt_delete_file(&root.join("data").join("mod.esp").to_string_lossy());
    let root_itself = ntapi::nt_delete_file(&root.to_string_lossy());
    let outside = ntapi::nt_delete_file(&base.join("outside.txt").to_string_lossy());

    drop(hooks);

    assert_eq!(
        std::fs::read(root.join("data").join("mod.esp")).ok().as_deref(),
        Some(HOST_MOD),
        "the real data/mod.esp was deleted — with no director the overlay whiteout is what \
         has to absorb the delete, exactly as it already does for a handle-based one"
    );
    assert_eq!(under, STATUS_SUCCESS, "the whiteout handled it; got {under:#x}");
    let marker = overlay_layer_dir(&overlay, RootId::DEFAULT)
        .join("data")
        .join(vfs_redirect::whiteout_marker("mod.esp"));
    assert!(
        marker.exists(),
        "no whiteout marker at {}: reporting success without one is a delete that comes back \
         on the next listing",
        marker.display()
    );

    assert_eq!(
        root_itself, STATUS_ACCESS_DENIED,
        "the managed root itself resolves with an empty remainder, which the overlay \
         declines — and a decline under a managed root must fail closed rather than hand \
         the path to the kernel; got {root_itself:#x}"
    );
    assert!(root.is_dir(), "the managed root directory itself was deleted");

    assert_eq!(
        outside, STATUS_SUCCESS,
        "a delete outside every managed root must still reach the real filesystem; \
         got {outside:#x}"
    );
    assert!(
        !base.join("outside.txt").exists(),
        "the file outside every root was not deleted — the hook is answering calls that are \
         none of its business"
    );

    let _ = std::fs::remove_dir_all(&base);
}

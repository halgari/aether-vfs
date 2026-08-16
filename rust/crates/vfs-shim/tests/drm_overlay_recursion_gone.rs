//! The unbounded recursion the DRM exceptions enabled is gone by construction
//! (gate 5, Task 6).
//!
//! **The hazard, as it was recorded.** `cow_seed_reentrancy.rs` documented a
//! latent stack overflow, reproduced while that test was being written:
//!
//! > the overlay copy of a DRM-named file is itself DRM-named, so every probe
//! > of it takes the exception too, is never sealed by the director, and
//! > resolves to a one-level-deeper overlay path — unbounded recursion,
//! > reproduced as a stack overflow while writing this.
//!
//! The mechanism, spelled out. `Engine::decide_open` asks
//! `Overlay::has_file`, which is `Overlay::lookup`'s
//! `std::fs::symlink_metadata` (`overlay.rs:118`) — an ordinary NT call, made
//! with the detours live and **not** under a `ShimIoGuard`. When the overlay
//! sits *under* a managed root, that probe names a path under the root, so the
//! hooks re-decide it. The probed path carries the same basename as the file
//! being copied up, so with the exceptions in place it matched the DRM list,
//! returned `None` from `try_fuse_create` before the ring, and came back to
//! `Engine::decide` — which resolved it one overlay level deeper and probed
//! again. Nothing terminated that.
//!
//! **Why it is gone.** The enabling condition was never the filename as such:
//! it was that *some* basename returned `None` before the ring, so the probe
//! was never sealed. Task 4 deleted the last such name. A probe of the overlay
//! copy is now an ordinary under-root open — the director does not serve
//! `__overlay/...`, so it is sealed with `STATUS_OBJECT_NAME_NOT_FOUND`,
//! `lookup` reads that as absent, and the walk stops at depth one.
//!
//! This test pins that rather than trusting it: it rebuilds the exact
//! configuration the hazard needed — overlay nested under the managed root, a
//! DRM-named file, a preserving write — and asserts the resolution lands
//! **one** overlay level deep. Termination alone would be a weak assertion (a
//! stack overflow simply kills the binary), so the depth is asserted
//! explicitly: the recursive version walked one level deeper per iteration, so
//! a target that is nested twice is the failure this test is watching for.
//!
//! **Scope, stated honestly.** This covers the default configuration, which is
//! the one Task 4 changed. A narrower version of the same shape survives behind
//! `VFS_ALLOW_DISK_FALLTHROUGH=1`, which un-seals under-root misses and so
//! restores "the probe is not sealed" for *every* filename, not just the four.
//! That switch is off by default, cleared defensively by `skyrim-live`, and
//! requires an overlay nested under a managed root — a layout no shipped
//! session uses. It is a pre-existing property of that opt-out rather than
//! anything the DRM exceptions contributed, and it is recorded in the Task 6
//! report rather than fixed here.

mod fakedirector;

use fakedirector::{Fake, ReadStyle};
use vfs_redirect::RootId;
use vfs_shim::{install, outcome_count, Engine, OpenOutcome};

const PROVIDER: &[u8] = b"bytes only the director has";

/// `GENERIC_WRITE` + `FILE_OPEN`: a preserving write to a file that must
/// already exist — the shape that drives `decide_open` into copy-up, and so
/// into the `has_file` probe that used to recurse.
const GENERIC_WRITE: u32 = 0x4000_0000;

#[test]
fn a_drm_named_file_with_the_overlay_under_the_root_resolves_one_level_deep() {
    let base = std::env::temp_dir().join(format!("vfs-drm-recur-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    // Deliberately UNDER the managed root: this is the layout that made the
    // recursion reachable at all.
    let overlay = root.join("__overlay");
    std::fs::create_dir_all(root.join("Data")).unwrap();
    std::fs::create_dir_all(&overlay).unwrap();
    // Pre-create the destination's parent so `ensure_parent` is out of the
    // frame and this test turns on the `has_file` probe and nothing else —
    // `cow_seed_reentrancy` does the same, for the same reason.
    let layer = vfs_shim::overlay_layer_dir(&overlay, RootId::DEFAULT);
    std::fs::create_dir_all(layer.join("data")).unwrap();

    std::env::set_var(vfs_env::SHIM_STATS_LOG, base.join("shim-stats.log"));
    std::env::set_var(vfs_env::SHIM_STATS_INTERVAL_MS, "3600000");
    // Left unset on purpose. Setting it would un-seal under-root misses and
    // restore the recursion for every filename — see the module doc's scope
    // note. The claim under test is about the default configuration.
    std::env::remove_var(vfs_env::ALLOW_DISK_FALLTHROUGH);

    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "unrelated.txt".into(),
                kind: EntryKind::File,
                source: r"D:\nowhere\unrelated.txt".into(),
                size: 0,
                mtime: 0,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };

    // The DRM name, at the exact spelling that used to take the exception.
    fakedirector::install(
        &root,
        Fake::new().with("data/steam_appid.txt", PROVIDER.to_vec(), ReadStyle::Whole),
        0,
    );

    let build_engine = || {
        Engine::with_overlay(root.to_str().unwrap(), overlay.to_str().unwrap(), snapshot.clone())
            .unwrap()
    };
    let hooks = install(build_engine()).expect("install");
    let engine = build_engine();

    // The call that recursed. Reaching it does not depend on the exception any
    // more (that was the old route in), so it is made directly — what is under
    // test is what happens *inside* it, with the detours live.
    let nt = format!(r"\??\{}", root.join("Data").join("steam_appid.txt").display());
    let decision = engine.decide_open(&nt, GENERIC_WRITE, vfs_redirect::FILE_OPEN);

    let drm_exceptions = outcome_count(OpenOutcome::FellThroughDrmException);
    drop(hooks);

    let vfs_redirect::Decision::Redirect { target_nt } = &decision else {
        panic!("a preserving write with an overlay configured must redirect; got {decision:?}");
    };

    // **The depth assertion.** One overlay level, at the path the first
    // resolution names. The recursive version produced a target nested one
    // level deeper per iteration before it exhausted the stack, so anything
    // but this exact path is the hazard resurfacing.
    let expected = vfs_redirect::to_nt(&layer.join("data").join("steam_appid.txt").to_string_lossy());
    assert_eq!(
        target_nt, &expected,
        "the overlay resolution walked past depth one — the probe of the overlay copy was \
         not sealed, which is the condition the unbounded recursion needed"
    );
    // Belt and braces on the mechanism itself: the overlay probe carries the
    // same DRM basename as the file being copied up, and it must not have been
    // excepted. This is the counter Task 4 kept wired precisely so this class
    // can be shown at zero rather than merely absent.
    assert_eq!(
        drm_exceptions, 0,
        "a probe of the overlay copy took a DRM exception, so it was never sealed — that is \
         exactly the enabling condition the recursion needed"
    );
    // And the copy-up really did happen, so the probe above was reached rather
    // than skipped: a test that never ran `has_file` would pass vacuously.
    let dest = layer.join("data").join("steam_appid.txt");
    assert_eq!(
        std::fs::read(&dest).unwrap_or_default(),
        PROVIDER,
        "copy-up did not materialise the overlay copy, so the `has_file` probe this test is \
         about may never have run"
    );

    let _ = std::fs::remove_dir_all(&base);
}

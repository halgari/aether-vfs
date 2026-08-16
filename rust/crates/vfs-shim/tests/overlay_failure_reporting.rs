//! An overlay mutation that fails must be explainable from the shim's own
//! stats report (gate 4, Task 6).
//!
//! `Overlay::ensure_parent` discarded its `create_dir_all` result. That reads
//! as harmless best-effort and is not: `Engine::decide_open` calls it and then
//! answers `Decision::Redirect` with a target *inside* the directory that was
//! never created, so the caller's open fails at the NT boundary and nothing
//! anywhere says why. The copy-up counters do not see it either — a
//! non-preserving (creating or truncating) write never runs copy-up at all,
//! which is exactly the shape this test drives.
//!
//! Same failure family as every other defect in this gate: invisible to a
//! green suite, visible only in a live session, and expensive to find by
//! bisection. So the assertion is not "a counter moved" but "the live
//! session's own report names the operation and the file".
//!
//! Its own binary, for the reasons `cow_seed_reporting.rs` gives:
//! `hookstats::enabled` resolves once per process, and the detours and
//! `FuseClient` are process-global.
//!
//! **How this reaches `Engine::decide_open`'s write branch** (rewritten by
//! gate 5, Task 6). Reaching it with a director attached needs an open that
//! `try_fuse_create` declines *before* answering from the ring. Until Task 4
//! that was the DRM/identity exception list (`steam_appid.txt` and friends,
//! matched on basename at any depth), and this fixture was named after one of
//! them for exactly that reason.
//!
//! Task 4 deleted those exceptions. The prediction attached to them — that
//! their removal would leave the shim-local overlay write path with no live
//! callers at all — turned out to be **wrong**, and Task 6 established the
//! correct answer by measurement: `VFS_ALLOW_DISK_FALLTHROUGH=1` still sends an
//! under-root `ST_NOT_FOUND` to `decision_for`, and that is now the sole route
//! from an NT open into this branch. It is an opt-out, off by default and
//! cleared defensively by `skyrim-live`, but it is a supported one that the
//! escape matrix relies on for real-disk stray detection — so the write path is
//! live machinery, not dead code, and this test is re-pointed at the route
//! rather than deleted.
//!
//! The filename is now an ordinary one, because the name no longer selects the
//! route: the switch does.

mod fakedirector;

use fakedirector::Fake;
use vfs_shim::{install, overlay_fail_count, Engine, OverlayFail};

#[test]
fn an_overlay_directory_that_cannot_be_created_names_itself_in_the_stats_report() {
    let base = std::env::temp_dir().join(format!("vfs-ovfail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    let overlay = base.join("overlay");
    let report = base.join("shim-stats.log");
    std::fs::create_dir_all(root.join("Data")).unwrap();
    std::fs::create_dir_all(&overlay).unwrap();

    // The failure, arranged physically: the overlay's `root-0/data`
    // subdirectory — the parent `ensure_parent` must create for a write to
    // `<root>\Data\mod.esp` — is occupied by a *file*, so
    // `create_dir_all` cannot succeed. A directory that is unwritable for any
    // ordinary reason (permissions, a full volume, a name collision like this
    // one) produces the same discarded error.
    let layer = vfs_shim::overlay_layer_dir(&overlay, vfs_redirect::RootId::DEFAULT);
    std::fs::create_dir_all(&layer).unwrap();
    std::fs::write(layer.join("data"), b"not a directory").unwrap();

    std::env::set_var(vfs_env::SHIM_STATS_LOG, &report);
    std::env::set_var(vfs_env::SHIM_STATS_INTERVAL_MS, "10");
    // The route in — see the module doc. Read once and cached, so it has to be
    // set before the first hooked open.
    std::env::set_var(vfs_env::ALLOW_DISK_FALLTHROUGH, "1");

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

    fakedirector::install(&root, Fake::new(), 0);

    let engine =
        Engine::with_overlay(root.to_str().unwrap(), overlay.to_str().unwrap(), snapshot).unwrap();
    let hooks = install(engine).expect("install");

    let before = overlay_fail_count(OverlayFail::EnsureParent);
    // `create_new` is `CREATE_NEW` -> `FILE_CREATE`, which
    // `vfs_redirect::classify_open` classifies as a write that does **not**
    // preserve. So `decide_open` skips copy-up entirely and the copy-up
    // counters — the only other thing in this crate that reports on the
    // overlay write path — stay silent. That is the gap this counter fills,
    // and asserting it below is what stops the new counter from being a
    // duplicate of the old one.
    let created = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(root.join("Data").join("mod.esp"));
    let after = overlay_fail_count(OverlayFail::EnsureParent);

    drop(hooks);

    // The harm is real, not hypothetical: the redirect pointed into a
    // directory that does not exist, so the caller's open failed.
    assert!(
        created.is_err(),
        "setup: the open must actually fail — an open that succeeded would mean the overlay \
         directory was created after all, and this test would be asserting nothing"
    );
    assert_eq!(
        after,
        before + 1,
        "the failed `create_dir_all` must be counted exactly once"
    );

    let mut body = String::new();
    for _ in 0..500 {
        body = std::fs::read_to_string(&report).unwrap_or_default();
        if body.contains("FAILED: overlay mkdir") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert!(
        body.contains("shim-local overlay failures"),
        "the report has no overlay-failure section, so this failure is still invisible in \
         the one place a live session prints outcomes.\n--- report ---\n{body}"
    );
    assert!(
        body.contains("FAILED: overlay mkdir"),
        "the report does not say *which* overlay operation failed.\n--- report ---\n{body}"
    );
    assert!(
        body.contains("root0/data/mod.esp"),
        "the report does not name the file whose write went nowhere.\n--- report ---\n{body}"
    );
    // The copy-up section must stay out of it: this write does not preserve,
    // so copy-up never ran. If this ever fires, the new counter is duplicating
    // an existing one rather than covering the gap it was added for.
    assert!(
        !body.contains("copy-on-write copy-ups"),
        "a non-preserving write must not produce a copy-up record; the overlay-failure \
         counter exists precisely because nothing else sees this open.\n--- report ---\n{body}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

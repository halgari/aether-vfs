//! **The write seal** (gate 4, Task 5): under a managed root, the director's
//! answer to a write open *is* the caller's answer. There is no second chance.
//!
//! Until this task, a write open the director would not serve returned `None`
//! from `try_fuse_create`, which sent `create_hook`/`open_hook` on to
//! `Engine::decide_open` — the shim-local overlay where one is configured, and
//! the real filesystem under the managed root where one is not. Either way the
//! bytes ended up somewhere the provider graph never agreed to and cannot
//! account for. This binary drives real `NtCreateFile`/`NtOpenFile` detours
//! against a real ring and asserts the four answers the boundary now gives, by
//! their exact Win32 error codes rather than by `io::ErrorKind` (Rust folds
//! `ERROR_FILE_NOT_FOUND` and `ERROR_PATH_NOT_FOUND` into the same `NotFound`,
//! which is precisely the distinction under test).
//!
//! One test function, one binary, on purpose: `ENGINE`, the detours, the
//! `FuseClient` and `hookstats::enabled()` are all process-global and
//! resolve-once (the `VA_LOCK` convention — a test asserting on process-global
//! state either takes the lock or lives alone). The steps also share state
//! deliberately: step 1 writes the file step 2 reads back.
//!
//! The overlay is configured here, which is the live shape (`skyrim-live` sets
//! one and mounts the same directory into the director's own graph). That
//! makes the refusals below the *strong* form of the claim: not "the write had
//! nowhere else to go", but "there was somewhere else to go and it went
//! nowhere".

mod fakedirector;

use std::io::Write;
use vfs_redirect::RootId;
use vfs_shim::{install, outcome_count, overlay_layer_dir, Engine, OpenOutcome};

/// `ERROR_FILE_NOT_FOUND` — `STATUS_OBJECT_NAME_NOT_FOUND`.
const ERROR_FILE_NOT_FOUND: i32 = 2;
/// `ERROR_PATH_NOT_FOUND` — `STATUS_OBJECT_PATH_NOT_FOUND`.
const ERROR_PATH_NOT_FOUND: i32 = 3;
/// `ERROR_ACCESS_DENIED` — `STATUS_ACCESS_DENIED`.
const ERROR_ACCESS_DENIED: i32 = 5;

const SERVED: &[u8] = b"bytes that crossed the ring";
const READ_ONLY_BYTES: &[u8] = b"served by a read-only layer";

#[test]
fn a_write_under_a_managed_root_is_answered_only_by_the_director() {
    let base = std::env::temp_dir().join(format!("vfs-write-seal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    let overlay = base.join("overlay");
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::create_dir_all(&overlay).unwrap();
    let ovl0 = overlay_layer_dir(&overlay, RootId::DEFAULT);
    std::fs::create_dir_all(&ovl0).unwrap();

    // Instrumentation on for the whole process: `hookstats::enabled()` is
    // resolved once and cached, so this must precede `install`. The interval
    // is set past this test's lifetime on purpose — the counters are read
    // through `outcome_count`, and a reporter thread writing files inside a
    // hooked process is noise this test does not need.
    let report = base.join("shim-stats.log");
    std::env::set_var(vfs_env::SHIM_STATS_LOG, &report);
    std::env::set_var(vfs_env::SHIM_STATS_INTERVAL_MS, "3600000");

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

    // A provider graph with one writable mount (`write/`) and one read-only
    // area (`data/`) — the ordinary modded-game shape, and the one that makes
    // the director's three different refusals reachable from one fixture.
    let fake = fakedirector::install(
        &root,
        fakedirector::Fake::new()
            .with(
                "data/readonly.esp",
                READ_ONLY_BYTES.to_vec(),
                fakedirector::ReadStyle::Whole,
            )
            .writable_under("write/"),
        0,
    );

    let engine =
        Engine::with_overlay(root.to_str().unwrap(), overlay.to_str().unwrap(), snapshot).unwrap();
    let hooks = install(engine).expect("install");

    // --- 1. A write the director CAN serve still succeeds ------------------
    //
    // Nothing pre-exists at this vpath: the create itself is what brings it
    // into the graph, through `OP_OPEN` with the disposition bits forwarded.
    let served = root.join("write").join("served.bin");
    {
        let mut f = std::fs::File::create(&served).expect(
            "a create under a writable mount must be served by the director — if this is \
             the failure, the seal has closed over writes that should still work, which is \
             a far worse outcome than the fall-through it replaced",
        );
        f.write_all(SERVED).unwrap();
    }

    // --- 2. …and is visible to a later read through the director -----------
    // (spec §8 criterion 4, first two clauses; the third is asserted after the
    // detours come down, at the end.)
    let read_back = std::fs::read(&served).expect("read back through the director");

    // --- 3. A create against a path no provider serves fails ---------------
    //
    // `data/` has no writable mount over it, so the director answers
    // `ST_NOT_FOUND` to a create there. The name is not what is missing — the
    // caller was about to supply it — so this is ERROR_PATH_NOT_FOUND.
    let unserved = root.join("data").join("unserved.bin");
    let err = std::fs::File::create(&unserved).expect_err(
        "a create under a managed root that no provider serves must fail, not fall through \
         to the shim-local overlay",
    );
    let unserved_errno = err.raw_os_error();

    // --- 4. A write open that did NOT ask to create is a plain not-found ---
    //
    // Same director status (`ST_NOT_FOUND`), inside the writable mount this
    // time, but with no `OPEN_CREATE`: the file itself is simply absent, and
    // the honest answer is the ordinary ERROR_FILE_NOT_FOUND — the answer the
    // "open for write, and on file-not-found create it" idiom needs.
    let absent = root.join("write").join("absent.bin");
    let absent_err = std::fs::OpenOptions::new()
        .write(true)
        .open(&absent)
        .expect_err("opening an absent file for write must fail");
    let absent_errno = absent_err.raw_os_error();

    // --- 5. A write to a path served by a read-only layer is denied --------
    let ro = root.join("data").join("readonly.esp");
    let ro_err = std::fs::OpenOptions::new()
        .write(true)
        .open(&ro)
        .expect_err("a write to a read-only mount must fail, not divert to the overlay");
    let ro_errno = ro_err.raw_os_error();
    // The control that stops the assertion above passing for the wrong
    // reason: the same path still *reads* fine through the director.
    let ro_read = std::fs::read(&ro);

    // --- 6. The counters ---------------------------------------------------
    let fell_through = outcome_count(OpenOutcome::FellThroughWriteFallback);
    let routed = outcome_count(OpenOutcome::Routed);

    // Everything below inspects the real filesystem under the managed root,
    // which is only visible with the detours down.
    drop(hooks);

    assert_eq!(
        read_back, SERVED,
        "a write through the director must be visible to a subsequent read through the \
         director (spec §8 criterion 4)"
    );
    assert_eq!(
        fake.contents("write/served.bin").as_deref(),
        Some(SERVED),
        "the bytes must land where the provider graph says they land — the director's own \
         copy is what a later session, and every other process, will see"
    );
    assert_eq!(
        fake.tally.writes("write/served.bin"),
        1,
        "the payload must have crossed the ring as an OP_WRITE; zero here with the right \
         bytes on disk means something else wrote them"
    );
    assert!(
        !served.exists(),
        "spec §8 criterion 4, third clause: {served:?} must NOT exist on the real \
         filesystem under the managed root — the provider graph's storage is elsewhere and \
         the root's real tree stays untouched"
    );

    assert_eq!(
        unserved_errno,
        Some(ERROR_PATH_NOT_FOUND),
        "a refused create must report ERROR_PATH_NOT_FOUND ({ERROR_PATH_NOT_FOUND}); got \
         {unserved_errno:?}"
    );
    assert!(
        !unserved.exists(),
        "the refused create left a real file at {unserved:?} — under a managed root the \
         real filesystem must be unreachable by any spelling"
    );
    assert_eq!(
        absent_errno,
        Some(ERROR_FILE_NOT_FOUND),
        "a write open with no create disposition against an absent file must report the \
         ordinary ERROR_FILE_NOT_FOUND ({ERROR_FILE_NOT_FOUND}), not the create-refused \
         ERROR_PATH_NOT_FOUND; got {absent_errno:?}"
    );
    assert_eq!(
        ro_errno,
        Some(ERROR_ACCESS_DENIED),
        "a write refused by a read-only mount (ST_READ_ONLY) must report \
         ERROR_ACCESS_DENIED ({ERROR_ACCESS_DENIED}) — what a real read-only filesystem \
         answers — rather than the generic failure the other director errors get; got \
         {ro_errno:?}"
    );
    assert_eq!(
        ro_read.unwrap_or_default(),
        READ_ONLY_BYTES,
        "the read-only path must still READ through the director; if this fails, the \
         access-denied assertion above proves nothing about writes specifically"
    );

    // Nothing anywhere in the overlay: not the served write (it crossed the
    // ring), and not one of the three refusals (they were refused, not
    // diverted). This is the bypass the gate closes, so an empty tree here is
    // the headline claim.
    let mut stray: Vec<std::path::PathBuf> = Vec::new();
    collect_files(&overlay, &mut stray);
    assert!(
        stray.is_empty(),
        "the shim-local overlay must be empty — every one of these is a write that escaped \
         the provider graph: {stray:?}"
    );

    assert_eq!(
        fell_through, 0,
        "`fell-through: write-fallback` must read zero: no write under a managed root may \
         leave the director's answer behind. The counter itself is deliberately still \
         wired (behind `VFS_ALLOW_DISK_FALLTHROUGH`, unset here) so this reads as a \
         measurement rather than as a constant"
    );
    assert!(
        routed > 0,
        "no under-root open was classified as `routed` at all, so the zero above is \
         vacuous — the whole fixture missed the director"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// Every file (not directory) beneath `dir`, recursively. A missing directory
/// yields nothing, which is fine: the caller already created it.
fn collect_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_files(&p, out);
        } else {
            out.push(p);
        }
    }
}

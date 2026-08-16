//! A copy-up that fails must be explainable from the shim's own stats report
//! (gate 4, task 4, round 1).
//!
//! Copy-up is best-effort and both callers discard its result: a director that
//! will not hand over a file's existing content leaves the game an empty
//! overlay file, or a not-found from a `FILE_OPEN`. Every other counter in
//! `hookstats` sees that as a perfectly ordinary write fall-through, so before
//! this the only trace of "the content went missing, and here is why" was
//! nothing at all.
//!
//! That is the shape of failure this gate keeps producing: invisible to a
//! green suite, visible only in a live session, and expensive to find by
//! bisection. So the point of this test is not that a counter increments —
//! it is that a **live session's own report names the file and the reason**,
//! which is the only form of observability that helps at the moment it
//! matters.
//!
//! Its own binary: it turns instrumentation on for the whole process
//! (`hookstats::enabled` is resolved once and cached), installs the
//! process-global detours, and installs the process-global `FuseClient`.
//!
//! ## The two routes in, and why there are two (gate 5, Task 6)
//!
//! This test used to reach copy-up through the DRM/identity exception list
//! (`steam_appid.txt` and friends), which returned `None` from
//! `try_fuse_create` before the ring was consulted. Its fixtures were named
//! after those exceptions for that reason, and the note here predicted that
//! when gate 5 closed them "copy-up has no live callers left".
//!
//! **That prediction was wrong, and Task 6 established the correct answer by
//! measurement rather than by argument.** Two facts came out of it:
//!
//! 1. `VFS_ALLOW_DISK_FALLTHROUGH=1` still sends an under-root `ST_NOT_FOUND`
//!    to `decision_for`, and it is now the *only* route from an NT open into
//!    copy-up. It is an opt-out, off by default and cleared defensively by
//!    `skyrim-live`, but supported and relied on by the escape matrix — so the
//!    copy-up machinery is live, not dead, and this test is re-pointed at that
//!    route rather than deleted.
//!
//! 2. **On that route copy-up can only ever fail.** The arm is entered
//!    precisely because the director answered `ST_NOT_FOUND` for that exact
//!    `(root, vpath)`; copy-up then asks the same director for the same path
//!    and gets the same answer. So the *seeded* half of this report — the half
//!    that distinguishes "never attempted" from "attempted and fine" — cannot
//!    be produced through a hook at all any more, and is driven here by calling
//!    `Engine::decide_open` directly, the way `cow_seed_reentrancy` does and
//!    for the same kind of reason. Both halves land in one process-global
//!    report, which is what the assertions read.
//!
//! The fixtures are ordinary filenames now: the name no longer selects the
//! route, the switch does.

mod fakedirector;

use fakedirector::{Fake, ReadStyle};
use vfs_shim::{install, Engine};

const PROVIDER: &[u8] = b"content the director does have";

/// `GENERIC_WRITE` + `FILE_OPEN` — a preserving write to a file that must
/// already exist, which is the shape that asks for a copy-up.
const GENERIC_WRITE: u32 = 0x4000_0000;

#[test]
fn a_failed_copy_up_names_the_file_and_the_reason_in_the_stats_report() {
    let base = std::env::temp_dir().join(format!("vfs-cowseed-report-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    let overlay = base.join("overlay");
    let report = base.join("shim-stats.log");
    std::fs::create_dir_all(root.join("Data")).unwrap();
    std::fs::create_dir_all(&overlay).unwrap();

    // Instrumentation on, and ticking fast enough for a millisecond-scale
    // test — `report_interval`'s documented reason for existing.
    std::env::set_var(vfs_env::SHIM_STATS_LOG, &report);
    std::env::set_var(vfs_env::SHIM_STATS_INTERVAL_MS, "10");
    // The hook route in — see the module doc. Cached on first read, so it must
    // be set before any hooked open.
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

    // `data/served.esp` is served, and is the seeded half. `data/missing.esp`
    // is not served at all, which is both what puts it on the fall-through
    // route and what makes its copy-up fail.
    fakedirector::install(
        &root,
        Fake::new().with("data/served.esp", PROVIDER.to_vec(), ReadStyle::Whole),
        0,
    );

    let build_engine = || {
        Engine::with_overlay(root.to_str().unwrap(), overlay.to_str().unwrap(), snapshot.clone())
            .unwrap()
    };
    let hooks = install(build_engine()).expect("install");
    // An identically-configured second instance to drive the seeded half from,
    // since `install` moves its engine into a `OnceLock` the crate does not
    // hand back. Same roots, same overlay directory, same fake director.
    let engine = build_engine();

    // --- the failure, through a real hooked open ---------------------------
    //
    // The director does not serve this path, so the write open answers
    // `ST_NOT_FOUND`; with the fall-through switch on that reaches
    // `decide_open`'s write branch, which runs copy-up — and copy-up asks the
    // same director for the same path, so there is nothing to seed.
    let missing = std::fs::OpenOptions::new()
        .append(true)
        .open(root.join("Data").join("missing.esp"));
    assert!(
        missing.is_err(),
        "setup: a preserving open of a path nothing serves must fail — that failure with \
         no explanation anywhere is the thing this test exists to prevent"
    );

    // --- the success, driven directly (see the module doc) -----------------
    let nt = format!(r"\??\{}", root.join("Data").join("served.esp").display());
    let decision = engine.decide_open(&nt, GENERIC_WRITE, vfs_redirect::FILE_OPEN);
    assert!(
        matches!(decision, vfs_redirect::Decision::Redirect { .. }),
        "setup: the preserving write must be redirected into the overlay, or no copy-up ran \
         and the seeded half of the report is vacuous; got {decision:?}"
    );

    // The report file is outside the root, but the assertions below read it
    // through `std::fs` and the reporter thread writes it the same way, so
    // take the detours down first rather than relying on that.
    drop(hooks);

    // The reporter rewrites the whole file every tick, so the tick already on
    // disk may predate the copy-ups above — waiting for "the section exists"
    // catches a snapshot taken between the two copy-ups and reads it as a
    // missing failure. Wait for the *last* thing recorded instead, then assert
    // on that same body.
    let mut body = String::new();
    for _ in 0..500 {
        body = std::fs::read_to_string(&report).unwrap_or_default();
        if body.contains("root0/data/served.esp") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert!(
        body.contains("copy-on-write copy-ups"),
        "the report has no copy-up section at all, so a copy-up failure is still invisible \
         in the one place a live session prints outcomes.\n--- report ---\n{body}"
    );
    // The reason, distinctly — not merely "a copy-up failed".
    assert!(
        body.contains("FAILED: director refused"),
        "the report does not say *why* the copy-up failed.\n--- report ---\n{body}"
    );
    // And the file, because "something failed" is what the silent version
    // already told you.
    assert!(
        body.contains("root0/data/missing.esp"),
        "the report does not name the file whose content went missing.\n--- report ---\n{body}"
    );
    // The success is recorded too: "did this one work?" is the other half of
    // explaining an empty file, and a report that only lists failures cannot
    // distinguish "never attempted" from "attempted and fine".
    assert!(
        body.contains("root0/data/served.esp") && body.contains("seeded"),
        "the report does not record the copy-up that succeeded.\n--- report ---\n{body}"
    );

    // Both copy-ups above issued their own `OP_OPEN` at the director — the
    // seeded one and the refused one alike — and neither carries an
    // `OpenOutcome::Routed`. Uncounted, they are silent negative drift in
    // `vfs-directord`'s `assert_reconciled`, which for four sessions has
    // reported any drift as a live bypass. This is the guard that keeps that
    // reconciliation an exact equality rather than a lie about a bypass.
    assert!(
        vfs_shim::unrouted_director_opens() >= 2,
        "copy-up's own director opens are not being counted (got {}); \
         `assert_reconciled` would read them as a bypass",
        vfs_shim::unrouted_director_opens()
    );
    // Matched by `vfs-directord`'s `tests/support/mod.rs` as a literal — the
    // reconciliation reads this row, so the row has to be in the report.
    assert!(
        body.contains("director-open: unrouted"),
        "the outcomes section has no unrouted-director-open row, so the \
         shim/director reconciliation cannot see these opens.\n--- report ---\n{body}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

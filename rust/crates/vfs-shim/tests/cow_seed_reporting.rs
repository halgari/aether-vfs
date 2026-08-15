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

mod fakedirector;

use fakedirector::{Fake, ReadStyle};
use std::io::Write;
use vfs_shim::{install, Engine};

const PROVIDER: &[u8] = b"content the director does have";

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

    // Read-only provider, so writes fall through to the shim-local overlay and
    // reach copy-up (see `Fake::read_only`). `present.esp` is served;
    // `absent.esp` is not, which is the failure this test is about.
    fakedirector::install(
        &root,
        Fake::new()
            .with("data/present.esp", PROVIDER.to_vec(), ReadStyle::Whole)
            .read_only(),
        0,
    );

    let engine =
        Engine::with_overlay(root.to_str().unwrap(), overlay.to_str().unwrap(), snapshot).unwrap();
    let hooks = install(engine).expect("install");

    // A copy-up that works...
    let ok = std::fs::OpenOptions::new()
        .append(true)
        .open(root.join("Data").join("present.esp"));
    if let Ok(mut f) = ok {
        let _ = f.write_all(b"!");
    }
    // ...and one that does not: the director does not serve this path, so
    // there is nothing to seed and the redirected open finds no overlay copy.
    let missing = std::fs::OpenOptions::new()
        .append(true)
        .open(root.join("Data").join("absent.esp"));
    assert!(
        missing.is_err(),
        "setup: a preserving open of a path nothing serves must fail — that failure with \
         no explanation anywhere is the thing this test exists to prevent"
    );

    // The report file is outside the root, but the assertions below read it
    // through `std::fs` and the reporter thread writes it the same way, so
    // take the detours down first rather than relying on that.
    drop(hooks);

    // The reporter rewrites the whole file every tick, so the tick already on
    // disk may predate the copy-ups above — waiting for "the section exists"
    // catches a snapshot taken between the two opens and reads it as a missing
    // failure. Wait for the *last* thing recorded instead, then assert on that
    // same body.
    let mut body = String::new();
    for _ in 0..500 {
        body = std::fs::read_to_string(&report).unwrap_or_default();
        if body.contains("root0/data/absent.esp") {
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
        body.contains("root0/data/absent.esp"),
        "the report does not name the file whose content went missing.\n--- report ---\n{body}"
    );
    // The success is recorded too: "did this one work?" is the other half of
    // explaining an empty file, and a report that only lists failures cannot
    // distinguish "never attempted" from "attempted and fine".
    assert!(
        body.contains("root0/data/present.esp") && body.contains("seeded"),
        "the report does not record the copy-up that succeeded.\n--- report ---\n{body}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

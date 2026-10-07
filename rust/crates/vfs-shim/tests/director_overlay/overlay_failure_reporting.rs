//! A write the provider graph *failed* (not refused) fails closed, even with disk fall-through
//! switched on, and the shim's own counters say where it went.
//!
//! This was `an_overlay_directory_that_cannot_be_created_names_itself_in_the_stats_report`
//! (gate 4, Task 6): with `VFS_ALLOW_DISK_FALLTHROUGH=1` an under-root write reached the
//! shim-local overlay, whose `ensure_parent` could fail silently, and the test made two claims
//! about that failure — it must not become a write somewhere the VFS cannot account for, and the
//! session's own instrumentation must show it. Task C8 removed the shim-local overlay, so there
//! is no shim-side mutation left to fail. The same two claims are made about the failure that is
//! left, a director error on the write open itself:
//!
//! - **Fails closed.** `VFS_ALLOW_DISK_FALLTHROUGH` relaxes "the director does not have this"
//!   (`ST_NOT_FOUND`), never "the director failed" (`hook/file_open.rs::try_fuse_create`). A
//!   provider I/O error on a write is `ERROR_GEN_FAILURE`, and nothing appears on the real
//!   filesystem under the root. The switch is on here, which is what makes this the strong form
//!   of the claim.
//! - **Accounted for.** The open is counted `Routed` (the director answered it) and
//!   `FellThroughWriteFallback` stays at zero.
//!
//! The control is a write the director does not have: with the switch on, that one *does*
//! reach the real file, so the refusal above is not an accident of nothing being writable.

use crate::fakedirector;

use fakedirector::Fake;
use vfs_shim::{install, outcome_count, OpenOutcome};

/// `ERROR_GEN_FAILURE` — `STATUS_UNSUCCESSFUL`.
const ERROR_GEN_FAILURE: i32 = 31;

#[test]
fn a_director_failure_on_a_write_fails_closed_even_with_disk_fallthrough_on() {
    isolate!();
    let base = std::env::temp_dir().join(format!("vfs-ovfail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    std::fs::create_dir_all(root.join("Data")).unwrap();

    // Counters on for the whole process (`hookstats::enabled` resolves once, so before
    // `install`); the reporter interval is pushed past the test.
    std::env::set_var(vfs_env::SHIM_STATS_LOG, base.join("shim-stats.log"));
    std::env::set_var(vfs_env::SHIM_STATS_INTERVAL_MS, "3600000");
    // The opt-out that un-seals under-root misses. A failure must stay sealed regardless.
    std::env::set_var(vfs_env::ALLOW_DISK_FALLTHROUGH, "1");

    fakedirector::install(&root, Fake::new().failing_writes_under("data/"), 0);
    let hooks = install().expect("install");

    let failed = root.join("Data").join("mod.esp");
    let failed_result = std::fs::write(&failed, b"bytes the provider could not take");
    // The control: a miss, which the switch does let through.
    let missed = root.join("missed.txt");
    let missed_result = std::fs::write(&missed, b"fell through");

    let fell_through = outcome_count(OpenOutcome::FellThroughWriteFallback);
    let routed = outcome_count(OpenOutcome::Routed);
    drop(hooks);

    let err = failed_result.expect_err("a write the director failed must fail");
    assert_eq!(
        err.raw_os_error(),
        Some(ERROR_GEN_FAILURE),
        "a provider failure on a write is ERROR_GEN_FAILURE, got {err:?}"
    );
    assert!(
        !failed.exists(),
        "a write the director failed reached the real filesystem under the root at {failed:?}: \
         the fall-through switch relaxes misses, never failures"
    );
    assert!(
        routed >= 1,
        "the failed write must be counted as answered by the director"
    );

    missed_result.expect("with the switch on, a write the director does not have falls through");
    assert!(
        missed.exists(),
        "the control did not reach the real file, so the refusal above may be an accident"
    );
    assert_eq!(
        fell_through, 1,
        "exactly one write fell through (the control); the failed one must not be among them"
    );

    let _ = std::fs::remove_dir_all(&base);
}

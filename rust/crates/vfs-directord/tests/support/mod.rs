//! Shared support for `vfs-directord`'s e2e tests.
//!
//! This is a `tests/support/mod.rs`, not a top-level `tests/*.rs` file, so
//! cargo does not compile it as its own (empty) test binary — only `e2e.rs`
//! pulls it in via `mod support;`.
//!
//! ## The reconciliation this closes
//!
//! Gate 1 of the bypass-removal plan measures, rather than removes, the
//! shim's silent disk fallback. Two earlier tasks built the two halves:
//!
//! - The shim (`vfs_shim::hookstats`) classifies every under-root open by
//!   which code path it took — `Routed` if it reached the director, or one
//!   of several `FellThrough*`/`Denied` outcomes if it didn't — and renders
//!   the counts into a `VFS_SHIM_STATS_LOG` report file on an interval
//!   (`VFS_SHIM_STATS_INTERVAL_MS`, default 250ms).
//! - The director (`vfs_director::io_stats`) counts every open that actually
//!   arrived over the ring, split into `opens_ok`/`opens_err`
//!   (`io_stats::open_totals()`).
//!
//! The invariant: the shim's `routed` count equals the director's total
//! arrived-open count, `opens_ok + opens_err`. Any drift means an open the
//! shim believed it routed never arrived at the director — a bypass, by
//! definition — or the reverse, an open the director served that the shim
//! never accounted for.
//!
//! ## A correction to this task's original interface, verified before
//! ## committing to it
//!
//! This task's brief stated the invariant as `routed == opens_ok` (the
//! success count alone). Wiring `assert_reconciled` to that literally
//! produces a false failure on both existing write-path e2e tests, every
//! run: measured before writing this note, `routed = 12`,
//! `opens_ok` delta `= 9`, `opens_err` delta `= 3`, in both the single- and
//! two-source tests — `9 + 3 = 12` exactly, not `9`.
//!
//! This is not a bypass. `vfs_shim::hook`'s `create_hook`/`open_hook` call
//! `note_open_outcome(OpenOutcome::Routed, ..)` unconditionally whenever
//! `try_fuse_create` returns `Some(status)`, *before* branching on whether
//! `status` is success or an NTSTATUS error the director legitimately
//! returned (see the `st >= 0` branch that only affects the trace log's
//! "ok"/"FAIL" label, not which outcome gets recorded). `Routed` means "this
//! open was forwarded to, and answered by, the director" — not "the
//! director said yes." `vfs-fixture-writepath`'s own header comment
//! documents exactly three deliberate error probes per run: a re-open of
//! the pre-rename name (must fail, proving the rename actually removed it),
//! a re-open of the deleted file (must fail, proving the delete actually
//! removed it), and a second `CREATE_NEW` against an already-existing path
//! (must fail with `AlreadyExists`, proving exclusivity). All three are real
//! `NtCreateFile`/`NtOpenFile` calls that cross the ring and get a genuine
//! negative answer back — correctly `Routed`, and correctly landing in the
//! director's `opens_err`, not `opens_ok`.
//!
//! `io_stats.rs`'s own doc comment on the `opens_ok`/`opens_err` fields
//! independently agrees with this reading: "the shim's `Routed` outcome
//! counter and this pair are meant to agree" — *this pair*, i.e. their sum,
//! not `opens_ok` in isolation. `assert_reconciled`'s `opens_ok` parameter
//! is named to match this task's originally specified signature, but the
//! value callers must pass is the director's *total* arrived-open count
//! (`opens_ok + opens_err`) — see the call sites in `e2e.rs`.
//!
//! ## Directory creates are out of scope for this comparison
//!
//! `OP_MKDIR` dispatches straight to `Director::mkdir` and back
//! (`vfs_director::ring_dispatch::dispatch_director`), never touching
//! `io_stats::record_open` — only the `OP_OPEN` arm does that. Shim-side,
//! `try_fuse_mkdir` never calls the outcome classifier either. The two
//! omissions cancel today (directory creates are invisible to *both*
//! counters, so their absence doesn't show up as drift), but that is a
//! coincidence of the current wiring on both sides, not a guarantee this
//! module encodes. If either side starts counting mkdir traffic without the
//! other following, this reconciliation would start reporting phantom
//! drift for a reason that has nothing to do with a real bypass — so this
//! module does not attempt to add mkdir traffic to either number, and ties
//! its scope to plain file opens only, same as `io_stats::record_open` and
//! the shim's `OpenOutcome` classifier both already do.
//!
//! ## `opens_err` already includes rejected writes — don't add them again
//!
//! `Director::open` calls `io_stats::record_rejected_write` for a write
//! against a read-only mount *and then* returns an error that
//! `ring_dispatch`'s `OP_OPEN` arm feeds into `record_open`'s error branch —
//! so a rejected write is already counted once in `opens_err` via the
//! ordinary error path. This module takes the director's `opens_err` (as
//! folded into the caller-supplied total, see above) exactly as
//! `io_stats::open_totals()` reports it; it never separately reads or adds
//! in `io_stats::rejected_writes()`, which would double-count the same
//! opens under a second name.
//!
//! ## Missing or partial report tolerance
//!
//! The reporter thread only rewrites the file on its interval tick; a
//! short-lived process can exit before ever ticking, leaving the report
//! absent (or, mid-tick, briefly a `.tmp` file that hasn't been renamed
//! yet — never a half-written target file, since the write is temp+rename).
//! `assert_reconciled` treats "file doesn't exist" and "file exists but has
//! no outcomes section" identically: both parse as `routed = 0` and an
//! empty fall-through map, rather than panicking on a read/parse error. If
//! that yields a real mismatch against a nonzero `opens_ok`, the assertion
//! below still fires — with a message that says the report was missing/
//! empty, not a confusing string-parsing panic.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Parsed result of reconciling the shim's self-reported open-outcome
/// counts against the director's own arrived-open count for the same
/// launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reconciliation {
    /// The shim's own `routed` outcome count, parsed from its report.
    pub routed: u64,
    /// Every `fell-through: *` / `denied` outcome present in the report,
    /// keyed by its rendered label. **Not** asserted to be zero here: gate 1
    /// only measures the bypass, it does not remove it. Gates 2-5 are the
    /// ones that drive each of these classes to zero, one at a time.
    pub fell_through: BTreeMap<String, u64>,
    /// The director's total arrived-open count for this launch
    /// (`opens_ok + opens_err`), as supplied by the caller — already
    /// isolated via a before/after delta on
    /// `vfs_director::io_stats::open_totals()` (see callers in `e2e.rs`).
    /// Named `opens_ok` to match this task's originally specified
    /// interface; see the module doc's "correction" section for why the
    /// value itself must be the sum, not `opens_ok` alone.
    pub opens_ok: u64,
    /// `routed as i64 - opens_ok as i64`. Always `0` on a `Reconciliation`
    /// returned normally, since `assert_reconciled` panics on any other
    /// value before returning — kept on the struct so a caller that wants
    /// to log or display it does not need to recompute it.
    pub drift: i64,
    /// Whether the report's "under-root open outcomes:" section header was
    /// found at all. Distinct from `fell_through.is_empty()`, which is
    /// equally true whether the section is entirely absent (report missing,
    /// reporter never ticked, or predates gate 1's classifier) or present
    /// with every count at zero — this field tells those apart.
    pub outcomes_section_found: bool,
}

const OUTCOMES_HEADER: &str = "under-root open outcomes:\n";

/// A line inside the outcomes section that is itself one outcome's summary
/// row (`"  {label:<32} {count:>8}\n"`, rendered by
/// `vfs_shim::hookstats::render_outcome`), as opposed to one of the deeper,
/// six-space-indented per-path breakdown lines (`format_outcome_paths`)
/// nested underneath it. Both start with at least two spaces, so the
/// distinguishing feature is the *exact* indent: a summary row's label
/// starts immediately after two spaces, a path row's count starts after
/// six.
fn parse_outcome_summary_line(line: &str) -> Option<(String, u64)> {
    if !line.starts_with("  ") || line.starts_with("   ") {
        return None;
    }
    let trimmed = line.trim();
    // The label field is left-padded to a fixed width and the count is
    // right-aligned in its own field, so the last run of whitespace in the
    // trimmed line always separates them — including for labels with an
    // internal ": " like "fell-through: redirect", since `rsplit_once` only
    // ever looks at the *last* match.
    let (label, count_str) = trimmed.rsplit_once(char::is_whitespace)?;
    let count = count_str.trim().parse::<u64>().ok()?;
    Some((label.trim().to_string(), count))
}

/// Parse the `routed` count and every fall-through/denied outcome out of a
/// shim stats report's text. Returns `(0, {}, false)` for text with no
/// outcomes section at all (including empty text), rather than erroring —
/// see the module doc's "missing or partial report" note.
fn parse_outcomes(text: &str) -> (u64, BTreeMap<String, u64>, bool) {
    let mut routed = 0u64;
    let mut fell_through = BTreeMap::new();
    let Some(idx) = text.find(OUTCOMES_HEADER) else {
        return (routed, fell_through, false);
    };
    // The outcomes section is rendered last in the report
    // (`hookstats::start_reporter`'s concatenation order), so it runs to
    // EOF — no need to bound the other end.
    let section = &text[idx + OUTCOMES_HEADER.len()..];
    for line in section.lines() {
        let Some((label, count)) = parse_outcome_summary_line(line) else {
            continue;
        };
        if label == "routed" {
            routed = count;
        } else {
            fell_through.insert(label, count);
        }
    }
    (routed, fell_through, true)
}

/// A nested per-path breakdown line under one outcome's summary row
/// (`"      {count:>6}x  {path}\n"`, rendered by
/// `vfs_shim::hookstats::format_outcome_paths`) — six-space indent, a count,
/// a literal `x`, two spaces, then the path. Distinct from the truncation
/// marker line (`"      ... and {n} more\n"`), which has no `x`-suffixed
/// count and is reported separately rather than mistaken for a path.
fn parse_outcome_path_line(line: &str) -> Option<&str> {
    // A summary row is indented exactly two spaces; a nested path row is
    // indented six, with the count then right-aligned *within* its own
    // 6-wide field on top of that — so the line's total leading whitespace
    // varies with the count's digit width and a bare `starts_with("      ")`
    // (a fixed-length prefix check) would accept a 2-space summary row too
    // once its own label happened to be short. Comparing indent *widths*
    // rather than literal prefixes is what actually distinguishes the two.
    let indent = line.len() - line.trim_start().len();
    if indent < 6 {
        return None;
    }
    let trimmed = line.trim();
    let (count_x, path) = trimmed.split_once("  ")?;
    let count_str = count_x.strip_suffix('x')?;
    count_str.trim().parse::<u64>().ok()?;
    Some(path)
}

/// Task 6's classification signal: every distinct (lowercased) path that
/// appears *anywhere* in the shim report's "under-root open outcomes"
/// section, regardless of which of the seven outcome classes it landed in.
///
/// This is deliberately not scoped to any one outcome label. Gate 2's own
/// exit criterion for the negative canary is "classified under-root", not
/// "reachable" and not "served" — a spelling that the director legitimately
/// answers `NotFound` for (`routed`, sealed) is exactly as much evidence of
/// correct classification as one that falls all the way through to
/// `fell-through: passthrough`, because both mean *some* counter saw the
/// open as ours. The only outcome that fails the exit criterion is total
/// absence — an open the shim's own accounting never mentions at all, which
/// is indistinguishable from ordinary background noise for a path genuinely
/// outside every managed root.
///
/// Returns the set alongside whether any outcome's path list was truncated
/// (`"... and N more"`, `format_outcome_paths`'s cap at
/// `OUTCOME_PATHS_SHOWN` = 20) — a caller asserting *absence* of a marker
/// from this set must know whether the list it searched was actually
/// complete.
pub fn classified_paths(shim_report: &Path) -> (BTreeSet<String>, bool) {
    let text = std::fs::read_to_string(shim_report).unwrap_or_default();
    let mut paths = BTreeSet::new();
    let mut truncated = false;
    let Some(idx) = text.find(OUTCOMES_HEADER) else {
        return (paths, truncated);
    };
    let section = &text[idx + OUTCOMES_HEADER.len()..];
    for line in section.lines() {
        if line.trim_start().starts_with("... and ") {
            truncated = true;
            continue;
        }
        if let Some(path) = parse_outcome_path_line(line) {
            paths.insert(path.to_ascii_lowercase());
        }
    }
    (paths, truncated)
}

/// Reconcile the shim's `routed` under-root-open count (read from
/// `shim_report`) against the director's total arrived-open count (supplied
/// by the caller as `opens_ok`, which must be `opens_ok + opens_err` — see
/// the module doc's "correction" section for why), and panic with a
/// message naming the drift if they disagree.
///
/// Does **not** assert anything about the fall-through counts beyond
/// parsing them — see the module doc. Callers that want to confirm the
/// fall-through section itself is present (as opposed to merely absent
/// because nothing fell through) should check the returned
/// `Reconciliation::outcomes_section_found`.
pub fn assert_reconciled(shim_report: &Path, opens_ok: u64) -> Reconciliation {
    let text = std::fs::read_to_string(shim_report).unwrap_or_default();
    let (routed, fell_through, outcomes_section_found) = parse_outcomes(&text);
    let drift = routed as i64 - opens_ok as i64;

    assert_eq!(
        drift,
        0,
        "shim/director open-count reconciliation failed: shim report at \
         {shim_report:?} parsed `routed` = {routed}, director arrived-open \
         total (opens_ok + opens_err) = {opens_ok} (drift = {drift}). A \
         nonzero drift means an open one side recorded never shows up on \
         the other — a live bypass, not a measurement quirk. (Directory \
         creates are explicitly out of scope for both counters and are not \
         part of this comparison — see this module's doc comment. Report \
         contents: {:?})",
        if text.is_empty() {
            "<missing or empty — reporter thread may not have ticked before \
             the process exited>"
                .to_string()
        } else {
            text
        }
    );

    Reconciliation {
        routed,
        fell_through,
        opens_ok,
        drift,
        outcomes_section_found,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors `vfs_shim::hookstats::render_outcome`'s exact format string
    /// (`"  {label:<32} {count:>8}\n"`), so this test fails if that shape
    /// ever drifts from what this parser expects.
    fn render_summary_row(label: &str, count: u64) -> String {
        format!("  {label:<32} {count:>8}\n")
    }

    #[test]
    fn parses_routed_and_fall_through_from_a_rendered_section() {
        let text = format!(
            "vfs-shim hook stats (pid 1)\nTOTAL 0 calls\n\n{OUTCOMES_HEADER}{}{}",
            render_summary_row("routed", 3),
            render_summary_row("fell-through: passthrough", 2),
        );
        let (routed, fell_through, found) = parse_outcomes(&text);
        assert_eq!(routed, 3);
        assert_eq!(fell_through.get("fell-through: passthrough"), Some(&2));
        assert!(found);
    }

    #[test]
    fn ignores_nested_per_path_breakdown_lines() {
        // Six-space-indented path rows must not be mistaken for a second
        // outcome row (their trailing token is "6x", not a bare count).
        let text = format!(
            "{OUTCOMES_HEADER}{}      {:>6}x  data/hello.txt\n",
            render_summary_row("routed", 1),
            1,
        );
        let (routed, fell_through, found) = parse_outcomes(&text);
        assert_eq!(routed, 1);
        assert!(fell_through.is_empty());
        assert!(found);
    }

    #[test]
    fn missing_section_parses_as_zero_not_an_error() {
        let (routed, fell_through, found) = parse_outcomes("vfs-shim hook stats (pid 1)\n");
        assert_eq!(routed, 0);
        assert!(fell_through.is_empty());
        assert!(!found);
    }

    #[test]
    fn empty_text_parses_as_zero_not_an_error() {
        let (routed, fell_through, found) = parse_outcomes("");
        assert_eq!(routed, 0);
        assert!(fell_through.is_empty());
        assert!(!found);
    }

    #[test]
    fn assert_reconciled_panics_on_drift_with_a_named_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        let report = dir.path().join("shim-stats.log");
        std::fs::write(
            &report,
            format!("{OUTCOMES_HEADER}{}", render_summary_row("routed", 1)),
        )
        .unwrap();
        let result = std::panic::catch_unwind(|| assert_reconciled(&report, 2));
        let err = result.expect_err("mismatched routed/opens_ok must panic");
        let msg = err
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_else(|| "<non-string panic payload>".into());
        assert!(msg.contains("drift"), "{msg}");
        assert!(msg.contains('1') && msg.contains('2'), "{msg}");
    }

    /// Mirrors `vfs_shim::hookstats::format_outcome_paths`'s exact row shape
    /// (`"      {c:>6}x  {p}\n"`).
    fn render_path_row(path: &str, count: u64) -> String {
        format!("      {count:>6}x  {path}\n")
    }

    #[test]
    fn classified_paths_collects_across_every_outcome_bucket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let report = dir.path().join("shim-stats.log");
        let text = format!(
            "{OUTCOMES_HEADER}{}{}{}{}",
            render_summary_row("routed", 2),
            render_path_row(r"\??\c:\root\data\A.esp", 1),
            render_summary_row("fell-through: passthrough", 1),
            render_path_row(r"\??\globalroot\device\harddiskvolume3\root\data\a.esp", 1),
        );
        std::fs::write(&report, text).unwrap();
        let (paths, truncated) = classified_paths(&report);
        assert!(!truncated);
        // Case-folded: a caller searching for a marker must not have to
        // guess whether the report happened to render upper- or lowercase.
        assert!(paths.contains(r"\??\c:\root\data\a.esp"));
        assert!(paths.contains(r"\??\globalroot\device\harddiskvolume3\root\data\a.esp"));
        assert_eq!(paths.len(), 2);
    }

    #[test]
    fn classified_paths_reports_truncation_rather_than_silently_dropping_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let report = dir.path().join("shim-stats.log");
        let text = format!(
            "{OUTCOMES_HEADER}{}{}      ... and 5 more\n",
            render_summary_row("routed", 6),
            render_path_row(r"\??\c:\root\data\a.esp", 1),
        );
        std::fs::write(&report, text).unwrap();
        let (paths, truncated) = classified_paths(&report);
        assert!(truncated);
        assert_eq!(paths.len(), 1);
    }

    #[test]
    fn classified_paths_empty_for_a_missing_report() {
        let dir = tempfile::tempdir().expect("tempdir");
        let report = dir.path().join("never-written.log");
        let (paths, truncated) = classified_paths(&report);
        assert!(paths.is_empty());
        assert!(!truncated);
    }

    #[test]
    fn assert_reconciled_tolerates_a_missing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let report = dir.path().join("never-written.log");
        // 0 routed, 0 opens_ok: reconciles trivially even though the file
        // was never created (the short-lived-process case).
        let recon = assert_reconciled(&report, 0);
        assert_eq!(recon.routed, 0);
        assert_eq!(recon.drift, 0);
        assert!(!recon.outcomes_section_found);
    }
}

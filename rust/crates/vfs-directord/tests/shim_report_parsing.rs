//! Unit tests for the shim-report parsers in `support/shim_report.rs`.
//!
//! They live in their own binary so the parsers run once, not once for every
//! test binary that pulls `support` in.

#[path = "support/shim_report.rs"]
#[allow(dead_code)]
mod shim_report;

use shim_report::*;

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
    let o = parse_outcomes(&text);
    assert_eq!(o.routed, 3);
    assert_eq!(o.fell_through.get("fell-through: passthrough"), Some(&2));
    assert_eq!(o.unrouted_director_opens, 0);
    assert!(o.found);
}

/// The unrouted-open row must land in its own field, not in
/// `fell_through`: it is not a fall-through, and every caller that reads
/// that map treats each key as one bypass class.
#[test]
fn unrouted_director_opens_parse_out_of_the_fall_through_map() {
    let text = format!(
        "{OUTCOMES_HEADER}{}{}",
        render_summary_row("routed", 5),
        render_summary_row(UNROUTED_OPEN_LABEL, 2),
    );
    let o = parse_outcomes(&text);
    assert_eq!(o.routed, 5);
    assert_eq!(o.unrouted_director_opens, 2);
    assert!(o.fell_through.is_empty(), "{:?}", o.fell_through);
}

/// The gate-4 invariant: the shim side of the comparison is
/// `routed + unrouted`, so a run with a directory downgrade or a copy-up
/// reconciles instead of reporting a phantom bypass.
#[test]
fn unrouted_director_opens_count_toward_the_directors_total() {
    let dir = vfs_testkit::tempdir().expect("tempdir");
    let report = dir.path().join("shim-stats.log");
    std::fs::write(
        &report,
        format!(
            "{OUTCOMES_HEADER}{}{}",
            render_summary_row("routed", 9),
            render_summary_row(UNROUTED_OPEN_LABEL, 3),
        ),
    )
    .unwrap();
    // 9 routed + 3 shim-issued = 12 arrivals at the director.
    let recon = assert_reconciled(&report, 12);
    assert_eq!(recon.drift, 0);
    assert_eq!(recon.unrouted_director_opens, 3);
}

/// …and it is still an equality, not a tolerance: one genuinely missing
/// open still fails, whatever the unrouted count is.
#[test]
fn an_unaccounted_open_still_fails_even_with_unrouted_opens_present() {
    let dir = vfs_testkit::tempdir().expect("tempdir");
    let report = dir.path().join("shim-stats.log");
    std::fs::write(
        &report,
        format!(
            "{OUTCOMES_HEADER}{}{}",
            render_summary_row("routed", 9),
            render_summary_row(UNROUTED_OPEN_LABEL, 3),
        ),
    )
    .unwrap();
    let result = std::panic::catch_unwind(|| assert_reconciled(&report, 13));
    let err = result.expect_err("one unaccounted arrival must still panic");
    let msg = err
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_else(|| "<non-string panic payload>".into());
    assert!(msg.contains("drift = -1"), "{msg}");
    // The message must name the non-bypass sources rather than asserting
    // a bypass outright — that claim was false from gate 4 onward.
    assert!(msg.contains("not *necessarily* one"), "{msg}");
    assert!(msg.contains("directory downgrade"), "{msg}");
    assert!(msg.contains("cow_seed"), "{msg}");
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
    let o = parse_outcomes(&text);
    assert_eq!(o.routed, 1);
    assert!(o.fell_through.is_empty());
    assert!(o.found);
}

#[test]
fn missing_section_parses_as_zero_not_an_error() {
    let o = parse_outcomes("vfs-shim hook stats (pid 1)\n");
    assert_eq!(o.routed, 0);
    assert_eq!(o.unrouted_director_opens, 0);
    assert!(o.fell_through.is_empty());
    assert!(!o.found);
}

#[test]
fn empty_text_parses_as_zero_not_an_error() {
    let o = parse_outcomes("");
    assert_eq!(o.routed, 0);
    assert_eq!(o.unrouted_director_opens, 0);
    assert!(o.fell_through.is_empty());
    assert!(!o.found);
}

#[test]
fn assert_reconciled_panics_on_drift_with_a_named_message() {
    let dir = vfs_testkit::tempdir().expect("tempdir");
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
    let dir = vfs_testkit::tempdir().expect("tempdir");
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
    let dir = vfs_testkit::tempdir().expect("tempdir");
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
    let dir = vfs_testkit::tempdir().expect("tempdir");
    let report = dir.path().join("never-written.log");
    let (paths, truncated) = classified_paths(&report);
    assert!(paths.is_empty());
    assert!(!truncated);
}

/// Hand-copied from `vfs_shim::hookstats::note_readdir`'s format string.
/// Being a copy, it cannot *detect* drift in `hookstats.rs` — both sides
/// would have to be edited for these tests to fail. What actually catches
/// a format change is the e2e test's `!ours.is_empty()` assertion, which
/// fails rather than passing vacuously when the parser stops matching.
/// These tests pin the parser's own behaviour against a known-good row.
fn render_readdir_row(source: &str, count: u64, filter: &str, dir: &str) -> String {
    format!("  {source:<9} {count:>4} entries  filter={filter:<16} {dir}\n")
}

#[test]
fn parses_every_readdir_row_with_its_source() {
    let text = format!(
        "vfs-shim hook stats (pid 1)\n\ndirectory enumerations (3):\n{}{}{}\n\
         under-root open outcomes:\n",
        render_readdir_row("director", 2, "*", r"\??\c:\root\games\skyrim\data"),
        render_readdir_row("contained", 0, "*.esp", r"\??\c:\root\other"),
        render_readdir_row("OS", 0, "*", r"c:\windows\system32"),
    );
    let rows = parse_readdirs(&text);
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[0].source, "director");
    assert_eq!(rows[0].count, 2);
    assert_eq!(rows[0].filter, "*");
    assert_eq!(rows[0].dir, r"\??\c:\root\games\skyrim\data");
    assert_eq!(rows[1].source, "contained");
    assert_eq!(rows[1].filter, "*.esp");
    assert_eq!(rows[2].source, "OS");
    // The section stops at the next section rather than swallowing it.
    assert!(rows.iter().all(|r| !r.dir.contains("outcomes")));
}

#[test]
fn readdir_rows_keep_directory_paths_containing_spaces_intact() {
    let text = format!(
        "\ndirectory enumerations (1):\n{}",
        render_readdir_row("director", 1, "*", r"\??\c:\program files\game\data"),
    );
    let rows = parse_readdirs(&text);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].dir, r"\??\c:\program files\game\data");
}

#[test]
fn missing_readdir_section_parses_as_empty_not_an_error() {
    assert!(parse_readdirs("vfs-shim hook stats (pid 1)\n").is_empty());
    assert!(parse_readdirs("").is_empty());
}

#[test]
fn assert_reconciled_tolerates_a_missing_file() {
    let dir = vfs_testkit::tempdir().expect("tempdir");
    let report = dir.path().join("never-written.log");
    // 0 routed, 0 opens_ok: reconciles trivially even though the file
    // was never created (the short-lived-process case).
    let recon = assert_reconciled(&report, 0);
    assert_eq!(recon.routed, 0);
    assert_eq!(recon.drift, 0);
    assert!(!recon.outcomes_section_found);
}

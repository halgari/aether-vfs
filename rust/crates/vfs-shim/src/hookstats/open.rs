//! What happened to each open: passthrough paths, outcomes, undecodable names, stats and the trace.

use super::*;

/// How often each path was opened, capped so a storm cannot grow this without
/// bound.
///
/// A hook count alone cannot say what a stalled game is hunting for: a world
/// load issued 25k `NtCreateFile` with only 210 resolving under the root, and
/// the useful question is which paths the other 24.9k were. A *distinct* list
/// could not answer that either — a process wedged in a retry loop reopens one
/// path thousands of times, and dedup hides exactly the path that matters. So
/// this counts repeats and reports the busiest first.
pub(super) static PATHS: BoundedTally<String> = BoundedTally::new(4000);
/// How many of the busiest paths to print. Generous: the question this answers
/// is usually "did the process ever touch X", and a path asked for once is
/// exactly the interesting case when X is a file that should have loaded.
pub(super) const PATHS_SHOWN: usize = 1000;

/// Record a path an open was attempted on. Cheap no-op when disabled.
pub(crate) fn note_passthrough(path: &str) {
    if !enabled() {
        return;
    }
    // Past the cap we stop learning new paths but keep counting known ones,
    // so a loop that started early still shows its true rate.
    PATHS.add(path.to_ascii_lowercase());
}

/// Busiest-first rendering, split out so the ordering is testable without
/// touching the process-wide map (which is inert unless instrumentation is on).
pub(super) fn format_paths(mut pairs: Vec<(String, u64)>) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    let distinct = pairs.len();
    let total: u64 = pairs.iter().map(|(_, c)| *c).sum();
    // Count descending, then path so equal counts do not reorder between
    // snapshots — a diff of two reports is how a loop's rate gets measured.
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let shown = pairs.len().min(PATHS_SHOWN);
    let mut s =
        format!("\nopen paths by frequency (top {shown} of {distinct} distinct, {total} opens):\n");
    for (p, c) in pairs.into_iter().take(shown) {
        s.push_str(&format!("  {c:>8}x  {p}\n"));
    }
    s
}

/// Opens whose path could not be decoded, keyed by the bare object name.
///
/// A relative open names its parent only by handle; if that handle is unknown
/// the call cannot be matched against the root, cannot be served, and appears
/// in no path-keyed report. Counting them says whether anything is hiding.
pub(super) static UNDECODABLE: BoundedTally<String> = BoundedTally::unbounded();

pub(crate) fn note_undecodable(name: Option<&str>) {
    if !enabled() {
        return;
    }
    UNDECODABLE.add(name.unwrap_or("<no name>").to_ascii_lowercase());
}

pub(super) fn render_undecodable(snap: &Snapshot) -> String {
    let map = &snap.undecodable;
    if map.is_empty() {
        return String::new();
    }
    let mut rows: Vec<(&String, &u64)> = map.iter().collect();
    rows.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let total: u64 = rows.iter().map(|(_, c)| **c).sum();
    let mut s = format!(
        "
undecodable opens ({} distinct, {total} calls):
",
        rows.len()
    );
    for (k, c) in rows.iter().take(60) {
        s.push_str(&format!(
            "  {c:>6}x  {k}
"
        ));
    }
    s
}

/// Ordered log of operations against the managed root.
///
/// Counts say *what* was touched; only order says *where a sequence stopped*.
/// A load that gives up part way looks identical in a frequency table to one
/// that never started, because both simply lack the entries that would have
/// followed.
pub(super) static TRACE: Mutex<Option<Vec<String>>> = Mutex::new(None);
pub(super) const TRACE_MAX: usize = 4000;

pub(crate) fn note_trace(op: &str, path: &str, result: &str) {
    if !enabled() {
        return;
    }
    let Ok(mut g) = TRACE.lock() else { return };
    let v = g.get_or_insert_with(Vec::new);
    if v.len() >= TRACE_MAX {
        return;
    }
    v.push(format!(
        "{:<10} {:<12} {}",
        op,
        result,
        path.to_ascii_lowercase()
    ));
}

pub(super) fn render_trace(snap: &Snapshot) -> String {
    let v = &snap.trace;
    if v.is_empty() {
        return String::new();
    }
    let mut s = format!(
        "
ordered trace of under-root operations ({}):
",
        v.len()
    );
    for (i, line) in v.iter().enumerate() {
        s.push_str(&format!(
            "  {i:>5}  {line}
"
        ));
    }
    s
}

/// Attribute queries against the managed root, with their outcome.
///
/// A stat is how a caller asks "does this exist" without opening it, so a stat
/// that wrongly says no is invisible in every open-side counter: the file is
/// simply never requested. Skyrim validates its load order this way and drops
/// any plugin whose stat fails, so "never opened Skyrim.esm" and "stat said
/// Skyrim.esm is missing" look identical from the open path.
/// Keyed by outcome + path and counted, so recording *every* stat — including
/// the thousands of Windows DLL probes — stays bounded by distinct paths.
pub(super) static STATS: BoundedTally<String> = BoundedTally::new(4000);

pub(crate) fn note_stat(path: &str, outcome: &str) {
    if !enabled() {
        return;
    }
    note_trace("stat", path, outcome);
    STATS.add(format!("{:<12} {}", outcome, path.to_ascii_lowercase()));
}

pub(super) fn render_stats(snap: &Snapshot) -> String {
    let map = &snap.stats;
    if map.is_empty() {
        return String::new();
    }
    // Sorted by key so the outcome groups together and two reports diff cleanly.
    let mut rows: Vec<(&String, &u64)> = map.iter().collect();
    rows.sort_by(|a, b| a.0.cmp(b.0));
    let mut s = format!("\nattribute queries ({} distinct):\n", rows.len());
    for (k, c) in rows {
        s.push_str(&format!("  {c:>6}x  {k}\n"));
    }
    s
}

/// Which code path an under-root open actually took.
///
/// The shim's decision for an open under the managed root is not binary:
/// besides being routed to the director, it can fall through to the real
/// filesystem for several *different* reasons — a redirect that resolved to
/// nothing, the generic pass-through default, a DRM host-exe exception, or the
/// write-fallback path — or be denied outright. A single "fell through" counter
/// cannot tell which of those happened, and that distinction is the entire
/// point: gates 2-5 each remove exactly one of these classes, and only a
/// counter that stays distinct per class can show that the gate which removed a
/// class actually drove it to zero, without also masking a regression in a
/// class that gate did not touch. `FellThroughRedirect`/`FellThroughServe` are
/// gate 3's, `FellThroughPassthrough` is gates 2 and 3's,
/// `FellThroughWriteFallback` is gate 4's, and `FellThroughDrmException` was
/// gate 5's.
///
/// **`FellThroughServe` can no longer be recorded.** Gate 4 task 7 deleted
/// `Decision::Serve` and the in-shim zip-window server it fed, so nothing
/// increments it. The variant is kept rather than removed because the
/// discriminants index `OUTCOME_COUNTS` and the audit tables in
/// `docs/bypass-baseline.md` are written against these positions; renumbering
/// them to retire a counter that already read zero in every measured run would
/// invalidate that record for no gain. Read a zero here as "route removed",
/// not "route measured and unexercised".
///
/// **`FellThroughDrmException` can no longer be recorded either, and is in
/// exactly the same state.** Gate 5 task 4 deleted the four filename
/// exceptions in `try_fuse_create` that were its only increment site, so a
/// zero here also means "route removed", not "route measured and unexercised".
/// The distinction is what the acceptance evidence turns on: a reader who takes
/// this zero for a measured-and-clean run would be crediting the counter with
/// proving something no counter can prove about a code path that no longer
/// exists. What the retained variant *does* buy is the other direction — it
/// cannot silently start counting again without a report saying so, and the
/// shim/director reconciliation asserts on it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum OpenOutcome {
    Routed = 0,
    FellThroughRedirect = 1,
    FellThroughServe = 2,
    FellThroughPassthrough = 3,
    FellThroughDrmException = 4,
    FellThroughWriteFallback = 5,
    Denied = 6,
}

pub(super) const OUTCOME_N: usize = 7;

/// Every variant, for iteration in `render_outcomes` and for the test that
/// checks labels stay distinct.
pub(crate) const ALL_OUTCOMES: [OpenOutcome; OUTCOME_N] = [
    OpenOutcome::Routed,
    OpenOutcome::FellThroughRedirect,
    OpenOutcome::FellThroughServe,
    OpenOutcome::FellThroughPassthrough,
    OpenOutcome::FellThroughDrmException,
    OpenOutcome::FellThroughWriteFallback,
    OpenOutcome::Denied,
];

impl OpenOutcome {
    /// Rendered label. Must stay distinct across variants — see
    /// `every_outcome_renders_with_a_distinct_label` — or a gate's removal of
    /// one bypass class would be indistinguishable from another's in the
    /// report.
    pub fn label(&self) -> &'static str {
        match self {
            OpenOutcome::Routed => "routed",
            OpenOutcome::FellThroughRedirect => "fell-through: redirect",
            OpenOutcome::FellThroughServe => "fell-through: serve",
            OpenOutcome::FellThroughPassthrough => "fell-through: passthrough",
            OpenOutcome::FellThroughDrmException => "fell-through: drm-exception",
            OpenOutcome::FellThroughWriteFallback => "fell-through: write-fallback",
            OpenOutcome::Denied => "denied",
        }
    }
}

pub(super) static OUTCOME_COUNTS: [AtomicU64; OUTCOME_N] = [const { AtomicU64::new(0) }; OUTCOME_N];

/// Paths seen for each outcome, bounded the same way `PATHS` is: past the cap
/// we stop learning new paths but keep counting known ones, so an early-
/// starting loop still shows its true rate.
pub(super) static OUTCOME_PATHS: [BoundedTally<String>; OUTCOME_N] =
    [const { BoundedTally::new(4000) }; OUTCOME_N];
/// How many of the busiest paths to print per outcome. Smaller than
/// `PATHS_SHOWN`: this table prints one such list per outcome, so it must
/// stay skimmable rather than repeat the full passthrough dump seven times.
///
/// **Not purely cosmetic**: `vfs-directord`'s escape matrix locates each
/// vector's own attempt in this list and asserts the list did not truncate
/// (`... and N more`) for a run that small, because a per-vector search
/// against a truncated list proves nothing. Raised from 20 when the matrix
/// grew the three object-manager spellings vector 3 never covered
/// (`3b`/`3c`/`3d`) and the write matrix's busiest bucket — one distinct raw
/// spelling per vector, plus the handful of incidental opens every launch
/// makes — crossed the old cap. Widen it again rather than shrinking the
/// matrix if that happens next time: the cap exists for skimmability, the
/// matrix's coverage does not bend to it.
pub(super) const OUTCOME_PATHS_SHOWN: usize = 40;

/// Current value of one outcome's counter. `pub` (not `#[cfg(test)]`) so a
/// future gate's own tests can assert a class went to zero without reaching
/// into the atomics directly.
pub fn outcome_count(outcome: OpenOutcome) -> u64 {
    OUTCOME_COUNTS[outcome as usize].load(Ordering::Relaxed)
}

/// `OP_OPEN`s the shim issued that no [`OpenOutcome::Routed`] accounts for.
///
/// This exists to keep one specific invariant true. Four recorded sessions
/// have used `routed == opens_ok + opens_err` (the director's own arrived-open
/// total) as this project's health check, on the reading that any drift means
/// an open one side saw and the other did not — a bypass. Gate 4 added two
/// places where the shim asks the director to open something *without* that
/// open being a `Routed` decision, which breaks the equality without any
/// bypass existing:
///
///  - **The directory downgrade** (`hook/file_open.rs`): a write-flavoured open of a
///    directory is re-issued as a read open, so one `Routed` produces two
///    `OP_OPEN`s.
///  - **Copy-up** (`Engine::cow_seed` → `seed_from_director`): the shim opens
///    the file itself to read its prior content. That open is the shim's, not
///    the game's, so nothing ever classified it as an outcome.
///
/// Counting them rather than tolerating them keeps the reconciliation exact:
/// `routed + unrouted_director_opens == opens_ok + opens_err`, still an
/// equality, so a real bypass of one open still fails it. A tolerance would
/// have hidden exactly the thing the check is for.
///
/// Gated on `enabled()` like every other counter here, so it stays consistent
/// with `routed` — both are absent together or present together.
pub(super) static UNROUTED_DIRECTOR_OPENS: AtomicU64 = AtomicU64::new(0);

/// Rendered label for [`UNROUTED_DIRECTOR_OPENS`], inside the outcomes
/// section so one parse of that section yields both halves of the
/// reconciliation. Deliberately not an `OpenOutcome` variant: it does not
/// classify a *game* open the way the others do, and giving it a discriminant
/// would renumber `OUTCOME_COUNTS` against the audit tables in
/// `docs/bypass-baseline.md`.
///
/// `vfs-directord`'s `tests/support/mod.rs` matches this string. A rename
/// there without one here turns the reconciliation back into a silent
/// inequality.
pub(crate) const UNROUTED_OPEN_LABEL: &str = "director-open: unrouted";

/// Record an `OP_OPEN` the shim issued on its own behalf, or a re-issue of one
/// already counted as `Routed`. See [`UNROUTED_DIRECTOR_OPENS`].
pub(crate) fn note_unrouted_director_open() {
    if !enabled() {
        return;
    }
    UNROUTED_DIRECTOR_OPENS.fetch_add(1, Ordering::Relaxed);
}

/// Current value of [`UNROUTED_DIRECTOR_OPENS`], for in-process tests that
/// assert on it directly rather than through the rendered report.
pub fn unrouted_director_opens() -> u64 {
    UNROUTED_DIRECTOR_OPENS.load(Ordering::Relaxed)
}

/// Record which path an under-root open actually took. Cheap no-op when
/// disabled, exactly like `note_passthrough`.
///
/// Wired from every under-root decision site in `hook/file_open.rs`'s `create_hook` /
/// `open_hook` / `try_fuse_create` — see those for the full site-by-site
/// argument that each open records exactly once.
pub(crate) fn note_open_outcome(outcome: OpenOutcome, path: &str) {
    if !enabled() {
        return;
    }
    let idx = outcome as usize;
    OUTCOME_COUNTS[idx].fetch_add(1, Ordering::Relaxed);
    OUTCOME_PATHS[idx].add(path.to_ascii_lowercase());
}

/// Busiest-first rendering of one outcome's paths, capped at
/// `OUTCOME_PATHS_SHOWN` with the remainder called out explicitly — a
/// truncated list silently presented as complete would make every later gate
/// measure against a count that is quietly wrong.
pub(super) fn format_outcome_paths(mut pairs: Vec<(String, u64)>) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    let distinct = pairs.len();
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let shown = pairs.len().min(OUTCOME_PATHS_SHOWN);
    let mut s = String::new();
    for (p, c) in pairs.iter().take(shown) {
        s.push_str(&format!("      {c:>6}x  {p}\n"));
    }
    if distinct > shown {
        s.push_str(&format!("      ... and {} more\n", distinct - shown));
    }
    s
}

/// One outcome's section: label, total count, and its busiest paths.
pub(super) fn render_outcome(outcome: OpenOutcome, snap: &Snapshot) -> String {
    let idx = outcome as usize;
    let count = snap.outcome_counts[idx];
    if count == 0 {
        return String::new();
    }
    let paths = snap.outcome_paths[idx]
        .iter()
        .map(|(p, c)| (p.clone(), *c))
        .collect();
    format!(
        "  {:<32} {count:>8}\n{}",
        outcome.label(),
        format_outcome_paths(paths)
    )
}

/// Under-root open outcomes, one section per class so a gate that removes one
/// bypass can be checked in isolation from the others.
pub(super) fn render_outcomes(snap: &Snapshot) -> String {
    let mut body = String::new();
    for outcome in ALL_OUTCOMES {
        body.push_str(&render_outcome(outcome, snap));
    }
    // Same row shape as an outcome so the section stays parseable by one
    // rule, but not an outcome — see `UNROUTED_DIRECTOR_OPENS`. Omitted at
    // zero, like every outcome row.
    if snap.unrouted_director_opens > 0 {
        body.push_str(&format!(
            "  {UNROUTED_OPEN_LABEL:<32} {:>8}\n",
            snap.unrouted_director_opens
        ));
    }
    if body.is_empty() {
        return String::new();
    }
    format!("\nunder-root open outcomes:\n{body}")
}

pub(super) fn render_passthrough(snap: &Snapshot) -> String {
    format_paths(
        snap.passthrough
            .iter()
            .map(|(p, c)| (p.clone(), *c))
            .collect(),
    )
}

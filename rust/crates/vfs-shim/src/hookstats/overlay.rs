//! Copy-up and shim-local overlay mutation counters.

use super::*;

/// How a copy-on-write copy-up ended.
///
/// Copy-up (`Engine::cow_seed`) reads a file's existing content through the
/// director so a preserving write can start from it. Every way it can decline
/// or fail is silent from every other vantage point in this module: the open
/// that triggered it is still answered with a `Redirect`, and the game simply
/// receives an empty overlay file (or, for `FILE_OPEN`, a not-found from the
/// redirected open). Nothing says the content went missing, and nothing says
/// why.
///
/// Which outcome that open is counted as has changed twice. Gate 4's Task 5
/// made it `FellThroughDrmException`, because the DRM/identity filename
/// exceptions were then the only route that reached copy-up in a normal
/// session. Gate 5's Task 4 deleted those exceptions, so that answer is stale
/// too. **The live answer is `FellThroughWriteFallback` again**, and it is
/// reachable only behind the `allow_disk_fallthrough` opt-out, off by default:
/// that switch is now the sole route from an NT open into copy-up (measured,
/// gate 5 Task 6).
///
/// **And on that route copy-up can only ever fail.** The arm is entered
/// precisely because the director answered `ST_NOT_FOUND` for that exact
/// `(root, vpath)`; copy-up then asks the same director for the same path and
/// gets the same answer. `Engine::rename`'s copy-up cannot seed either: a
/// non-synthetic under-root handle reaches it only by the same fall-through, or
/// off the `Decision::Redirect` arm — and there the overlay copy already
/// exists, so `has_file` short-circuits before copy-up is called. A copy-up
/// that actually *seeds* is therefore reachable only by calling the `Engine`
/// API directly, which is what `cow_seed_reads_through_director` and
/// `cow_seed_reentrancy` do.
///
/// That matters more here than the counter's size suggests. This gate's
/// defects have all been invisible to a green test suite and visible only in a
/// live session, and "the game's save/ini/plugin file came back empty" is
/// exactly that shape. So the reasons are kept **distinct** rather than
/// collapsed into one failure count: "the director does not have this file"
/// (ordinary, and the invariant working as intended for content no provider
/// serves), "the read failed part-way" (a director hiccup mid-session) and "no
/// ring at all" (a misconfigured launch) call for completely different
/// responses, and a single counter cannot tell them apart.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum CopyUp {
    /// The destination holds the director's bytes, in full.
    Seeded = 0,
    /// No `FuseClient` — nothing was configured to read from.
    DeclinedNoDirector = 1,
    /// Shim-initiated I/O was already in flight on this thread; copy-up
    /// declined rather than recursing (see `hook::ShimIoGuard`).
    DeclinedReentrant = 2,
    /// The path named an alternate data stream. The resolved remainder has no
    /// stream in it, so seeding would copy the *base* file's content into a
    /// write aimed at a named stream.
    DeclinedStream = 3,
    /// The director's OPEN failed — usually not-found, i.e. no provider serves
    /// this path. The empty overlay file the caller then gets is consistent
    /// with the not-found the same path reads as.
    DirectorRefused = 4,
    /// OPEN succeeded and the read did not: an error status, or fewer bytes
    /// than OPEN promised. The partial destination is removed.
    ReadFailed = 5,
    /// The destination file could not be created or written.
    DestWriteFailed = 6,
}

pub(super) const COPYUP_N: usize = 7;

/// Every variant, for iteration in `render_copy_ups` and the label test.
pub const ALL_COPY_UPS: [CopyUp; COPYUP_N] = [
    CopyUp::Seeded,
    CopyUp::DeclinedNoDirector,
    CopyUp::DeclinedReentrant,
    CopyUp::DeclinedStream,
    CopyUp::DirectorRefused,
    CopyUp::ReadFailed,
    CopyUp::DestWriteFailed,
];

impl CopyUp {
    /// Rendered label. Distinct across variants — see
    /// `every_copy_up_outcome_renders_with_a_distinct_label` — for the same
    /// reason `OpenOutcome::label` is.
    pub fn label(&self) -> &'static str {
        match self {
            CopyUp::Seeded => "seeded",
            CopyUp::DeclinedNoDirector => "declined: no director",
            CopyUp::DeclinedReentrant => "declined: reentrant",
            CopyUp::DeclinedStream => "declined: stream suffix",
            CopyUp::DirectorRefused => "FAILED: director refused",
            CopyUp::ReadFailed => "FAILED: read",
            CopyUp::DestWriteFailed => "FAILED: destination write",
        }
    }
}

pub(super) static COPYUP_COUNTS: [AtomicU64; COPYUP_N] = [const { AtomicU64::new(0) }; COPYUP_N];
pub(super) static COPYUP_BYTES: AtomicU64 = AtomicU64::new(0);
/// `label` + root-qualified vpath, counted — the `STATS` shape, so outcomes
/// group together when the rows are sorted by key and two reports diff cleanly.
pub(super) static COPYUPS: BoundedTally<String> = BoundedTally::new(2000);

/// Current value of one copy-up counter. `pub` for the same reason
/// [`outcome_count`] is: a gate's own test can assert a class went to zero
/// without reaching into the atomics.
pub fn copy_up_count(outcome: CopyUp) -> u64 {
    COPYUP_COUNTS[outcome as usize].load(Ordering::Relaxed)
}

/// Record a copy-up's outcome. `bytes` is what was written (0 unless
/// [`CopyUp::Seeded`]). Cheap no-op when disabled, like every counter here.
///
/// Also lands in the ordered trace: a copy-up that failed matters most in
/// relation to what the game did next, and only the trace preserves that.
pub fn note_copy_up(outcome: CopyUp, root: u32, vpath: &str, bytes: u64) {
    if !enabled() {
        return;
    }
    COPYUP_COUNTS[outcome as usize].fetch_add(1, Ordering::Relaxed);
    // Only a completed copy-up contributes to the seeded total — a failed one
    // had whatever it wrote removed, so counting its bytes would report data
    // as delivered that no longer exists. The partial count is not lost: the
    // trace line below carries it, which is where a mid-session failure wants
    // to be read anyway (in sequence with what the game did next).
    if outcome == CopyUp::Seeded {
        COPYUP_BYTES.fetch_add(bytes, Ordering::Relaxed);
    }
    let path = format!("root{root}/{}", vpath.to_ascii_lowercase());
    note_trace("copy-up", &path, &format!("{} {bytes}B", outcome.label()));
    COPYUPS.add(format!("{:<26} {path}", outcome.label()));
}

/// A shim-local overlay filesystem mutation that did not happen.
///
/// The overlay's four mutating operations (`Overlay::ensure_parent`,
/// `clear_whiteout`, `whiteout`, `rename`) all used to discard their
/// `std::io::Result`. That is not the harmless "best-effort" it reads as, and
/// `ensure_parent` is the clearest case: `Engine::decide_open` calls it and
/// then answers `Decision::Redirect` with a target *inside* the directory it
/// just failed to create. The game's own open then fails at the NT boundary,
/// and nothing anywhere records why — not even the copy-up counters, since a
/// truncating or creating write never runs copy-up at all. The other three
/// fail just as quietly in the other direction: a whiteout that is not
/// written leaves a deleted file visible, and a whiteout that is not cleared
/// leaves a recreated file invisible.
///
/// **Only failures are counted *per path* here**, unlike [`CopyUp`], which
/// names the file for its successes too. Copy-ups are a handful per session
/// and "which file was seeded" is half the diagnosis; these run on every
/// single overlay-bound write, delete and rename, so a per-path success tally
/// would be volume with no reader.
///
/// Successes still get a **bare count** ([`OverlayFail::Succeeded`]), and that
/// is not a hedge — without it an absent section is ambiguous between "no
/// overlay operation happened at all" and "they all happened and were fine",
/// which are very different readings of a live run. That is the same
/// ambiguity [`CopyUp`] deliberately fixed one enum over by counting
/// `Seeded`, and this enum went a whole gate without it. With the count, an
/// absent section means the first and a `succeeded` row means the second; any
/// *other* line is still a finding.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum OverlayFail {
    /// `create_dir_all` for the overlay file's parent failed. The redirect
    /// that follows points into a directory that does not exist.
    EnsureParent = 0,
    /// A whiteout marker could not be removed: the path stays hidden even
    /// though it was just recreated.
    ClearWhiteout = 1,
    /// A whiteout marker could not be written: the deleted path stays
    /// visible, backed by the snapshot/provider content beneath it.
    Whiteout = 2,
    /// The overlay-internal move failed: the rename's destination does not
    /// hold the source's content.
    Rename = 3,
    /// Declined: shim-initiated I/O was already in flight on this thread, so
    /// the mutation would have been re-decided by our own hooks rather than
    /// reaching the real filesystem (see `hook::ShimIoGuard`).
    DeclinedReentrant = 4,
    /// The mutation happened. Counted only — no path, no trace entry — so an
    /// absent section can be read as "no overlay mutations at all" rather
    /// than being ambiguous with "all of them worked". Last discriminant so
    /// the four failure classes keep their positions.
    Succeeded = 5,
}

pub(super) const OVERLAY_FAIL_N: usize = 6;

/// Every variant, for iteration in `render_overlay_fails` and the label test.
pub const ALL_OVERLAY_FAILS: [OverlayFail; OVERLAY_FAIL_N] = [
    OverlayFail::EnsureParent,
    OverlayFail::ClearWhiteout,
    OverlayFail::Whiteout,
    OverlayFail::Rename,
    OverlayFail::DeclinedReentrant,
    OverlayFail::Succeeded,
];

impl OverlayFail {
    /// Rendered label. Distinct across variants — see
    /// `every_overlay_failure_renders_with_a_distinct_label`.
    pub fn label(&self) -> &'static str {
        match self {
            OverlayFail::EnsureParent => "FAILED: overlay mkdir",
            OverlayFail::ClearWhiteout => "FAILED: clear whiteout",
            OverlayFail::Whiteout => "FAILED: write whiteout",
            OverlayFail::Rename => "FAILED: overlay rename",
            OverlayFail::DeclinedReentrant => "declined: reentrant",
            OverlayFail::Succeeded => "succeeded",
        }
    }
}

pub(super) static OVERLAY_FAIL_COUNTS: [AtomicU64; OVERLAY_FAIL_N] =
    [const { AtomicU64::new(0) }; OVERLAY_FAIL_N];
/// `label` + root-qualified vpath, counted — the `STATS`/`COPYUPS` shape.
pub(super) static OVERLAY_FAILS: BoundedTally<String> = BoundedTally::new(2000);

/// Current value of one overlay-failure counter. `pub` for the same reason
/// [`copy_up_count`] is: a test can assert a class stayed at zero, or moved,
/// without reaching into the atomics.
pub fn overlay_fail_count(fail: OverlayFail) -> u64 {
    OVERLAY_FAIL_COUNTS[fail as usize].load(Ordering::Relaxed)
}

/// Record an overlay mutation that failed or was declined. Cheap no-op when
/// disabled, like every counter here.
/// Record an overlay mutation that worked. Count only — see
/// [`OverlayFail::Succeeded`] for why there is no path and no trace entry.
pub fn note_overlay_ok() {
    if !enabled() {
        return;
    }
    OVERLAY_FAIL_COUNTS[OverlayFail::Succeeded as usize].fetch_add(1, Ordering::Relaxed);
}

pub fn note_overlay_fail(fail: OverlayFail, root: u32, vpath: &str) {
    if !enabled() {
        return;
    }
    debug_assert!(
        fail != OverlayFail::Succeeded,
        "use note_overlay_ok; Succeeded carries no path and no trace entry"
    );
    OVERLAY_FAIL_COUNTS[fail as usize].fetch_add(1, Ordering::Relaxed);
    let path = format!("root{root}/{}", vpath.to_ascii_lowercase());
    // Also in the ordered trace: what the game did *next* after the overlay
    // refused to move is the other half of explaining the open that failed.
    note_trace("overlay", &path, fail.label());
    OVERLAY_FAILS.add(format!("{:<26} {path}", fail.label()));
}

/// The header still counts **failures only** — that is the number a reader is
/// looking for — but the section renders whenever any overlay mutation
/// happened at all, so a run with nothing but successes prints a `succeeded`
/// row instead of nothing. See [`OverlayFail::Succeeded`] for why the
/// difference matters.
pub(super) fn render_overlay_fails(snap: &Snapshot) -> String {
    let succeeded = snap.overlay_fail_counts[OverlayFail::Succeeded as usize];
    let total: u64 = snap.overlay_fail_counts.iter().sum::<u64>() - succeeded;
    if total == 0 && succeeded == 0 {
        return String::new();
    }
    let mut s = format!("\nshim-local overlay failures ({total}):\n");
    for fail in ALL_OVERLAY_FAILS {
        let c = snap.overlay_fail_counts[fail as usize];
        if c != 0 {
            s.push_str(&format!("  {:<32} {c:>8}\n", fail.label()));
        }
    }
    let mut rows: Vec<(&String, &u64)> = snap.overlay_fails.iter().collect();
    rows.sort_by(|a, b| a.0.cmp(b.0));
    for (k, c) in rows {
        s.push_str(&format!("    {c:>6}x  {k}\n"));
    }
    s
}

pub(super) fn render_copy_ups(snap: &Snapshot) -> String {
    let total: u64 = snap.copy_up_counts.iter().sum();
    if total == 0 {
        return String::new();
    }
    let mut s = format!(
        "\ncopy-on-write copy-ups ({total}, {:.1} MiB seeded):\n",
        snap.copy_up_bytes as f64 / (1024.0 * 1024.0)
    );
    for outcome in ALL_COPY_UPS {
        let c = snap.copy_up_counts[outcome as usize];
        if c != 0 {
            s.push_str(&format!("  {:<32} {c:>8}\n", outcome.label()));
        }
    }
    // Every copy-up by path, not just the failures: "which file did this
    // succeed for" is the other half of explaining an empty file, and the
    // volume is a handful per session, not thousands.
    let mut rows: Vec<(&String, &u64)> = snap.copy_ups.iter().collect();
    rows.sort_by(|a, b| a.0.cmp(b.0));
    for (k, c) in rows {
        s.push_str(&format!("    {c:>6}x  {k}\n"));
    }
    s
}

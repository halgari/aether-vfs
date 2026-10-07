//! The snapshot every report is built from, its renderer, and the reporter thread.

use super::*;

/// One consistent instant across every counter this module tracks.
///
/// Each `render_*` function used to read its own globals live, at the moment
/// it happened to run inside `start_reporter`'s one `format!` call. Rendering
/// does real work between those reads — formatting rows, cloning and sorting
/// HashMaps — so hook calls landing during that work were picked up by later
/// sections but not earlier ones, and the report could contradict itself:
/// observed in practice, a run whose "routed" row read 11 while its trace
/// section — rendered earlier in the same file — said "10" operations.
/// Taking every raw value up front, before any formatting starts, shrinks the
/// window in which that can happen from "however long rendering takes" down
/// to "however long these loads take": a handful of atomic reads and small
/// clones, done back to back. The counters themselves stay lock-free and
/// keep moving; what changes is that a single report is built from one set
/// of readings instead of several taken at different times.
pub(super) struct Snapshot {
    pub(super) calls: [u64; N],
    pub(super) nanos: [u64; N],
    pub(super) rooted: [u64; N],
    pub(super) max_nanos: [u64; N],
    pub(super) slow: [u64; N],
    pub(super) async_opens: u64,
    pub(super) sync_opens: u64,
    pub(super) apc_reads: u64,
    pub(super) event_reads: u64,
    pub(super) bare_reads: u64,
    pub(super) iocp_binds: u64,
    pub(super) fills_started: u64,
    pub(super) fills_completed: u64,
    pub(super) fills_failed: u64,
    pub(super) fill_bytes: u64,
    pub(super) fill_nanos: u64,
    pub(super) fill_max_nanos: u64,
    pub(super) setinfo_noop: HashMap<u32, u64>,
    pub(super) synth_locks: HashMap<String, u64>,
    pub(super) passthrough: HashMap<String, u64>,
    pub(super) undecodable: HashMap<String, u64>,
    pub(super) trace: Vec<String>,
    pub(super) stats: HashMap<String, u64>,
    pub(super) readdirs: Vec<String>,
    pub(super) readdir_calls: u64,
    pub(super) readdirs_dropped: u64,
    pub(super) outcome_counts: [u64; OUTCOME_N],
    pub(super) outcome_paths: [HashMap<String, u64>; OUTCOME_N],
    pub(super) unrouted_director_opens: u64,
    pub(super) name_queries: u64,
    pub(super) name_queries_cached: u64,
    pub(super) name_lookups: u64,
    pub(super) reg_read_fallbacks: u64,
    pub(super) reg_unresolved: u64,
    pub(super) reg_close_lock_given_up: u64,
    pub(super) reg: RegCounters,
    pub(super) copy_up_counts: [u64; COPYUP_N],
    pub(super) copy_up_bytes: u64,
    pub(super) copy_ups: HashMap<String, u64>,
    pub(super) overlay_fail_counts: [u64; OVERLAY_FAIL_N],
    pub(super) overlay_fails: HashMap<String, u64>,
    pub(super) hook_panics_total: u64,
    pub(super) hook_panics: HashMap<&'static str, u64>,
    /// `None` when `VFS_SHIM_READ_CACHE` turned the cache off.
    pub(super) read_cache: Option<vfs_ipc::CacheStats>,
    /// The read cache's busiest files (empty when it is off).
    pub(super) read_cache_files: Vec<vfs_ipc::FileReport>,
}

/// Clone the contents of one of this module's `Mutex<Option<T>>` logs,
/// treating "poisoned" and "never initialised" alike as empty. The ordered logs
/// (`TRACE`, `READDIRS`) are read through this; every counted table is a
/// `BoundedTally` and reads through its `snapshot`.
pub(super) fn accumulated<T: Clone + Default>(m: &Mutex<Option<T>>) -> T {
    m.lock()
        .ok()
        .and_then(|g| g.as_ref().cloned())
        .unwrap_or_default()
}

pub(super) fn snapshot() -> Snapshot {
    let mut calls = [0u64; N];
    let mut nanos = [0u64; N];
    let mut rooted = [0u64; N];
    let mut max_nanos = [0u64; N];
    let mut slow = [0u64; N];
    for i in 0..N {
        calls[i] = CALLS[i].load(Ordering::Relaxed);
        nanos[i] = NANOS[i].load(Ordering::Relaxed);
        rooted[i] = ROOTED[i].load(Ordering::Relaxed);
        max_nanos[i] = MAX_NANOS[i].load(Ordering::Relaxed);
        slow[i] = SLOW[i].load(Ordering::Relaxed);
    }
    let mut outcome_counts = [0u64; OUTCOME_N];
    let mut outcome_paths: [HashMap<String, u64>; OUTCOME_N] =
        std::array::from_fn(|_| HashMap::new());
    for (i, outcome) in ALL_OUTCOMES.into_iter().enumerate() {
        outcome_counts[i] = outcome_count(outcome);
        outcome_paths[i] = OUTCOME_PATHS[i].snapshot();
    }
    Snapshot {
        calls,
        nanos,
        rooted,
        max_nanos,
        slow,
        async_opens: ASYNC_OPENS.load(Ordering::Relaxed),
        sync_opens: SYNC_OPENS.load(Ordering::Relaxed),
        apc_reads: APC_READS.load(Ordering::Relaxed),
        event_reads: EVENT_READS.load(Ordering::Relaxed),
        bare_reads: BARE_READS.load(Ordering::Relaxed),
        iocp_binds: IOCP_BINDS.load(Ordering::Relaxed),
        fills_started: FILLS_STARTED.load(Ordering::Relaxed),
        fills_completed: FILLS_COMPLETED.load(Ordering::Relaxed),
        fills_failed: FILLS_FAILED.load(Ordering::Relaxed),
        fill_bytes: FILL_BYTES.load(Ordering::Relaxed),
        fill_nanos: FILL_NANOS.load(Ordering::Relaxed),
        fill_max_nanos: FILL_MAX_NANOS.load(Ordering::Relaxed),
        setinfo_noop: SETINFO_NOOP.snapshot(),
        synth_locks: SYNTH_LOCKS.snapshot(),
        passthrough: PATHS.snapshot(),
        undecodable: UNDECODABLE.snapshot(),
        trace: accumulated(&TRACE),
        stats: STATS.snapshot(),
        readdirs: accumulated(&READDIRS),
        readdir_calls: READDIR_CALLS.load(Ordering::Relaxed),
        readdirs_dropped: READDIRS_DROPPED.load(Ordering::Relaxed),
        outcome_counts,
        outcome_paths,
        unrouted_director_opens: UNROUTED_DIRECTOR_OPENS.load(Ordering::Relaxed),
        name_queries: NAME_QUERIES.load(Ordering::Relaxed),
        name_queries_cached: NAME_QUERIES_CACHED.load(Ordering::Relaxed),
        name_lookups: NAME_LOOKUPS.load(Ordering::Relaxed),
        reg_read_fallbacks: reg_read_fallback_count(),
        reg_unresolved: reg_unresolved_count(),
        reg_close_lock_given_up: reg_close_lock_given_up_count(),
        reg: reg_counters(),
        copy_up_counts: std::array::from_fn(|i| copy_up_count(ALL_COPY_UPS[i])),
        copy_up_bytes: COPYUP_BYTES.load(Ordering::Relaxed),
        copy_ups: COPYUPS.snapshot(),
        overlay_fail_counts: std::array::from_fn(|i| overlay_fail_count(ALL_OVERLAY_FAILS[i])),
        overlay_fails: OVERLAY_FAILS.snapshot(),
        hook_panics_total: hook_panics_total(),
        hook_panics: HOOK_PANICS.snapshot(),
        read_cache: crate::read_cache::stats(),
        read_cache_files: crate::read_cache::top_files(READ_CACHE_FILES_SHOWN),
    }
}

/// Counters as a human-readable table, as of `snap`.
///
/// `i` indexes five parallel fixed-size arrays plus `NAMES` at once; zipping
/// all of them would be less readable than the plain index it replaces.
#[allow(clippy::needless_range_loop)]
pub(super) fn render(snap: &Snapshot) -> String {
    let mut total_calls = 0u64;
    let mut total_nanos = 0u64;
    let mut total_rooted = 0u64;
    let mut rows = String::new();
    for i in 0..N {
        let c = snap.calls[i];
        if c == 0 {
            continue;
        }
        let ns = snap.nanos[i];
        let r = snap.rooted[i];
        total_calls += c;
        total_nanos += ns;
        total_rooted += r;
        let slow = snap.slow[i];
        let max = snap.max_nanos[i];
        // Share of total time owned by the calls that stalled: if this is most
        // of it, the mean is a wake-latency artefact, not per-call work.
        let stall_share = if ns == 0 {
            0.0
        } else {
            100.0 * (slow as f64 * SLOW_NS as f64) / ns as f64
        };
        rows.push_str(&format!(
            "  {:<28} {:>7} calls {:>8.3}s {:>8.1} us/call  max {:>8.1}ms  >1ms {:>5} ({:>4.1}% min-share)\n",
            NAMES[i],
            c,
            ns as f64 / 1e9,
            (ns as f64 / c as f64) / 1000.0,
            max as f64 / 1e6,
            slow,
            stall_share
        ));
    }
    format!(
        "vfs-shim hook stats (pid {})\n{}  {:<28} {:>9} calls  {:>8.3}s  {:>7.1} us/call  rooted {:>7} ({:>4.1}%)\n",
        std::process::id(),
        rows,
        "TOTAL",
        total_calls,
        total_nanos as f64 / 1e9,
        if total_calls == 0 {
            0.0
        } else {
            (total_nanos as f64 / total_calls as f64) / 1000.0
        },
        total_rooted,
        if total_calls == 0 {
            0.0
        } else {
            100.0 * total_rooted as f64 / total_calls as f64
        }
    )
}

/// How often the reporter thread rewrites the report.
///
/// The 250ms default assumes a session lasting well past that — true for
/// every real launch, but not for a millisecond-scale e2e fixture, which can
/// exit before the first tick ever fires.
/// `VFS_SHIM_STATS_INTERVAL_MS` (see `vfs_env::SHIM_STATS_INTERVAL_MS`)
/// overrides the interval for exactly that case — a short-lived test child
/// can opt into a fast tick for just itself; unset, every existing caller
/// keeps the same 250ms cadence.
pub(super) fn report_interval() -> std::time::Duration {
    std::time::Duration::from_millis(vfs_env::parsed_or(vfs_env::SHIM_STATS_INTERVAL_MS, 250))
}

/// When the process started this module, so the banner can say how much of the
/// run a periodic snapshot actually covers.
pub(super) static START: OnceLock<Instant> = OnceLock::new();

/// The line every report opens with, naming *what this report is*.
///
/// **Every report is a snapshot. There is no exit report**, and a reader has
/// to know that: an absent row means "this had not happened by t+N", which is
/// not the same claim as "this never happened". The 2026-08-14 prefs
/// investigation lost time to exactly that ambiguity — a missing `NtReadFile`
/// row that the director's own counters contradicted.
///
/// An exit flush was the obvious fix and was **built, measured, and removed**.
/// From `DLL_PROCESS_DETACH` — the only place a DLL can act on process exit —
/// every other thread is already terminated, and one killed mid-`std::fs::write`
/// leaves a lock (the CRT heap's, among others) that the flush then waits on
/// forever, inside the loader lock. Measured 2026-08-15 on the `vfs-directord`
/// e2e suite: with the flush, the suite wedged on 2 of 2 runs, each leaving an
/// unreapable fixture process holding the shim DLL's image lock; without it,
/// the same suite finished in 3 seconds. Reading counters with `try_lock` does
/// not save it, because rendering has to allocate.
///
/// So the banner is the answer instead: a short-lived process that needs its
/// tail in the report must outlive one tick (see `report_interval` and
/// `vfs-fixture-prefs`/`vfs-fixture-escape`'s end-of-run waits), and this line
/// says how much of the run the numbers below actually cover.
pub(super) fn banner() -> String {
    let elapsed = START
        .get()
        .map(|s| s.elapsed().as_secs_f64())
        .unwrap_or(0.0);
    format!(
        "SNAPSHOT at t+{elapsed:.3}s — process still running, no exit report exists. An absent \
         row means \"not by t+{elapsed:.3}s\", which is weaker than \"never\".\n"
    )
}

/// Render one complete report from a single snapshot.
///
/// One snapshot feeds every section, so a report can never show two sections
/// disagreeing about counters that only look independent — see `Snapshot`.
pub(super) fn render_report() -> String {
    let snap = snapshot();
    format!(
        "{}{}{}{}{}{}{}{}{}{}{}{}{}{}{}{}{}{}",
        banner(),
        render_hook_panics(&snap),
        render(&snap),
        render_read_cache(&snap),
        render_async(&snap),
        render_fills(&snap),
        render_name_queries(&snap),
        render_reg_fallbacks(&snap),
        render_stats(&snap),
        render_trace(&snap),
        render_undecodable(&snap),
        render_readdirs(&snap),
        render_passthrough(&snap),
        render_setinfo_noop(&snap),
        render_synth_locks(&snap),
        render_outcomes(&snap),
        render_copy_ups(&snap),
        render_overlay_fails(&snap)
    )
}

/// Write `body` to the report path via a temp + rename, so a reader never sees
/// a half file.
pub(super) fn write_report(path: &std::ffi::OsStr, body: &str) {
    let tmp = std::path::PathBuf::from(path).with_extension("tmp");
    if std::fs::write(&tmp, body.as_bytes()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Start a thread that rewrites the report periodically.
///
/// This is the only writer of the report file: there is no exit dump, and
/// [`banner`] explains at length why not. A process that ends before the first
/// tick therefore leaves no report at all.
pub(crate) fn start_reporter() {
    if !enabled() || REPORTER.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = START.set(Instant::now());
    let Some(path) = vfs_env::raw(vfs_env::SHIM_STATS_LOG) else {
        return;
    };
    let interval = report_interval();
    let _ = std::thread::Builder::new()
        .name("vfs-shim-stats".into())
        .spawn(move || {
            loop {
                std::thread::sleep(interval);
                write_report(&path, &render_report());
            }
        });
}

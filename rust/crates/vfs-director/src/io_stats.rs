//! Process-wide VFS I/O telemetry for post-launch diagnosis.
//!
//! Tracks getattr / readdir / open / read / close / errors so hosts can see
//! whether the game is actually pulling BSAs/ESMs once it is running.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();

fn start() -> Instant {
    *START.get_or_init(Instant::now)
}

#[derive(Default)]
struct PathStats {
    opens: u64,
    open_size: u64,
    reads: u64,
    bytes: u64,
    writes: u64,
    write_bytes: u64,
    getattrs: u64,
    readdirs: u64,
    not_found: u64,
    errors: u64,
}

#[derive(Default)]
struct State {
    by_path: HashMap<String, PathStats>,
    fh_path: HashMap<u64, String>,
    ops_getattr: u64,
    ops_readdir: u64,
    ops_open: u64,
    ops_read: u64,
    ops_write: u64,
    ops_close: u64,
    ops_err: u64,
    total_bytes: u64,
    total_write_bytes: u64,
    /// `record_open`'s ok/err split, kept apart from `ops_open` (every open,
    /// regardless of outcome) and `ops_err` (every error, regardless of
    /// operation) so `open_totals()` can report opens specifically. This is
    /// the director-side half of the shim/director open-count reconciliation:
    /// the shim's `Routed` outcome counter and this pair are meant to agree.
    opens_ok: u64,
    opens_err: u64,
    /// Writes refused because the resolved mount had no `ReadWrite`
    /// provider, keyed by path with a running count.
    rejected_writes: HashMap<String, u64>,
}


static STATE: OnceLock<Mutex<State>> = OnceLock::new();

/// Epoch for "since launch" reporting (call once after CreateProcess resumes).
static LAUNCH_MARK_MS: AtomicU64 = AtomicU64::new(0);

fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| Mutex::new(State::default()))
}

/// Aggregate counters, for benchmarking rather than the human report.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Totals {
    pub getattrs: u64,
    pub readdirs: u64,
    pub opens: u64,
    pub reads: u64,
    pub closes: u64,
    pub errors: u64,
    pub bytes: u64,
    /// Distinct paths touched (opened, statted or listed).
    pub paths: u64,
    /// Write RPCs — see `ops_write` (:39). Appended after `paths` rather than
    /// alongside `reads`/`bytes` so existing field order is untouched.
    pub write_ops: u64,
    pub write_bytes: u64,
}

impl Totals {
    /// Mean bytes per read. The headline number for read amplification: a game
    /// that streams 4 KiB records turns every one into a ring round-trip.
    pub fn bytes_per_read(&self) -> f64 {
        if self.reads == 0 {
            0.0
        } else {
            self.bytes as f64 / self.reads as f64
        }
    }

    /// Read RPCs per MiB delivered — scale-free, so runs of different length
    /// stay comparable.
    pub fn reads_per_mib(&self) -> f64 {
        let mib = self.bytes as f64 / (1024.0 * 1024.0);
        if mib <= 0.0 {
            0.0
        } else {
            self.reads as f64 / mib
        }
    }
}

/// Snapshot of the aggregate counters.
pub fn totals() -> Totals {
    let Ok(s) = state().lock() else {
        return Totals::default();
    };
    Totals {
        getattrs: s.ops_getattr,
        readdirs: s.ops_readdir,
        opens: s.ops_open,
        reads: s.ops_read,
        closes: s.ops_close,
        errors: s.ops_err,
        bytes: s.total_bytes,
        paths: s.by_path.len() as u64,
        write_ops: s.ops_write,
        write_bytes: s.total_write_bytes,
    }
}

pub fn mark_launch() {
    let ms = start().elapsed().as_millis() as u64;
    LAUNCH_MARK_MS.store(ms, Ordering::Relaxed);
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    state().lock().ok().map(|mut g| f(&mut g))
}

fn norm_path(p: &str) -> String {
    let p = p.trim().trim_start_matches('/').trim_start_matches('\\');
    if p.is_empty() || p == "." {
        return "/".into();
    }
    p.replace('\\', "/")
}

pub fn record_getattr(path: &str, found: bool, err: bool) {
    let path = norm_path(path);
    let _ = with_state(|s| {
        s.ops_getattr += 1;
        let e = s.by_path.entry(path).or_default();
        e.getattrs += 1;
        if err {
            s.ops_err += 1;
            e.errors += 1;
        } else if !found {
            e.not_found += 1;
        }
    });
}

pub fn record_readdir(path: &str, ok: bool) {
    let path = norm_path(path);
    let _ = with_state(|s| {
        s.ops_readdir += 1;
        let e = s.by_path.entry(path).or_default();
        e.readdirs += 1;
        if !ok {
            s.ops_err += 1;
            e.errors += 1;
        }
    });
}

pub fn record_open(path: &str, fh: Option<u64>, size: u64, err: bool) {
    let path = norm_path(path);
    let _ = with_state(|s| {
        s.ops_open += 1;
        if err {
            s.ops_err += 1;
            s.opens_err += 1;
            s.by_path.entry(path).or_default().errors += 1;
            return;
        }
        s.opens_ok += 1;
        let e = s.by_path.entry(path.clone()).or_default();
        e.opens += 1;
        e.open_size = size;
        if let Some(fh) = fh {
            s.fh_path.insert(fh, path);
        }
    });
}

/// `(ok, err)` counts of every open `record_open` has seen — the director's
/// half of the shim/director open-count reconciliation. The shim classifies
/// each under-root open by which path it took (`Routed` vs. a `FellThrough*`
/// variant); this is the corresponding count of opens that actually arrived
/// at the director. Gate 4 compares the two: an open the shim believed was
/// `Routed` that never shows up here is a bypass, by definition.
pub fn open_totals() -> (u64, u64) {
    let Ok(s) = state().lock() else {
        return (0, 0);
    };
    (s.opens_ok, s.opens_err)
}

pub fn record_read(fh: u64, n: usize, err: bool) {
    let _ = with_state(|s| {
        s.ops_read += 1;
        if err {
            s.ops_err += 1;
            return;
        }
        s.total_bytes += n as u64;
        if let Some(path) = s.fh_path.get(&fh).cloned() {
            let e = s.by_path.entry(path).or_default();
            e.reads += 1;
            e.bytes += n as u64;
        }
    });
}

pub fn record_write(fh: u64, n: usize, err: bool) {
    let _ = with_state(|s| {
        s.ops_write += 1;
        if err {
            s.ops_err += 1;
            return;
        }
        s.total_write_bytes += n as u64;
        if let Some(path) = s.fh_path.get(&fh).cloned() {
            let e = s.by_path.entry(path).or_default();
            e.writes += 1;
            e.write_bytes += n as u64;
        }
    });
}

/// Record that `open(..., OPEN_WRITE)` was refused because the resolved
/// mount's provider has no `ReadWrite` access. Keyed by path so a caller can
/// tell "no provider here is writable" apart from a one-off mistake.
pub fn record_rejected_write(path: &str) {
    let path = norm_path(path);
    let _ = with_state(|s| {
        *s.rejected_writes.entry(path).or_insert(0) += 1;
    });
}

/// Snapshot of `(path, count)` for every rejected write seen so far.
pub fn rejected_writes() -> Vec<(String, u64)> {
    let Ok(s) = state().lock() else {
        return Vec::new();
    };
    s.rejected_writes
        .iter()
        .map(|(path, count)| (path.clone(), *count))
        .collect()
}

/// Clear rejected-write tracking (tests; also useful before a fresh probe).
pub fn reset_rejected_writes() {
    if let Ok(mut s) = state().lock() {
        s.rejected_writes.clear();
    }
}

pub fn record_close(fh: u64) {
    let _ = with_state(|s| {
        s.ops_close += 1;
        s.fh_path.remove(&fh);
    });
}

/// Human-readable snapshot for logs. `top_n` paths by bytes then opens.
pub fn snapshot_report(top_n: usize) -> String {
    let launch_ms = LAUNCH_MARK_MS.load(Ordering::Relaxed);
    let now_ms = start().elapsed().as_millis() as u64;
    let since_launch = if launch_ms > 0 {
        now_ms.saturating_sub(launch_ms)
    } else {
        now_ms
    };

    let Some(s) = state().lock().ok() else {
        return "io_stats: lock poisoned".into();
    };

    let mut paths: Vec<_> = s.by_path.iter().collect();
    paths.sort_by(|a, b| {
        b.1.bytes
            .cmp(&a.1.bytes)
            .then(b.1.opens.cmp(&a.1.opens))
            .then(b.1.getattrs.cmp(&a.1.getattrs))
    });

    let mut out = String::new();
    out.push_str(&format!(
        "vfs-io t+{:.1}s ops: getattr={} readdir={} open={} read={} close={} err={} bytes={:.2} MiB paths={} write={} write_bytes={:.2} MiB\n",
        since_launch as f64 / 1000.0,
        s.ops_getattr,
        s.ops_readdir,
        s.ops_open,
        s.ops_read,
        s.ops_close,
        s.ops_err,
        s.total_bytes as f64 / (1024.0 * 1024.0),
        s.by_path.len(),
        s.ops_write,
        s.total_write_bytes as f64 / (1024.0 * 1024.0),
    ));

    for (path, st) in paths.into_iter().take(top_n) {
        if st.opens == 0
            && st.bytes == 0
            && st.getattrs == 0
            && st.readdirs == 0
            && st.not_found == 0
            && st.errors == 0
            && st.writes == 0
        {
            continue;
        }
        out.push_str(&format!(
            "  {path}: open={} size={} read_ops={} bytes={:.2} MiB getattr={} readdir={} nf={} err={} write_ops={} write_bytes={:.2} MiB\n",
            st.opens,
            st.open_size,
            st.reads,
            st.bytes as f64 / (1024.0 * 1024.0),
            st.getattrs,
            st.readdirs,
            st.not_found,
            st.errors,
            st.writes,
            st.write_bytes as f64 / (1024.0 * 1024.0),
        ));
    }

    // Always surface failures (often the real boot blockers).
    let mut errs: Vec<_> = s
        .by_path
        .iter()
        .filter(|(_, st)| st.errors > 0 || st.not_found > 0)
        .collect();
    errs.sort_by(|a, b| {
        (b.1.errors + b.1.not_found).cmp(&(a.1.errors + a.1.not_found))
    });
    if !errs.is_empty() {
        out.push_str(&format!(
            "  --- top failures (err/nf), showing {} of {} ---\n",
            top_n.min(errs.len()),
            errs.len()
        ));
        for (path, st) in errs.into_iter().take(top_n) {
            out.push_str(&format!(
                "  FAIL {path}: open_ok={} err={} nf={} getattr={}\n",
                st.opens, st.errors, st.not_found, st.getattrs
            ));
        }
    }
    out
}

/// Reset counters (e.g. after preflight probes so post-launch is clean).
pub fn reset() {
    if let Ok(mut s) = state().lock() {
        *s = State::default();
    }
    LAUNCH_MARK_MS.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The counters behind `state()` are process-global with no test
    /// dimension, and `reset()` zeroes all of them. Any test that resets is
    /// therefore mutually exclusive with any test that reads a *delta*
    /// across two snapshots: a reset landing between the two reads makes the
    /// delta negative or zero. Tests here take this lock rather than assuming
    /// test order — the convention stated at `VA_LOCK` in
    /// `vfs-shim::lazy_section`.
    ///
    /// Concurrent *increments* from other modules' tests (`ring_dispatch`
    /// drives real opens through these same counters) are harmless to a
    /// delta and deliberately not serialized; only `reset()` is destructive.
    static STATS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn records_open_read_and_surfaces_failures() {
        let _stats = STATS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        record_open("Data/Skyrim.esm", Some(1), 249_753_412, false);
        record_read(1, 1_048_576, false);
        record_read(1, 2_097_152, false);
        record_open("missing.dat", None, 0, true);
        record_getattr("ghost.bin", false, false);
        mark_launch();
        let report = snapshot_report(20);
        assert!(
            report.contains("Data/Skyrim.esm") || report.contains("data/skyrim.esm"),
            "master path missing from report: {report}"
        );
        assert!(
            report.contains("MiB") && report.contains("bytes="),
            "byte totals missing: {report}"
        );
        assert!(
            report.contains("FAIL") || report.contains("err="),
            "failures not surfaced: {report}"
        );
        // 1 + 2 MiB read
        assert!(
            report.contains("3.00 MiB") || report.contains("2.99 MiB") || report.contains("3.0"),
            "expected ~3 MiB total: {report}"
        );
    }

    /// Director-side half of the shim/director open-count reconciliation
    /// (aether-vfs measurement gate): `open_totals()` must distinguish a
    /// successful open from a failed one, not just count "opens happened".
    ///
    /// Uses deltas rather than a fresh `reset()` baseline: this crate's test
    /// binary runs its `#[test]` fns concurrently, and several of them
    /// (`ring_dispatch`'s dispatch tests in particular) drive real opens
    /// through the same process-wide counters. A hard reset here would race
    /// those threads; a delta tolerates their increments.
    ///
    /// A delta is not sufficient on its own, though, and claiming so is what
    /// made this test flaky (~1.7% of runs, always `0 -> 0`): a sibling
    /// test's `reset()` landing between the two `open_totals()` reads zeroes
    /// the increment this asserts on. Hence `STATS_LOCK`.
    #[test]
    fn open_totals_counts_ok_and_err_separately() {
        let _stats = STATS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (before_ok, before_err) = open_totals();
        record_open("open-totals-probe-ok.dat", Some(u64::MAX - 7), 7, false);
        record_open("open-totals-probe-err.dat", None, 0, true);
        let (after_ok, after_err) = open_totals();
        assert!(
            after_ok > before_ok,
            "ok open count did not increment: {before_ok} -> {after_ok}"
        );
        assert!(
            after_err > before_err,
            "err open count did not increment: {before_err} -> {after_err}"
        );
    }

    /// Gate 4 prerequisite: `record_write` already updates `ops_write` /
    /// `total_write_bytes` and `PathStats::writes`/`write_bytes`, but neither
    /// `snapshot_report` nor `totals()` printed them — leaving no way to tell
    /// "writes routed" from "no writes happened" (the ambiguity that made
    /// stage 2b's `FellThroughWriteFallback=0` uninformative). Uses a
    /// `totals()` delta for the same concurrency reason as
    /// `open_totals_counts_ok_and_err_separately`, and takes the same lock
    /// for the same reason — a concurrent `reset()` would not merely skew the
    /// delta here, it would drop the per-path line the tail of this test
    /// requires. The per-path assertion keys on a path unique to this test so
    /// concurrently-running tests can't touch its counts.
    #[test]
    fn write_ops_and_bytes_are_surfaced() {
        let _stats = STATS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let before = totals();
        record_open("write-ops-probe.dat", Some(u64::MAX - 42), 0, false);
        record_write(u64::MAX - 42, 1_048_576, false);
        record_write(u64::MAX - 42, 1_048_576, false);
        let after = totals();
        assert!(
            after.write_ops >= before.write_ops + 2,
            "write op count did not increment: {} -> {}",
            before.write_ops,
            after.write_ops
        );
        assert!(
            after.write_bytes >= before.write_bytes + 2 * 1_048_576,
            "write byte count did not increment: {} -> {}",
            before.write_bytes,
            after.write_bytes
        );

        let report = snapshot_report(usize::MAX);
        let line = report
            .lines()
            .find(|l| l.contains("write-ops-probe.dat"))
            .unwrap_or_else(|| panic!("no per-path line for write-ops-probe.dat: {report}"));
        assert!(
            line.contains("write_ops=2 write_bytes=2.00 MiB"),
            "write counters missing from per-path line: {line}"
        );

        let totals_line = report
            .lines()
            .next()
            .unwrap_or_else(|| panic!("report has no lines: {report}"));
        assert!(
            totals_line.contains("write=") && totals_line.contains("write_bytes="),
            "aggregate line missing write fields: {totals_line}"
        );
    }
}

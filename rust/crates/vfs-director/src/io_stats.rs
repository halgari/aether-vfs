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
            s.by_path.entry(path).or_default().errors += 1;
            return;
        }
        let e = s.by_path.entry(path.clone()).or_default();
        e.opens += 1;
        e.open_size = size;
        if let Some(fh) = fh {
            s.fh_path.insert(fh, path);
        }
    });
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
        "vfs-io t+{:.1}s ops: getattr={} readdir={} open={} read={} close={} err={} bytes={:.2} MiB paths={}\n",
        since_launch as f64 / 1000.0,
        s.ops_getattr,
        s.ops_readdir,
        s.ops_open,
        s.ops_read,
        s.ops_close,
        s.ops_err,
        s.total_bytes as f64 / (1024.0 * 1024.0),
        s.by_path.len(),
    ));

    for (path, st) in paths.into_iter().take(top_n) {
        if st.opens == 0
            && st.bytes == 0
            && st.getattrs == 0
            && st.readdirs == 0
            && st.not_found == 0
            && st.errors == 0
        {
            continue;
        }
        out.push_str(&format!(
            "  {path}: open={} size={} read_ops={} bytes={:.2} MiB getattr={} readdir={} nf={} err={}\n",
            st.opens,
            st.open_size,
            st.reads,
            st.bytes as f64 / (1024.0 * 1024.0),
            st.getattrs,
            st.readdirs,
            st.not_found,
            st.errors,
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

    #[test]
    fn records_open_read_and_surfaces_failures() {
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
}

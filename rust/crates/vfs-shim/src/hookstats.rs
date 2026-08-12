//! Per-hook call counts and time, for attributing shim overhead.
//!
//! `io_stats` in the director counts only what reaches it over the ring. The
//! shim detours *every* file operation the process makes, and the ones that
//! pass straight through to disk never become a request — so the director is
//! blind to them. Measured 2026-08-12: a launch reaching a window served ~800
//! director ops but took ~9.3 s longer than the same game with no VFS, which
//! those 800 ops cannot explain. This counts the population the director
//! cannot see.
//!
//! Off unless `VFS_SHIM_STATS_LOG` names a file: reading a clock on every
//! intercepted call is itself measurable, so it must not be on by default.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

/// Hooks worth attributing separately. Anything not listed lands in `Other`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Hook {
    Create = 0,
    Open = 1,
    QAttr = 2,
    QFull = 3,
    Read = 4,
    Write = 5,
    Close = 6,
    QDirEx = 7,
    QueryInfo = 8,
    CreateSection = 9,
    MapView = 10,
    Other = 11,
}

const N: usize = 12;

const NAMES: [&str; N] = [
    "NtCreateFile",
    "NtOpenFile",
    "NtQueryAttributesFile",
    "NtQueryFullAttributesFile",
    "NtReadFile",
    "NtWriteFile",
    "NtClose",
    "NtQueryDirectoryFileEx",
    "NtQueryInformationFile",
    "NtCreateSection",
    "NtMapViewOfSection",
    "other",
];

static CALLS: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static NANOS: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
/// Calls that resolved to VFS content (rather than passing through to disk).
/// Only meaningful for hooks that call `mark_rooted` — currently create/open.
static ROOTED: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
/// Slowest single call, and how many exceeded [`SLOW_NS`]. A mean cannot tell
/// "every call is slow" from "a few calls stall on a cold wake", and those want
/// opposite fixes — the first is per-call work, the second is wake latency.
static MAX_NANOS: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static SLOW: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
/// Time above which a call is counted as stalled rather than served: well past
/// any ring RPC (20–209 µs measured) and into scheduler-quantum territory.
const SLOW_NS: u64 = 1_000_000;
static REPORTER: AtomicBool = AtomicBool::new(false);

/// Whether instrumentation is on, resolved once.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VFS_SHIM_STATS_LOG").is_some())
}

/// Times one hook invocation and records it on drop.
///
/// Constructing this when disabled reads no clock and touches no atomics, so an
/// un-instrumented run pays only a cached bool check.
pub struct Timed {
    hook: Hook,
    start: Option<Instant>,
    rooted: bool,
}

impl Timed {
    pub fn new(hook: Hook) -> Self {
        Timed {
            hook,
            start: if enabled() { Some(Instant::now()) } else { None },
            rooted: false,
        }
    }

    /// Mark this call as having been served from the VFS.
    pub fn mark_rooted(&mut self) {
        self.rooted = true;
    }
}

impl Drop for Timed {
    fn drop(&mut self) {
        let Some(start) = self.start else { return };
        let i = self.hook as usize;
        let ns = start.elapsed().as_nanos() as u64;
        CALLS[i].fetch_add(1, Ordering::Relaxed);
        NANOS[i].fetch_add(ns, Ordering::Relaxed);
        if ns > SLOW_NS {
            SLOW[i].fetch_add(1, Ordering::Relaxed);
        }
        MAX_NANOS[i].fetch_max(ns, Ordering::Relaxed);
        if self.rooted {
            ROOTED[i].fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Current counters as a human-readable table.
pub fn render() -> String {
    let mut total_calls = 0u64;
    let mut total_nanos = 0u64;
    let mut total_rooted = 0u64;
    let mut rows = String::new();
    for i in 0..N {
        let c = CALLS[i].load(Ordering::Relaxed);
        if c == 0 {
            continue;
        }
        let ns = NANOS[i].load(Ordering::Relaxed);
        let r = ROOTED[i].load(Ordering::Relaxed);
        total_calls += c;
        total_nanos += ns;
        total_rooted += r;
        let slow = SLOW[i].load(Ordering::Relaxed);
        let max = MAX_NANOS[i].load(Ordering::Relaxed);
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

/// Start a thread that rewrites the report periodically.
///
/// A snapshot rather than an exit dump: a game that is killed, or one still
/// running at the benchmark's window mark, would never produce an exit report.
pub fn start_reporter() {
    if !enabled() || REPORTER.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(path) = std::env::var_os("VFS_SHIM_STATS_LOG") else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("vfs-shim-stats".into())
        .spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(250));
            let body = render();
            // Write via a temp + rename so a reader never sees a half file.
            let tmp = std::path::PathBuf::from(&path).with_extension("tmp");
            if std::fs::write(&tmp, body.as_bytes()).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_timer_records_nothing() {
        // VFS_SHIM_STATS_LOG is unset under test, so `enabled()` is false and
        // the guard must not read a clock or touch counters.
        let before = CALLS[Hook::Create as usize].load(Ordering::Relaxed);
        {
            let mut t = Timed::new(Hook::Create);
            t.mark_rooted();
        }
        assert_eq!(CALLS[Hook::Create as usize].load(Ordering::Relaxed), before);
    }

    #[test]
    fn render_reports_nothing_when_no_calls() {
        let s = render();
        assert!(s.contains("hook stats"), "{s}");
        assert!(s.contains("TOTAL"), "{s}");
        // Never divide by zero on an idle process.
        assert!(!s.contains("NaN"), "{s}");
    }

    #[test]
    fn hook_names_cover_every_variant() {
        assert_eq!(NAMES.len(), N);
        assert_eq!(Hook::Other as usize, N - 1);
        assert!(NAMES.iter().all(|n| !n.is_empty()));
    }
}

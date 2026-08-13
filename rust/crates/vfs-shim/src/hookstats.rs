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

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
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
    QDir = 11,
    QByName = 12,
}

const N: usize = 13;

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
    "NtQueryDirectoryFile",
    "NtQueryInformationByName",
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
    *ON.get_or_init(|| vfs_env::present(vfs_env::SHIM_STATS_LOG))
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

/// Asynchronous-I/O usage against our synthetic handles.
///
/// We complete every read synchronously and signal `event` when one is given.
/// We do **not** run the caller's APC, and we cannot post to an I/O completion
/// port — a synthetic handle is not a kernel file object, so no packet is ever
/// queued. A caller that waits on either never wakes, which looks exactly like
/// the observed hang: handles already open (no new opens), reads barely moving,
/// every thread idle, and nothing of ours on any stack.
///
/// `FileCompletionInformation` is how a handle gets bound to a port, so seeing
/// it on a synthetic handle is the smoking gun.
static ASYNC_OPENS: AtomicU64 = AtomicU64::new(0);
static SYNC_OPENS: AtomicU64 = AtomicU64::new(0);
static APC_READS: AtomicU64 = AtomicU64::new(0);
static EVENT_READS: AtomicU64 = AtomicU64::new(0);
static BARE_READS: AtomicU64 = AtomicU64::new(0);
static IOCP_BINDS: AtomicU64 = AtomicU64::new(0);

/// A synthetic handle was opened; `synchronous` reflects the CreateOptions.
pub fn note_open_sync(synchronous: bool) {
    if !enabled() {
        return;
    }
    if synchronous {
        SYNC_OPENS.fetch_add(1, Ordering::Relaxed);
    } else {
        ASYNC_OPENS.fetch_add(1, Ordering::Relaxed);
    }
}

/// A read on a synthetic handle, classified by how the caller expects
/// completion: APC routine, event, or neither (fully synchronous).
pub fn note_read_completion(has_apc: bool, has_event: bool) {
    if !enabled() {
        return;
    }
    if has_apc {
        APC_READS.fetch_add(1, Ordering::Relaxed);
    } else if has_event {
        EVENT_READS.fetch_add(1, Ordering::Relaxed);
    } else {
        BARE_READS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Demand-paged section fills — the one I/O path no other counter can see.
///
/// A read from a mapped view is a page fault, not `NtReadFile`, and
/// `lazy_section` calls `read_fragmented` directly rather than going through the
/// read hook. So section traffic appears in neither the hook table nor the
/// director's request log from the game's point of view.
///
/// `started` vs `completed` is the important pair: a persistent gap means a fill
/// is in flight and never returned, i.e. the faulting thread is wedged. That is
/// indistinguishable from "the game went idle" at every other vantage point.
static FILLS_STARTED: AtomicU64 = AtomicU64::new(0);
static FILLS_COMPLETED: AtomicU64 = AtomicU64::new(0);
static FILLS_FAILED: AtomicU64 = AtomicU64::new(0);
static FILL_BYTES: AtomicU64 = AtomicU64::new(0);
static FILL_NANOS: AtomicU64 = AtomicU64::new(0);
static FILL_MAX_NANOS: AtomicU64 = AtomicU64::new(0);

pub fn note_fill_start() {
    if enabled() {
        FILLS_STARTED.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn note_fill_end(bytes: usize, nanos: u64, ok: bool) {
    if !enabled() {
        return;
    }
    FILLS_COMPLETED.fetch_add(1, Ordering::Relaxed);
    if !ok {
        FILLS_FAILED.fetch_add(1, Ordering::Relaxed);
    }
    FILL_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    FILL_NANOS.fetch_add(nanos, Ordering::Relaxed);
    FILL_MAX_NANOS.fetch_max(nanos, Ordering::Relaxed);
}

fn render_fills() -> String {
    let started = FILLS_STARTED.load(Ordering::Relaxed);
    let done = FILLS_COMPLETED.load(Ordering::Relaxed);
    if started == 0 {
        return String::new();
    }
    let ns = FILL_NANOS.load(Ordering::Relaxed);
    let inflight = started.saturating_sub(done);
    format!(
        "\ndemand-paged section fills:\n  \
         started {started} / completed {done} / failed {} / IN FLIGHT {inflight}\n  \
         {:.1} MiB, {:.3}s total, max {:.1}ms{}\n",
        FILLS_FAILED.load(Ordering::Relaxed),
        FILL_BYTES.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0),
        ns as f64 / 1e9,
        FILL_MAX_NANOS.load(Ordering::Relaxed) as f64 / 1e6,
        if inflight > 0 {
            "   <-- a fill never returned; the faulting thread is wedged"
        } else {
            ""
        }
    )
}

/// A synthetic handle was bound to an I/O completion port.
pub fn note_iocp_bind() {
    if !enabled() {
        return;
    }
    IOCP_BINDS.fetch_add(1, Ordering::Relaxed);
}

fn render_async() -> String {
    let (a, s) = (
        ASYNC_OPENS.load(Ordering::Relaxed),
        SYNC_OPENS.load(Ordering::Relaxed),
    );
    let (apc, ev, bare) = (
        APC_READS.load(Ordering::Relaxed),
        EVENT_READS.load(Ordering::Relaxed),
        BARE_READS.load(Ordering::Relaxed),
    );
    let iocp = IOCP_BINDS.load(Ordering::Relaxed);
    if a + s + apc + ev + bare + iocp == 0 {
        return String::new();
    }
    format!(
        "\nasync I/O on synthetic handles:\n  \
         opens: {a} async / {s} synchronous\n  \
         reads: {apc} with APC / {ev} with event / {bare} bare\n  \
         IOCP binds (FileCompletionInformation): {iocp}\n  \
         NOTE: APC reads and IOCP binds are completions we never deliver.\n"
    )
}

/// How often each path was opened, capped so a storm cannot grow this without
/// bound.
///
/// A hook count alone cannot say what a stalled game is hunting for: a world
/// load issued 25k `NtCreateFile` with only 210 resolving under the root, and
/// the useful question is which paths the other 24.9k were. A *distinct* list
/// could not answer that either — a process wedged in a retry loop reopens one
/// path thousands of times, and dedup hides exactly the path that matters. So
/// this counts repeats and reports the busiest first.
static PATHS: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);
const PATHS_MAX: usize = 4000;
/// How many of the busiest paths to print. Generous: the question this answers
/// is usually "did the process ever touch X", and a path asked for once is
/// exactly the interesting case when X is a file that should have loaded.
const PATHS_SHOWN: usize = 1000;

/// Record a path an open was attempted on. Cheap no-op when disabled.
pub fn note_passthrough(path: &str) {
    if !enabled() {
        return;
    }
    let Ok(mut g) = PATHS.lock() else { return };
    let map = g.get_or_insert_with(HashMap::new);
    let lower = path.to_ascii_lowercase();
    if let Some(c) = map.get_mut(&lower) {
        *c += 1;
        return;
    }
    // Past the cap we stop learning new paths but keep counting known ones,
    // so a loop that started early still shows its true rate.
    if map.len() < PATHS_MAX {
        map.insert(lower, 1);
    }
}

/// Busiest-first rendering, split out so the ordering is testable without
/// touching the process-wide map (which is inert unless instrumentation is on).
fn format_paths(mut pairs: Vec<(String, u64)>) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    let distinct = pairs.len();
    let total: u64 = pairs.iter().map(|(_, c)| *c).sum();
    // Count descending, then path so equal counts do not reorder between
    // snapshots — a diff of two reports is how a loop's rate gets measured.
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let shown = pairs.len().min(PATHS_SHOWN);
    let mut s = format!(
        "\nopen paths by frequency (top {shown} of {distinct} distinct, {total} opens):\n"
    );
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
static UNDECODABLE: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

pub fn note_undecodable(name: Option<&str>) {
    if !enabled() {
        return;
    }
    let Ok(mut g) = UNDECODABLE.lock() else { return };
    let map = g.get_or_insert_with(HashMap::new);
    let key = name.unwrap_or("<no name>").to_ascii_lowercase();
    *map.entry(key).or_insert(0) += 1;
}

fn render_undecodable() -> String {
    let Ok(g) = UNDECODABLE.lock() else { return String::new() };
    let Some(map) = g.as_ref() else { return String::new() };
    if map.is_empty() {
        return String::new();
    }
    let mut rows: Vec<(&String, &u64)> = map.iter().collect();
    rows.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let total: u64 = rows.iter().map(|(_, c)| **c).sum();
    let mut s = format!("
undecodable opens ({} distinct, {total} calls):
", rows.len());
    for (k, c) in rows.iter().take(60) {
        s.push_str(&format!("  {c:>6}x  {k}
"));
    }
    s
}

/// Ordered log of operations against the managed root.
///
/// Counts say *what* was touched; only order says *where a sequence stopped*.
/// A load that gives up part way looks identical in a frequency table to one
/// that never started, because both simply lack the entries that would have
/// followed.
static TRACE: Mutex<Option<Vec<String>>> = Mutex::new(None);
const TRACE_MAX: usize = 4000;

pub fn note_trace(op: &str, path: &str, result: &str) {
    if !enabled() {
        return;
    }
    let Ok(mut g) = TRACE.lock() else { return };
    let v = g.get_or_insert_with(Vec::new);
    if v.len() >= TRACE_MAX {
        return;
    }
    v.push(format!("{:<10} {:<12} {}", op, result, path.to_ascii_lowercase()));
}

fn render_trace() -> String {
    let Ok(g) = TRACE.lock() else {
        return String::new();
    };
    let Some(v) = g.as_ref() else {
        return String::new();
    };
    if v.is_empty() {
        return String::new();
    }
    let mut s = format!("
ordered trace of under-root operations ({}):
", v.len());
    for (i, line) in v.iter().enumerate() {
        s.push_str(&format!("  {i:>5}  {line}
"));
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
static STATS: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);
const STATS_MAX: usize = 4000;

pub fn note_stat(path: &str, outcome: &str) {
    if !enabled() {
        return;
    }
    let Ok(mut g) = STATS.lock() else { return };
    let map = g.get_or_insert_with(HashMap::new);
    note_trace("stat", path, outcome);
    let key = format!("{:<12} {}", outcome, path.to_ascii_lowercase());
    if let Some(c) = map.get_mut(&key) {
        *c += 1;
        return;
    }
    if map.len() < STATS_MAX {
        map.insert(key, 1);
    }
}

fn render_stats() -> String {
    let Ok(g) = STATS.lock() else {
        return String::new();
    };
    let Some(map) = g.as_ref() else {
        return String::new();
    };
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

/// Every directory enumeration, with what it was asked for and what it got.
///
/// A game that finds no plugins looks identical to a game with no plugins, and
/// only the enumeration can tell those apart: Skyrim builds its load order by
/// listing `Data`, so "listed `Data`, got 0 entries" and "never listed `Data`"
/// are different bugs with the same symptom. Volume is tiny (60 calls across a
/// whole launch), so every one is recorded rather than counted.
static READDIRS: Mutex<Option<Vec<String>>> = Mutex::new(None);
const READDIRS_MAX: usize = 300;

/// `served` distinguishes a listing we produced from one we handed to the OS.
pub fn note_readdir(dir: &str, wildcard: Option<&str>, count: usize, served: bool) {
    if !enabled() {
        return;
    }
    let Ok(mut g) = READDIRS.lock() else { return };
    let v = g.get_or_insert_with(Vec::new);
    if v.len() >= READDIRS_MAX {
        return;
    }
    v.push(format!(
        "{:<8} {:>4} entries  filter={:<16} {}",
        if served { "served" } else { "OS" },
        count,
        wildcard.unwrap_or("*"),
        dir.to_ascii_lowercase()
    ));
}

fn render_readdirs() -> String {
    let Ok(g) = READDIRS.lock() else {
        return String::new();
    };
    let Some(v) = g.as_ref() else {
        return String::new();
    };
    if v.is_empty() {
        return String::new();
    }
    let mut s = format!("\ndirectory enumerations ({}):\n", v.len());
    for line in v {
        s.push_str(&format!("  {line}\n"));
    }
    s
}

fn render_passthrough() -> String {
    let Ok(g) = PATHS.lock() else {
        return String::new();
    };
    let Some(map) = g.as_ref() else {
        return String::new();
    };
    format_paths(map.iter().map(|(p, c)| (p.clone(), *c)).collect())
}

/// Start a thread that rewrites the report periodically.
///
/// A snapshot rather than an exit dump: a game that is killed, or one still
/// running at the benchmark's window mark, would never produce an exit report.
pub fn start_reporter() {
    if !enabled() || REPORTER.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(path) = vfs_env::raw(vfs_env::SHIM_STATS_LOG) else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("vfs-shim-stats".into())
        .spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(250));
            let body = format!(
                "{}{}{}{}{}{}{}{}",
                render(),
                render_async(),
                render_fills(),
                render_stats(),
                render_trace(),
                render_undecodable(),
                render_readdirs(),
                render_passthrough()
            );
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
    fn busiest_path_is_reported_first() {
        // The point of counting repeats: a retry loop's path must outrank the
        // hundreds of one-shot opens a launch makes, whatever order they hashed
        // into the map.
        let s = format_paths(vec![
            ("\\??\\c:\\a.esm".into(), 3),
            ("\\??\\c:\\loop.bsa".into(), 9001),
            ("\\??\\c:\\b.esm".into(), 3),
        ]);
        let lines: Vec<&str> = s.lines().filter(|l| l.starts_with("  ")).collect();
        assert!(lines[0].contains("loop.bsa"), "{s}");
        assert!(lines[0].contains("9001"), "{s}");
        // Equal counts must tie-break by path, so two snapshots stay diffable.
        assert!(lines[1].contains("a.esm"), "{s}");
        assert!(lines[2].contains("b.esm"), "{s}");
        assert!(s.contains("3 distinct"), "{s}");
        assert!(s.contains("9007 opens"), "{s}");
    }

    #[test]
    fn no_paths_renders_nothing() {
        assert_eq!(format_paths(Vec::new()), "");
    }

    #[test]
    fn hook_names_cover_every_variant() {
        assert_eq!(NAMES.len(), N);
        // The last variant must index the last name, or a hook silently
        // reports under a neighbour's label.
        assert_eq!(Hook::QByName as usize, N - 1);
        assert!(NAMES.iter().all(|n| !n.is_empty()));
    }
}

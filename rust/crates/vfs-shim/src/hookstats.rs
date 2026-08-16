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
    SetInfo = 13,
    QVol = 14,
    Lock = 15,
    Unlock = 16,
    FlushBuffers = 17,
}

const N: usize = 18;

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
    "NtSetInformationFile",
    "NtQueryVolumeInformationFile",
    "NtLockFile",
    "NtUnlockFile",
    "NtFlushBuffersFile",
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
struct Snapshot {
    calls: [u64; N],
    nanos: [u64; N],
    rooted: [u64; N],
    max_nanos: [u64; N],
    slow: [u64; N],
    async_opens: u64,
    sync_opens: u64,
    apc_reads: u64,
    event_reads: u64,
    bare_reads: u64,
    iocp_binds: u64,
    fills_started: u64,
    fills_completed: u64,
    fills_failed: u64,
    fill_bytes: u64,
    fill_nanos: u64,
    fill_max_nanos: u64,
    setinfo_noop: HashMap<u32, u64>,
    synth_locks: HashMap<String, u64>,
    passthrough: HashMap<String, u64>,
    undecodable: HashMap<String, u64>,
    trace: Vec<String>,
    stats: HashMap<String, u64>,
    readdirs: Vec<String>,
    outcome_counts: [u64; OUTCOME_N],
    outcome_paths: [HashMap<String, u64>; OUTCOME_N],
    unrouted_director_opens: u64,
    copy_up_counts: [u64; COPYUP_N],
    copy_up_bytes: u64,
    copy_ups: HashMap<String, u64>,
    overlay_fail_counts: [u64; OVERLAY_FAIL_N],
    overlay_fails: HashMap<String, u64>,
}

/// Clone the contents of one of this module's `Mutex<Option<T>>` accumulators,
/// treating "poisoned" and "never initialised" alike as empty. Every field of
/// [`Snapshot`] that is not a plain atomic is read through this.
fn accumulated<T: Clone + Default>(m: &Mutex<Option<T>>) -> T {
    m.lock().ok().and_then(|g| g.as_ref().cloned()).unwrap_or_default()
}

fn snapshot() -> Snapshot {
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
    let mut outcome_paths: [HashMap<String, u64>; OUTCOME_N] = std::array::from_fn(|_| HashMap::new());
    for (i, outcome) in ALL_OUTCOMES.into_iter().enumerate() {
        outcome_counts[i] = outcome_count(outcome);
        outcome_paths[i] = accumulated(&OUTCOME_PATHS[i]);
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
        setinfo_noop: accumulated(&SETINFO_NOOP),
        synth_locks: accumulated(&SYNTH_LOCKS),
        passthrough: accumulated(&PATHS),
        undecodable: accumulated(&UNDECODABLE),
        trace: accumulated(&TRACE),
        stats: accumulated(&STATS),
        readdirs: accumulated(&READDIRS),
        outcome_counts,
        outcome_paths,
        unrouted_director_opens: UNROUTED_DIRECTOR_OPENS.load(Ordering::Relaxed),
        copy_up_counts: std::array::from_fn(|i| copy_up_count(ALL_COPY_UPS[i])),
        copy_up_bytes: COPYUP_BYTES.load(Ordering::Relaxed),
        copy_ups: accumulated(&COPYUPS),
        overlay_fail_counts: std::array::from_fn(|i| overlay_fail_count(ALL_OVERLAY_FAILS[i])),
        overlay_fails: accumulated(&OVERLAY_FAILS),
    }
}

/// Counters as a human-readable table, as of `snap`.
///
/// `i` indexes five parallel fixed-size arrays plus `NAMES` at once; zipping
/// all of them would be less readable than the plain index it replaces.
#[allow(clippy::needless_range_loop)]
fn render(snap: &Snapshot) -> String {
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

fn render_fills(snap: &Snapshot) -> String {
    let started = snap.fills_started;
    let done = snap.fills_completed;
    if started == 0 {
        return String::new();
    }
    let ns = snap.fill_nanos;
    let inflight = started.saturating_sub(done);
    format!(
        "\ndemand-paged section fills:\n  \
         started {started} / completed {done} / failed {} / IN FLIGHT {inflight}\n  \
         {:.1} MiB, {:.3}s total, max {:.1}ms{}\n",
        snap.fills_failed,
        snap.fill_bytes as f64 / (1024.0 * 1024.0),
        ns as f64 / 1e9,
        snap.fill_max_nanos as f64 / 1e6,
        if inflight > 0 {
            "   <-- a fill never returned; the faulting thread is wedged"
        } else {
            ""
        }
    )
}

/// `NtSetInformationFile` classes that took the soft no-op on a synthetic
/// handle — i.e. neither position, EOF/truncate, delete, rename, nor
/// completion-port bind, and (for delete/rename) any recognized-but-unrouted
/// case where the handle's path or vpath could not be resolved. The hook
/// reports `STATUS_SUCCESS` for all of these without doing anything, which is
/// deliberate for classes we genuinely don't need to act on — but "the set of
/// classes this applies to is empty" was exactly the assumption that let a
/// real delete/rename silently no-op before this counter existed. Counting by
/// class number, not asserting the set is empty, is what keeps that
/// assumption checkable.
static SETINFO_NOOP: Mutex<Option<HashMap<u32, u64>>> = Mutex::new(None);

pub fn note_setinfo_noop(class: u32) {
    if !enabled() {
        return;
    }
    let Ok(mut g) = SETINFO_NOOP.lock() else { return };
    let map = g.get_or_insert_with(HashMap::new);
    *map.entry(class).or_insert(0) += 1;
}

fn render_setinfo_noop(snap: &Snapshot) -> String {
    let map = &snap.setinfo_noop;
    if map.is_empty() {
        return String::new();
    }
    let mut rows: Vec<(&u32, &u64)> = map.iter().collect();
    rows.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let mut s = format!(
        "\nNtSetInformationFile classes taking the soft no-op on synthetic handles ({} distinct):\n",
        rows.len()
    );
    for (class, count) in rows {
        s.push_str(&format!("  {count:>6}x  class={class}\n"));
    }
    s
}

/// Byte-range locks (and flushes) granted on synthetic handles without any
/// lock actually being taken — see `hook::lock_hook` for why that is the
/// chosen answer rather than an oversight.
///
/// This counter is the visibility half of that choice. A no-op lock is safe
/// exactly while one process at a time touches a given file in a session; the
/// moment that stops being true, the resulting corruption has no other
/// symptom — both writers succeed, both believe they were serialised, and
/// nothing in any log says a lock was involved. Keyed by operation + path so
/// "who is locking what, and is anyone locking the same thing" is answerable
/// from a report rather than from a debugger.
static SYNTH_LOCKS: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);
const SYNTH_LOCKS_MAX: usize = 2000;

pub fn note_synthetic_lock(op: &str, path: Option<&str>) {
    if !enabled() {
        return;
    }
    let Ok(mut g) = SYNTH_LOCKS.lock() else { return };
    let map = g.get_or_insert_with(HashMap::new);
    let key = format!(
        "{:<16} {}",
        op,
        path.unwrap_or("<untracked handle>").to_ascii_lowercase()
    );
    if let Some(c) = map.get_mut(&key) {
        *c += 1;
        return;
    }
    if map.len() < SYNTH_LOCKS_MAX {
        map.insert(key, 1);
    }
}

fn render_synth_locks(snap: &Snapshot) -> String {
    let map = &snap.synth_locks;
    if map.is_empty() {
        return String::new();
    }
    let total: u64 = map.values().sum();
    let mut rows: Vec<(&String, &u64)> = map.iter().collect();
    rows.sort_by(|a, b| a.0.cmp(b.0));
    let mut s = format!(
        "\nsynthetic byte-range locks/flushes answered locally ({total}, {} distinct):\n  \
         NOTE: no lock is actually held — see hook::lock_hook. Safe only while one process\n  \
         at a time touches each of these paths.\n",
        rows.len()
    );
    for (k, c) in rows {
        s.push_str(&format!("  {c:>6}x  {k}\n"));
    }
    s
}

/// A synthetic handle was bound to an I/O completion port.
pub fn note_iocp_bind() {
    if !enabled() {
        return;
    }
    IOCP_BINDS.fetch_add(1, Ordering::Relaxed);
}

fn render_async(snap: &Snapshot) -> String {
    let (a, s) = (snap.async_opens, snap.sync_opens);
    let (apc, ev, bare) = (snap.apc_reads, snap.event_reads, snap.bare_reads);
    let iocp = snap.iocp_binds;
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

fn render_undecodable(snap: &Snapshot) -> String {
    let map = &snap.undecodable;
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

fn render_trace(snap: &Snapshot) -> String {
    let v = &snap.trace;
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

fn render_stats(snap: &Snapshot) -> String {
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

/// Every directory enumeration, with what it was asked for and what it got.
///
/// A game that finds no plugins looks identical to a game with no plugins, and
/// only the enumeration can tell those apart: Skyrim builds its load order by
/// listing `Data`, so "listed `Data`, got 0 entries" and "never listed `Data`"
/// are different bugs with the same symptom. Volume is tiny (60 calls across a
/// whole launch), so every one is recorded rather than counted.
static READDIRS: Mutex<Option<Vec<String>>> = Mutex::new(None);
const READDIRS_MAX: usize = 300;

/// Which mechanism produced a directory listing.
///
/// This used to be a `served: bool` — "a listing we produced" vs "one we
/// handed to the OS" — and that is not the distinction containment turns on.
/// Both of `serve_dir_query`'s under-root branches recorded `served: true`,
/// including the one that drained the *real* directory behind the mount and
/// merged the shim-local overlay onto it, so the one counter that could have
/// shown an under-root listing coming off real disk reported it identically
/// to a director-authored one. Gate 4 task 8b split the two and deleted the
/// draining branch; the three-way label is what makes a regression back to it
/// visible in the report rather than only in the bytes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ReadDirSource {
    /// The director's own `OP_READDIR` answered. Authoritative and unmerged:
    /// nothing from the real filesystem can appear in it.
    Director,
    /// Under a managed root, but the director could not be asked about this
    /// directory (no client installed, or its `vpath_under_root` does not
    /// recognise the path the engine's own root notion accepted). The listing
    /// is the shim-local write overlay's entries and nothing else — real disk
    /// is never drained under a managed root. A nonzero count here in a live
    /// session means the two under-root predicates have drifted apart again.
    ContainedNoDirector,
    /// Outside every managed root: the OS answered it verbatim, and the
    /// recorded count is `0` because the shim never sees the entries.
    Os,
}

impl ReadDirSource {
    /// The token this renders as in the report. Parsed by
    /// `vfs-directord`'s test `support::readdir_records`; keep them in step.
    pub fn label(self) -> &'static str {
        match self {
            ReadDirSource::Director => "director",
            ReadDirSource::ContainedNoDirector => "contained",
            ReadDirSource::Os => "OS",
        }
    }
}

pub fn note_readdir(dir: &str, wildcard: Option<&str>, count: usize, source: ReadDirSource) {
    if !enabled() {
        return;
    }
    let Ok(mut g) = READDIRS.lock() else { return };
    let v = g.get_or_insert_with(Vec::new);
    if v.len() >= READDIRS_MAX {
        return;
    }
    v.push(format!(
        "{:<9} {:>4} entries  filter={:<16} {}",
        source.label(),
        count,
        wildcard.unwrap_or("*"),
        dir.to_ascii_lowercase()
    ));
}

fn render_readdirs(snap: &Snapshot) -> String {
    let v = &snap.readdirs;
    if v.is_empty() {
        return String::new();
    }
    let mut s = format!("\ndirectory enumerations ({}):\n", v.len());
    for line in v {
        s.push_str(&format!("  {line}\n"));
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
/// `FellThroughWriteFallback` is gate 4's, and `FellThroughDrmException` is
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

const OUTCOME_N: usize = 7;

/// Every variant, for iteration in `render_outcomes` and for the test that
/// checks labels stay distinct.
pub const ALL_OUTCOMES: [OpenOutcome; OUTCOME_N] = [
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

static OUTCOME_COUNTS: [AtomicU64; OUTCOME_N] = [const { AtomicU64::new(0) }; OUTCOME_N];

/// Paths seen for each outcome, bounded the same way `PATHS` is: past the cap
/// we stop learning new paths but keep counting known ones, so an early-
/// starting loop still shows its true rate.
static OUTCOME_PATHS: [Mutex<Option<HashMap<String, u64>>>; OUTCOME_N] =
    [const { Mutex::new(None) }; OUTCOME_N];
const OUTCOME_PATHS_MAX: usize = 4000;
/// How many of the busiest paths to print per outcome. Smaller than
/// `PATHS_SHOWN`: this table prints one such list per outcome, so it must
/// stay skimmable rather than repeat the full passthrough dump seven times.
const OUTCOME_PATHS_SHOWN: usize = 20;

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
///  - **The directory downgrade** (`hook.rs`): a write-flavoured open of a
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
static UNROUTED_DIRECTOR_OPENS: AtomicU64 = AtomicU64::new(0);

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
pub const UNROUTED_OPEN_LABEL: &str = "director-open: unrouted";

/// Record an `OP_OPEN` the shim issued on its own behalf, or a re-issue of one
/// already counted as `Routed`. See [`UNROUTED_DIRECTOR_OPENS`].
pub fn note_unrouted_director_open() {
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
/// Wired from every under-root decision site in `hook.rs`'s `create_hook` /
/// `open_hook` / `try_fuse_create` — see those for the full site-by-site
/// argument that each open records exactly once.
pub fn note_open_outcome(outcome: OpenOutcome, path: &str) {
    if !enabled() {
        return;
    }
    let idx = outcome as usize;
    OUTCOME_COUNTS[idx].fetch_add(1, Ordering::Relaxed);
    let Ok(mut g) = OUTCOME_PATHS[idx].lock() else { return };
    let map = g.get_or_insert_with(HashMap::new);
    let lower = path.to_ascii_lowercase();
    if let Some(c) = map.get_mut(&lower) {
        *c += 1;
        return;
    }
    if map.len() < OUTCOME_PATHS_MAX {
        map.insert(lower, 1);
    }
}

/// Busiest-first rendering of one outcome's paths, capped at
/// `OUTCOME_PATHS_SHOWN` with the remainder called out explicitly — a
/// truncated list silently presented as complete would make every later gate
/// measure against a count that is quietly wrong.
fn format_outcome_paths(mut pairs: Vec<(String, u64)>) -> String {
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
fn render_outcome(outcome: OpenOutcome, snap: &Snapshot) -> String {
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
fn render_outcomes(snap: &Snapshot) -> String {
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

fn render_passthrough(snap: &Snapshot) -> String {
    format_paths(snap.passthrough.iter().map(|(p, c)| (p.clone(), *c)).collect())
}

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
/// Which outcome that open is counted as changed with gate 4's Task 5. This
/// comment used to say `FellThroughWriteFallback`, and that is no longer the
/// live answer: since Task 5 the only route that reaches copy-up in a normal
/// session is a DRM/identity filename exception (`hook.rs`'s `steam_appid.txt`
/// / launcher / `steam_api*` branch), which records
/// `FellThroughDrmException`. `FellThroughWriteFallback` is now only
/// reachable behind the `allow_disk_fallthrough` opt-out, off by default.
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

const COPYUP_N: usize = 7;

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

static COPYUP_COUNTS: [AtomicU64; COPYUP_N] = [const { AtomicU64::new(0) }; COPYUP_N];
static COPYUP_BYTES: AtomicU64 = AtomicU64::new(0);
/// `label` + root-qualified vpath, counted — the `STATS` shape, so outcomes
/// group together when the rows are sorted by key and two reports diff cleanly.
static COPYUPS: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);
const COPYUPS_MAX: usize = 2000;

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
    let Ok(mut g) = COPYUPS.lock() else { return };
    let map = g.get_or_insert_with(HashMap::new);
    let key = format!("{:<26} {path}", outcome.label());
    if let Some(c) = map.get_mut(&key) {
        *c += 1;
        return;
    }
    if map.len() < COPYUPS_MAX {
        map.insert(key, 1);
    }
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

const OVERLAY_FAIL_N: usize = 6;

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

static OVERLAY_FAIL_COUNTS: [AtomicU64; OVERLAY_FAIL_N] =
    [const { AtomicU64::new(0) }; OVERLAY_FAIL_N];
/// `label` + root-qualified vpath, counted — the `STATS`/`COPYUPS` shape.
static OVERLAY_FAILS: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);
const OVERLAY_FAILS_MAX: usize = 2000;

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
    let Ok(mut g) = OVERLAY_FAILS.lock() else { return };
    let map = g.get_or_insert_with(HashMap::new);
    let key = format!("{:<26} {path}", fail.label());
    if let Some(c) = map.get_mut(&key) {
        *c += 1;
        return;
    }
    if map.len() < OVERLAY_FAILS_MAX {
        map.insert(key, 1);
    }
}

/// The header still counts **failures only** — that is the number a reader is
/// looking for — but the section renders whenever any overlay mutation
/// happened at all, so a run with nothing but successes prints a `succeeded`
/// row instead of nothing. See [`OverlayFail::Succeeded`] for why the
/// difference matters.
fn render_overlay_fails(snap: &Snapshot) -> String {
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

fn render_copy_ups(snap: &Snapshot) -> String {
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

/// How often the reporter thread rewrites the report.
///
/// The 250ms default assumes a session lasting well past that — true for
/// every real launch, but not for a millisecond-scale e2e fixture, which can
/// exit before the first tick ever fires.
/// `VFS_SHIM_STATS_INTERVAL_MS` (see `vfs_env::SHIM_STATS_INTERVAL_MS`)
/// overrides the interval for exactly that case — a short-lived test child
/// can opt into a fast tick for just itself; unset, every existing caller
/// keeps the same 250ms cadence.
fn report_interval() -> std::time::Duration {
    std::time::Duration::from_millis(vfs_env::parsed_or(vfs_env::SHIM_STATS_INTERVAL_MS, 250))
}

/// When the process started this module, so the banner can say how much of the
/// run a periodic snapshot actually covers.
static START: OnceLock<Instant> = OnceLock::new();

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
fn banner() -> String {
    let elapsed = START.get().map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0);
    format!(
        "SNAPSHOT at t+{elapsed:.3}s — process still running, no exit report exists. An absent \
         row means \"not by t+{elapsed:.3}s\", which is weaker than \"never\".\n"
    )
}

/// Render one complete report from a single snapshot.
///
/// One snapshot feeds every section, so a report can never show two sections
/// disagreeing about counters that only look independent — see `Snapshot`.
fn render_report() -> String {
    let snap = snapshot();
    format!(
        "{}{}{}{}{}{}{}{}{}{}{}{}{}{}",
        banner(),
        render(&snap),
        render_async(&snap),
        render_fills(&snap),
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
fn write_report(path: &std::ffi::OsStr, body: &str) {
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
pub fn start_reporter() {
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
        .spawn(move || loop {
            std::thread::sleep(interval);
            write_report(&path, &render_report());
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
        let s = render(&snapshot());
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
        assert_eq!(Hook::FlushBuffers as usize, N - 1);
        // Spot-check the middle of the table too: appending variants without
        // appending names in the same order is the failure this guards, and
        // only the *last* index is caught by the check above.
        assert_eq!(NAMES[Hook::SetInfo as usize], "NtSetInformationFile");
        assert_eq!(NAMES[Hook::Lock as usize], "NtLockFile");
        assert!(NAMES.iter().all(|n| !n.is_empty()));
    }

    /// The synthetic-lock section must carry its warning, not just its counts.
    /// The counts alone would read as ordinary activity; the section exists to
    /// say that each of those grants is a lock nobody actually holds, and a
    /// reader who does not know that cannot act on the numbers.
    #[test]
    fn synthetic_lock_section_names_the_path_and_says_the_lock_is_not_real() {
        // The key is built exactly as `note_synthetic_lock` builds it, rather
        // than the counter being driven: it is a no-op under test
        // (`VFS_SHIM_STATS_LOG` is unset, the same convention
        // `outcome_counters_are_free_when_disabled` relies on), so going
        // through it would render an empty section and assert nothing.
        let mut snap = empty_snapshot();
        snap.synth_locks.insert(
            format!("{:<16} {}", "lock-exclusive", r"\??\c:\root\skyrimprefs.ini"),
            3,
        );
        let s = render_synth_locks(&snap);
        assert!(s.contains("skyrimprefs.ini"), "{s}");
        assert!(s.contains("3x"), "{s}");
        assert!(s.contains("lock-exclusive"), "{s}");
        assert!(s.contains("no lock is actually held"), "{s}");
    }

    #[test]
    fn empty_synthetic_lock_section_renders_nothing() {
        assert_eq!(render_synth_locks(&empty_snapshot()), "");
    }

    /// The banner must say both things a reader needs: that this is a
    /// point-in-time snapshot, and *which* point. Dropping either turns an
    /// absent row back into the ambiguity the banner exists to remove.
    #[test]
    fn banner_marks_the_report_as_a_snapshot_and_dates_it() {
        let b = banner();
        assert!(b.starts_with("SNAPSHOT at t+"), "{b}");
        assert!(b.contains("no exit report exists"), "{b}");
        assert!(b.ends_with('\n'), "{b:?}");
    }

    #[test]
    fn outcome_counters_are_free_when_disabled() {
        // VFS_SHIM_STATS_LOG is unset under test, so `enabled()` is false and
        // recording must not touch the counters at all.
        let before = outcome_count(OpenOutcome::Routed);
        note_open_outcome(OpenOutcome::Routed, "a.esp");
        assert_eq!(outcome_count(OpenOutcome::Routed), before);
    }

    #[test]
    fn every_outcome_renders_with_a_distinct_label() {
        // A gate that removes one bypass class must be able to see that class
        // alone; identical or missing labels would defeat that.
        let mut labels: Vec<&str> = ALL_OUTCOMES.iter().map(|o| o.label()).collect();
        let n = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), n, "outcome labels must be distinct: {labels:?}");
    }

    #[test]
    fn outcome_path_truncation_says_how_many_more() {
        // A truncated per-outcome path list silently presented as complete
        // would make a later gate measure against a count that is quietly
        // wrong, so the cut must say what it left out.
        let pairs: Vec<(String, u64)> = (0..OUTCOME_PATHS_SHOWN + 5)
            .map(|i| (format!("path{i}.esp"), 1))
            .collect();
        let s = format_outcome_paths(pairs);
        assert!(s.contains("... and 5 more"), "{s}");
    }

    #[test]
    fn no_outcome_paths_renders_nothing() {
        assert_eq!(format_outcome_paths(Vec::new()), "");
    }

    /// The unrouted-open row must render inside the outcomes section, in the
    /// same shape as an outcome row: `vfs-directord`'s `assert_reconciled`
    /// parses that one section and needs both halves of the reconciliation
    /// out of it. Its label must also not collide with any outcome's, or the
    /// count would parse as a fall-through class instead.
    #[test]
    fn unrouted_director_opens_render_as_a_row_in_the_outcomes_section() {
        let mut snap = empty_snapshot();
        snap.outcome_counts[OpenOutcome::Routed as usize] = 9;
        snap.unrouted_director_opens = 3;
        let s = render_outcomes(&snap);
        assert!(s.starts_with("\nunder-root open outcomes:\n"), "{s}");
        assert!(s.contains(&format!("  {UNROUTED_OPEN_LABEL:<32} {:>8}\n", 3)), "{s}");
        assert!(
            !ALL_OUTCOMES.iter().any(|o| o.label() == UNROUTED_OPEN_LABEL),
            "the unrouted-open label collides with an outcome label"
        );
    }

    /// Zero is omitted, exactly like an outcome at zero — so a run with
    /// neither drift source present renders the section it always did.
    #[test]
    fn no_unrouted_director_opens_renders_no_row() {
        let mut snap = empty_snapshot();
        snap.outcome_counts[OpenOutcome::Routed as usize] = 1;
        assert!(!render_outcomes(&snap).contains(UNROUTED_OPEN_LABEL));
    }

    #[test]
    fn every_copy_up_outcome_renders_with_a_distinct_label() {
        // The whole value of this counter is telling "the director does not
        // have it" apart from "the read failed part-way" — they call for
        // different responses. Shared or missing labels would collapse them
        // back into the single silent failure the counter exists to replace.
        let mut labels: Vec<&str> = ALL_COPY_UPS.iter().map(|o| o.label()).collect();
        let n = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), n, "copy-up labels must be distinct: {labels:?}");
        assert_eq!(ALL_COPY_UPS.len(), COPYUP_N);
        // `snapshot` indexes the counter array by position in `ALL_COPY_UPS`,
        // so a variant listed out of order would report under a neighbour's
        // label — silently, and only in a live report nobody diffs.
        for (i, o) in ALL_COPY_UPS.into_iter().enumerate() {
            assert_eq!(o as usize, i, "{o:?} is listed at position {i}");
        }
        // A failure must be visibly a failure at a glance in a live report;
        // the declines are ordinary and must not shout.
        for o in ALL_COPY_UPS {
            let loud = o.label().starts_with("FAILED");
            assert_eq!(
                loud,
                matches!(
                    o,
                    CopyUp::DirectorRefused | CopyUp::ReadFailed | CopyUp::DestWriteFailed
                ),
                "{:?} is labelled {:?}",
                o,
                o.label()
            );
        }
    }

    #[test]
    fn copy_up_counters_are_free_when_disabled() {
        // VFS_SHIM_STATS_LOG is unset under test, so `enabled()` is false and
        // recording must not touch the counters at all.
        let before = copy_up_count(CopyUp::ReadFailed);
        note_copy_up(CopyUp::ReadFailed, 0, "data/foo.esp", 0);
        assert_eq!(copy_up_count(CopyUp::ReadFailed), before);
    }

    #[test]
    fn copy_up_rendering_groups_by_outcome_and_names_the_file() {
        // Built from a synthetic snapshot rather than the process-wide
        // counters, which are inert under test — same approach as
        // `busiest_path_is_reported_first`.
        let mut counts = [0u64; COPYUP_N];
        counts[CopyUp::Seeded as usize] = 2;
        counts[CopyUp::ReadFailed as usize] = 1;
        let mut copy_ups = HashMap::new();
        copy_ups.insert(format!("{:<26} root0/data/a.esp", CopyUp::Seeded.label()), 2);
        copy_ups.insert(format!("{:<26} root1/saves/s.ess", CopyUp::ReadFailed.label()), 1);
        let snap = Snapshot {
            copy_up_counts: counts,
            copy_up_bytes: 3 * 1024 * 1024,
            copy_ups,
            ..empty_snapshot()
        };
        let s = render_copy_ups(&snap);
        assert!(s.contains("copy-on-write copy-ups (3, 3.0 MiB seeded)"), "{s}");
        assert!(s.contains("FAILED: read"), "{s}");
        // The file that failed has to be nameable from the report alone —
        // "something failed" is what the silent version already told you.
        assert!(s.contains("root1/saves/s.ess"), "{s}");
        // An outcome that did not happen must not appear at all.
        assert!(!s.contains("declined: reentrant"), "{s}");
    }

    #[test]
    fn no_copy_ups_renders_nothing() {
        assert_eq!(render_copy_ups(&empty_snapshot()), "");
    }

    #[test]
    fn every_overlay_failure_renders_with_a_distinct_label() {
        // Same argument as the copy-up labels: "the overlay directory could
        // not be created" and "the whiteout marker could not be written" are
        // different problems with different fixes, and a shared label folds
        // them back into the single silent failure this counter replaces.
        let mut labels: Vec<&str> = ALL_OVERLAY_FAILS.iter().map(|o| o.label()).collect();
        let n = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), n, "overlay-failure labels must be distinct: {labels:?}");
        assert_eq!(ALL_OVERLAY_FAILS.len(), OVERLAY_FAIL_N);
        // `snapshot` indexes the counter array by position in
        // `ALL_OVERLAY_FAILS`, so a variant listed out of order would report
        // under a neighbour's label.
        for (i, o) in ALL_OVERLAY_FAILS.into_iter().enumerate() {
            assert_eq!(o as usize, i, "{o:?} is listed at position {i}");
        }
    }

    /// The ambiguity `Succeeded` exists to remove: before it, an absent
    /// section meant either "no overlay mutation happened" or "they all
    /// worked", and a reader could not tell which.
    #[test]
    fn overlay_successes_render_so_an_absent_section_means_nothing_happened() {
        let mut counts = [0u64; OVERLAY_FAIL_N];
        counts[OverlayFail::Succeeded as usize] = 4;
        let snap = Snapshot { overlay_fail_counts: counts, ..empty_snapshot() };
        let s = render_overlay_fails(&snap);
        assert!(s.contains("succeeded"), "{s}");
        // The header counts failures, not operations — that is the number a
        // reader is scanning for, and four successes are not four failures.
        assert!(s.contains("shim-local overlay failures (0)"), "{s}");
        // …and with genuinely nothing recorded, still nothing at all.
        assert_eq!(render_overlay_fails(&empty_snapshot()), "");
    }

    #[test]
    fn overlay_failure_rendering_names_the_operation_and_the_path() {
        let mut counts = [0u64; OVERLAY_FAIL_N];
        counts[OverlayFail::EnsureParent as usize] = 2;
        let mut fails = HashMap::new();
        fails.insert(
            format!("{:<26} root0/data/x.ini", OverlayFail::EnsureParent.label()),
            2,
        );
        let snap = Snapshot {
            overlay_fail_counts: counts,
            overlay_fails: fails,
            ..empty_snapshot()
        };
        let s = render_overlay_fails(&snap);
        assert!(s.contains("shim-local overlay failures (2)"), "{s}");
        assert!(s.contains("FAILED: overlay mkdir"), "{s}");
        // Naming the file is the point: "an overlay op failed" is what the
        // discarded `io::Result` already told you, which is nothing.
        assert!(s.contains("root0/data/x.ini"), "{s}");
        // An outcome that did not happen must not appear at all.
        assert!(!s.contains("write whiteout"), "{s}");
    }

    #[test]
    fn no_overlay_failures_renders_nothing() {
        assert_eq!(render_overlay_fails(&empty_snapshot()), "");
    }

    /// A zeroed `Snapshot` for rendering tests, so one can be built without
    /// the process-wide counters (inert under test) and without every test
    /// listing all twenty-odd fields.
    fn empty_snapshot() -> Snapshot {
        Snapshot {
            calls: [0; N],
            nanos: [0; N],
            rooted: [0; N],
            max_nanos: [0; N],
            slow: [0; N],
            async_opens: 0,
            sync_opens: 0,
            apc_reads: 0,
            event_reads: 0,
            bare_reads: 0,
            iocp_binds: 0,
            fills_started: 0,
            fills_completed: 0,
            fills_failed: 0,
            fill_bytes: 0,
            fill_nanos: 0,
            fill_max_nanos: 0,
            unrouted_director_opens: 0,
            setinfo_noop: HashMap::new(),
            synth_locks: HashMap::new(),
            passthrough: HashMap::new(),
            undecodable: HashMap::new(),
            trace: Vec::new(),
            stats: HashMap::new(),
            readdirs: Vec::new(),
            outcome_counts: [0; OUTCOME_N],
            outcome_paths: std::array::from_fn(|_| HashMap::new()),
            copy_up_counts: [0; COPYUP_N],
            copy_up_bytes: 0,
            copy_ups: HashMap::new(),
            overlay_fail_counts: [0; OVERLAY_FAIL_N],
            overlay_fails: HashMap::new(),
        }
    }
}

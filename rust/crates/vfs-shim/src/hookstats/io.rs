//! I/O counters: async and sync opens, read completions, fills, the read cache, `NtSetInformationFile`
//! no-ops, synthetic locks and directory enumerations.

use super::*;

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
pub(super) static ASYNC_OPENS: AtomicU64 = AtomicU64::new(0);
pub(super) static SYNC_OPENS: AtomicU64 = AtomicU64::new(0);
pub(super) static APC_READS: AtomicU64 = AtomicU64::new(0);
pub(super) static EVENT_READS: AtomicU64 = AtomicU64::new(0);
pub(super) static BARE_READS: AtomicU64 = AtomicU64::new(0);
pub(super) static IOCP_BINDS: AtomicU64 = AtomicU64::new(0);

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
pub(super) static FILLS_STARTED: AtomicU64 = AtomicU64::new(0);
pub(super) static FILLS_COMPLETED: AtomicU64 = AtomicU64::new(0);
pub(super) static FILLS_FAILED: AtomicU64 = AtomicU64::new(0);
pub(super) static FILL_BYTES: AtomicU64 = AtomicU64::new(0);
pub(super) static FILL_NANOS: AtomicU64 = AtomicU64::new(0);
pub(super) static FILL_MAX_NANOS: AtomicU64 = AtomicU64::new(0);

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

/// The label of the read-cache section, for anything that parses the report.
pub const READ_CACHE_LABEL: &str = "read cache (small reads of immutable files)";

/// What the read cache (`crate::read_cache`) did: whether small reads were
/// answered from memory or still crossed the ring, and why files left it.
///
/// `hits` are reads that cost no round trip at all; `misses` fetched (or
/// waited for) one block each and every hit after it is the saving.
/// `declined` were small reads of cacheable files sent over the ring anyway —
/// a file that changed, went cold, or a cache with no room. A launch whose
/// `NtReadFile` row is large and whose hits are near zero is reading files the
/// director did not call immutable: see `invalidations` and the open reply.
pub(super) fn render_read_cache(snap: &Snapshot) -> String {
    let Some(c) = snap.read_cache else {
        return format!("\n{READ_CACHE_LABEL}: OFF (VFS_SHIM_READ_CACHE)\n");
    };
    if c.hits + c.misses + c.declined + c.invalidations == 0 {
        return String::new();
    }
    let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
    let mut s = format!(
        "\n{READ_CACHE_LABEL}:\n  \
         hits {} / misses {} / declined {}   ({:.1}% of cached reads were hits)\n  \
         fetches {} ({:.1} MiB fetched)   evictions {}   resident {:.1} of {:.0} MiB in {} files\n  \
         misses on units the cap evicted {}   invalidations {} ({} blocks dropped)   cold files {}\n  \
         failed fetches {}   fetches given up on (past their deadline) {}\n",
        c.hits,
        c.misses,
        c.declined,
        if c.hits + c.misses == 0 {
            0.0
        } else {
            100.0 * c.hits as f64 / (c.hits + c.misses) as f64
        },
        c.fetches,
        mib(c.bytes_fetched),
        c.evictions,
        mib(c.resident_bytes),
        mib(c.max_bytes),
        c.files,
        c.pressure_misses,
        c.invalidations,
        c.blocks_invalidated,
        c.cold,
        c.fetch_failures,
        c.fetches_abandoned,
    );
    s.push_str(&render_read_cache_files(&snap.read_cache_files));
    s
}

/// How many files the read cache's per-file table shows.
pub(super) const READ_CACHE_FILES_SHOWN: usize = 20;

/// The read cache's busiest files: small reads offered, how they were
/// served, what fetching cost, and whether (and why) the file went cold —
/// `guard` when its misses were too many for its hits, `failures` when its
/// fetches kept failing. `capacity` misses are units the process-wide cap
/// evicted, which a larger `VFS_SHIM_READ_CACHE_MIB` would have kept.
pub(super) fn render_read_cache_files(rows: &[vfs_ipc::FileReport]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let mut s = format!(
        "  busiest files (top {}):\n  {:>9} {:>9} {:>7} {:>8} {:>8} {:>9}  {:<22} file\n",
        rows.len(),
        "reads",
        "hits",
        "misses",
        "capacity",
        "declined",
        "fetched",
        "cold"
    );
    for r in rows {
        let d = &r.diag;
        let cold = match (d.cold_guard, d.cold_failures) {
            (0, 0) => "no".to_string(),
            (g, 0) => format!("guard x{g}"),
            (0, f) => format!("failures x{f}"),
            (g, f) => format!("guard x{g}, failures x{f}"),
        };
        let cold = format!(
            "{cold}{}{}",
            if r.cold_now { " (now)" } else { "" },
            if r.poisoned { " poisoned" } else { "" }
        );
        s.push_str(&format!(
            "  {:>9} {:>9} {:>7} {:>8} {:>8} {:>7.1}Mi  {:<22} {}:{}\n",
            d.reads,
            d.hits,
            d.misses,
            d.pressure_misses,
            d.declined,
            d.bytes_fetched as f64 / (1024.0 * 1024.0),
            cold,
            r.root,
            r.path
        ));
    }
    s
}

pub(super) fn render_fills(snap: &Snapshot) -> String {
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
pub(super) static SETINFO_NOOP: BoundedTally<u32> = BoundedTally::unbounded();

pub fn note_setinfo_noop(class: u32) {
    if !enabled() {
        return;
    }
    SETINFO_NOOP.add(class);
}

pub(super) fn render_setinfo_noop(snap: &Snapshot) -> String {
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
pub(super) static SYNTH_LOCKS: BoundedTally<String> = BoundedTally::new(2000);

pub fn note_synthetic_lock(op: &str, path: Option<&str>) {
    if !enabled() {
        return;
    }
    SYNTH_LOCKS.add(format!(
        "{:<16} {}",
        op,
        path.unwrap_or("<untracked handle>").to_ascii_lowercase()
    ));
}

pub(super) fn render_synth_locks(snap: &Snapshot) -> String {
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

pub(super) fn render_async(snap: &Snapshot) -> String {
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

/// Every directory enumeration, with what it was asked for and what it got.
///
/// A game that finds no plugins looks identical to a game with no plugins, and
/// only the enumeration can tell those apart: Skyrim builds its load order by
/// listing `Data`, so "listed `Data`, got 0 entries" and "never listed `Data`"
/// are different bugs with the same symptom. Volume is tiny (60 calls across a
/// whole launch), so every one is recorded rather than counted.
pub(super) static READDIRS: Mutex<Option<Vec<String>>> = Mutex::new(None);
pub(super) const READDIRS_MAX: usize = 300;
/// Every `note_readdir` call, including those whose row duplicates one the
/// table already holds. A caller pumps `NtQueryDirectoryFile` once per batch
/// (once per entry, for a single-entry buffer), so this counts calls while
/// `READDIRS` holds distinct rows — see `note_readdir`.
pub(super) static READDIR_CALLS: AtomicU64 = AtomicU64::new(0);
/// Distinct rows refused because the table stood at `READDIRS_MAX`. Rendered
/// whenever nonzero: a table that quietly stopped recording reads exactly like
/// a complete one, which is the whole failure mode `note_readdir` describes.
pub(super) static READDIRS_DROPPED: AtomicU64 = AtomicU64::new(0);

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
    READDIR_CALLS.fetch_add(1, Ordering::Relaxed);
    let row = format!(
        "{:<9} {:>4} entries  filter={:<16} {}",
        source.label(),
        count,
        wildcard.unwrap_or("*"),
        dir.to_ascii_lowercase()
    );
    let Ok(mut g) = READDIRS.lock() else { return };
    let v = g.get_or_insert_with(Vec::new);
    // **Distinct rows, not one per call.** A caller enumerates a directory
    // through repeated `NtQueryDirectoryFile` calls on the same handle, so a
    // single `read_dir` of an N-entry directory reaches here N-ish times. The
    // untracked-handle branch in `serve_dir_query` records `count: 0` for
    // every one of them, which makes those rows byte-identical — so one
    // listing of a fat directory used to push hundreds of duplicates, hit
    // `READDIRS_MAX`, and silently discard every *later* row.
    //
    // Not hypothetical: one enumeration of a developer `%TEMP%` holding ~12k
    // entries filled all 300 rows, so the director-served listing that
    // `vfs-directord`'s e2e test
    // `directory_enumeration_under_a_managed_root_hides_an_unserved_real_file`
    // asserts on was never recorded. The listing itself was correct; only the
    // evidence was gone, and the test failed on its own vacuity guard — on
    // that machine, while passing anywhere `%TEMP%` happened to be small.
    //
    // Bounding the table by distinct directories rather than by call volume
    // removes that coupling. `READDIR_CALLS` keeps what the duplicates were
    // worth: how many times the enumeration was pumped.
    if v.iter().any(|r| r == &row) {
        return;
    }
    if v.len() >= READDIRS_MAX {
        READDIRS_DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    v.push(row);
}

pub(super) fn render_readdirs(snap: &Snapshot) -> String {
    let v = &snap.readdirs;
    if v.is_empty() {
        return String::new();
    }
    // The header count is *distinct rows* — `note_readdir` collapses
    // duplicates — and `of N calls` is what those duplicates were worth.
    // `dropped` prints whenever it is nonzero, because the failure this
    // guards against was a table that had quietly stopped recording.
    // `vfs-directord`'s `support::readdir_records` finds this section by the
    // text up to `(` and then skips the header line, so extra fields are safe
    // to add here.
    let dropped = snap.readdirs_dropped;
    let mut s = format!(
        "\ndirectory enumerations ({} distinct of {} calls{}):\n",
        v.len(),
        snap.readdir_calls,
        if dropped > 0 {
            format!("; {dropped} dropped at cap {READDIRS_MAX} — rows are missing")
        } else {
            String::new()
        }
    );
    for line in v {
        s.push_str(&format!("  {line}\n"));
    }
    s
}

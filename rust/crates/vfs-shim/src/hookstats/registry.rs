//! Final-path name queries and the registry overlay's counters.

use super::*;

/// Final-path name queries on synthetic handles: `NtQueryObject` for an
/// object name and `NtQueryInformationFile` for either file-name class.
///
/// Each can cost a director round trip, so how many a launch makes is what
/// decides whether that matters. `cached` were answered from what the shim
/// already knew; `lookups` asked the director (one `OP_STORED_NAMES` each).
pub(super) static NAME_QUERIES: AtomicU64 = AtomicU64::new(0);
pub(super) static NAME_QUERIES_CACHED: AtomicU64 = AtomicU64::new(0);
pub(super) static NAME_LOOKUPS: AtomicU64 = AtomicU64::new(0);

/// A name query on a synthetic handle is being answered.
pub fn note_name_query() {
    if enabled() {
        NAME_QUERIES.fetch_add(1, Ordering::Relaxed);
    }
}

/// ... wholly from what the shim remembered.
pub fn note_name_query_cached() {
    if enabled() {
        NAME_QUERIES_CACHED.fetch_add(1, Ordering::Relaxed);
    }
}

/// ... by asking the director.
pub fn note_name_lookup() {
    if enabled() {
        NAME_LOOKUPS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Registry overlay reads the director did not answer (unreachable, no registry attached, a
/// reply too large for the ring, a refused request): each one left the caller to serve the
/// real key alone (spec section 6). Counted whether or not stats are on, so a test can see it.
pub(super) static REG_READ_FALLBACKS: AtomicU64 = AtomicU64::new(0);

/// A registry overlay read failed; the caller falls back to the real registry.
pub fn note_reg_read_fallback() {
    REG_READ_FALLBACKS.fetch_add(1, Ordering::Relaxed);
}

/// How many registry overlay reads fell back to the real registry so far.
pub fn reg_read_fallback_count() -> u64 {
    REG_READ_FALLBACKS.load(Ordering::Relaxed)
}

/// Registry opens whose root key handle the shim could not name (or whose name it could not
/// compose), so the call went to the real registry unexamined. Counted whether or not stats
/// are on, like [`REG_READ_FALLBACKS`].
pub(super) static REG_UNRESOLVED: AtomicU64 = AtomicU64::new(0);

/// A registry open could not be resolved to a path and was passed through.
pub fn note_reg_unresolved() {
    REG_UNRESOLVED.fetch_add(1, Ordering::Relaxed);
}

/// How many registry opens were passed through unresolved so far.
pub fn reg_unresolved_count() -> u64 {
    REG_UNRESOLVED.load(Ordering::Relaxed)
}

/// Times a registry handle-table removal on the close path gave up waiting for its lock
/// (`sync::lock_for_close`): each one left a record behind for a handle that was closed.
/// Counted whether or not stats are on.
pub(super) static REG_CLOSE_LOCK_GIVEN_UP: AtomicU64 = AtomicU64::new(0);

/// A registry handle-table removal gave up on its lock.
pub fn note_reg_close_lock_given_up() {
    REG_CLOSE_LOCK_GIVEN_UP.fetch_add(1, Ordering::Relaxed);
}

/// How many registry handle-table removals gave up on their lock so far.
pub fn reg_close_lock_given_up_count() -> u64 {
    REG_CLOSE_LOCK_GIVEN_UP.load(Ordering::Relaxed)
}

/// The registry detour whose absence turned the registry overlay off for the process (see
/// `regclient::detours_installed`). Recorded whether or not stats are on.
pub(super) static REG_OVERLAY_DISABLED: OnceLock<&'static str> = OnceLock::new();

/// The registry overlay is off because the detour `name` could not be installed.
pub fn note_reg_overlay_disabled(name: &'static str) {
    let _ = REG_OVERLAY_DISABLED.set(name);
}

/// The registry detour whose absence turned the registry overlay off, if one did.
pub fn reg_overlay_disabled_by() -> Option<&'static str> {
    REG_OVERLAY_DISABLED.get().copied()
}

/// Registry overlay writes refused because the director could not be asked (a lookup the write
/// needed failed, or the write request itself did not reach it): each one returned
/// `STATUS_UNSUCCESSFUL` and wrote nothing, real registry included (spec section 6). Kept apart
/// from [`REG_READ_FALLBACKS`], which are reads served from the real registry. Counted whether or
/// not stats are on.
pub(super) static REG_WRITE_REFUSED: AtomicU64 = AtomicU64::new(0);

/// A registry overlay write was refused for want of the director.
pub fn note_reg_write_refused() {
    REG_WRITE_REFUSED.fetch_add(1, Ordering::Relaxed);
}

/// How many registry overlay writes were refused for want of the director so far.
pub fn reg_write_refused_count() -> u64 {
    REG_WRITE_REFUSED.load(Ordering::Relaxed)
}

/// What happened to registry change notifications served by the overlay (`regnotify`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum RegNotify {
    /// A notification was registered as an overlay waiter.
    Registered = 0,
    /// A waiter completed because the overlay changed under its key.
    Completed = 1,
    /// A waiter ended with `STATUS_NOTIFY_CLEANUP` because its key handle was closed.
    CleanedUp = 2,
    /// A notifier poll (`REG_CHANGED`) the director did not answer; its waiters kept waiting.
    PollError = 3,
}

/// [`RegNotify`] counts. Counted whether or not stats are on.
pub(super) static REG_NOTIFY: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

pub fn note_reg_notify(e: RegNotify) {
    REG_NOTIFY[e as usize].fetch_add(1, Ordering::Relaxed);
}

pub fn reg_notify_count(e: RegNotify) -> u64 {
    REG_NOTIFY[e as usize].load(Ordering::Relaxed)
}

/// Live registry key handles the shim tracks, and the most there have been at once: synthetic
/// (virtual) and pass-through (spec section 6). Updated by `regkeys` whenever a table changes,
/// so the report reads atomics rather than the tables' locks.
pub(super) static REG_HANDLES: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// The synthetic key handle table now holds `n` handles.
pub fn note_reg_virtual_handles(n: usize) {
    REG_HANDLES[0].store(n as u64, Ordering::Relaxed);
    REG_HANDLES[1].fetch_max(n as u64, Ordering::Relaxed);
}

/// The pass-through key handle table now holds `n` handles.
pub fn note_reg_passthrough_handles(n: usize) {
    REG_HANDLES[2].store(n as u64, Ordering::Relaxed);
    REG_HANDLES[3].fetch_max(n as u64, Ordering::Relaxed);
}

/// The registry overlay's own counters, read once per report.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct RegCounters {
    pub(super) write_refused: u64,
    pub(super) notify: [u64; 4],
    /// Live virtual, peak virtual, live pass-through, peak pass-through.
    pub(super) handles: [u64; 4],
}

pub(super) fn reg_counters() -> RegCounters {
    RegCounters {
        write_refused: reg_write_refused_count(),
        notify: std::array::from_fn(|i| REG_NOTIFY[i].load(Ordering::Relaxed)),
        handles: std::array::from_fn(|i| REG_HANDLES[i].load(Ordering::Relaxed)),
    }
}

pub(super) fn render_reg_fallbacks(snap: &Snapshot) -> String {
    let mut out = String::new();
    let r = &snap.reg;
    if r.handles[1] != 0 || r.handles[3] != 0 {
        out.push_str(&format!(
            "\nregistry key handles: {} virtual (peak {}), {} pass-through (peak {})\n",
            r.handles[0], r.handles[1], r.handles[2], r.handles[3]
        ));
    }
    if r.notify.iter().any(|&n| n != 0) {
        out.push_str(&format!(
            "\nregistry notifications: {} registered, {} completed, {} cleaned up, {} poll \
             errors\n",
            r.notify[RegNotify::Registered as usize],
            r.notify[RegNotify::Completed as usize],
            r.notify[RegNotify::CleanedUp as usize],
            r.notify[RegNotify::PollError as usize]
        ));
    }
    if r.write_refused != 0 {
        out.push_str(&format!(
            "\nregistry overlay writes refused after a director failure: {}\n",
            r.write_refused
        ));
    }
    if let Some(name) = reg_overlay_disabled_by() {
        out.push_str(&format!(
            "\nregistry overlay disabled: detour {name} not installed\n"
        ));
    }
    if snap.reg_read_fallbacks != 0 {
        out.push_str(&format!(
            "\nregistry overlay reads served from the real registry after a director failure: {}\n",
            snap.reg_read_fallbacks
        ));
    }
    if snap.reg_close_lock_given_up != 0 {
        out.push_str(&format!(
            "\nregistry key handle records left behind because their table stayed locked: {}\n",
            snap.reg_close_lock_given_up
        ));
    }
    if snap.reg_unresolved != 0 {
        out.push_str(&format!(
            "\nregistry opens passed through because their key path could not be resolved: {}\n",
            snap.reg_unresolved
        ));
    }
    out
}

/// The label of the name-query row, for anything that parses the report.
pub const NAME_QUERY_LABEL: &str = "final-path name queries";

pub(super) fn render_name_queries(snap: &Snapshot) -> String {
    if snap.name_queries == 0 {
        return String::new();
    }
    format!(
        "\n{NAME_QUERY_LABEL}:\n  {} queries / {} answered from the shim's cache / {} director lookups\n",
        snap.name_queries, snap.name_queries_cached, snap.name_lookups
    )
}

//! Child processes the shim refused to start: the counter set that says a spawn failed closed.

use super::*;

/// Child processes whose injection failed and which were therefore killed,
/// with their `CreateProcess` call failed, rather than run without the shim.
///
/// Ungated for the same reason as the hook-panic counters: it is zero in a
/// healthy run, costs one relaxed `fetch_add` when it fires, and a launcher
/// that sees its game vanish needs this number even though stats were off.
/// Keyed by [`crate::child::ChildInjectError::label`].
pub(super) static CHILD_INJECT_REFUSED_TOTAL: AtomicU64 = AtomicU64::new(0);
pub(super) static CHILD_INJECT_REFUSED: BoundedTally<&'static str> = BoundedTally::unbounded();

/// Record a child that was killed because it could not be injected.
pub(crate) fn note_child_inject_refused(reason: &'static str) {
    CHILD_INJECT_REFUSED_TOTAL.fetch_add(1, Ordering::Relaxed);
    CHILD_INJECT_REFUSED.add(reason);
}

/// How many children have been refused process-wide.
pub fn child_inject_refused_total() -> u64 {
    CHILD_INJECT_REFUSED_TOTAL.load(Ordering::Relaxed)
}

/// How many were refused for one reason (a `ChildInjectError` label).
pub fn child_inject_refused_count(reason: &str) -> u64 {
    CHILD_INJECT_REFUSED.count(reason)
}

/// Refused children, rendered right after the caught panics: like a panic, one
/// of these invalidates the reading of everything below it for that process
/// tree (a child the game expected is not running).
pub(super) fn render_child_inject_refused(snap: &Snapshot) -> String {
    let total = snap.child_inject_refused_total;
    if total == 0 {
        return String::new();
    }
    let mut rows: Vec<(&&'static str, &u64)> = snap.child_inject_refused.iter().collect();
    rows.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let mut s = format!(
        "CHILD PROCESSES REFUSED: {total} child process(es) could not be injected and were \
         killed, their CreateProcess calls failing, rather than run without the shim.\n"
    );
    for (reason, c) in rows {
        s.push_str(&format!("  {reason:<32} {c:>8}\n"));
    }
    s
}

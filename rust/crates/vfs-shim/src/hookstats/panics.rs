//! Hook panics caught at the `extern "system"` boundary: the one counter set not gated on `enabled()`.

use super::*;

/// Hook invocations whose body panicked and was contained at the
/// `extern "system"` boundary rather than taking the process down.
///
/// **These are the one set of counters here that are not gated on
/// [`enabled`].** Everything else in this module is sampling instrumentation
/// with a per-call cost, off by default so an uninstrumented run pays nothing.
/// A caught panic is not a sample: it happens zero times in a healthy process,
/// costs one relaxed `fetch_add` when it does, and is a bug that already
/// happened. Gating it would mean the *default* configuration — the one a live
/// session runs under — is the one that cannot tell you a hook faulted, which
/// is the exact failure mode `hook::contain_panic` exists to stop being silent.
///
/// Two channels carry a caught panic, and they answer different questions.
/// `hook::install_panic_hook`'s log gets the message, location and thread at
/// panic time and is the *only* record when stats are off. This pair is the
/// aggregate: how many, and in which entry point — which is what says whether
/// a session hit one fault or is faulting on every read.
///
/// Keyed by the NT export name rather than by [`Hook`] so the
/// `CreateProcessInternalW` detour (a `kernelbase` export, and a `BOOL` return
/// rather than an `NTSTATUS`) can be counted alongside the ntdll hooks without
/// giving it a [`Hook`] discriminant it does not otherwise need. The total is a
/// separate atomic so a poisoned map still cannot hide that *something*
/// panicked.
pub(super) static HOOK_PANICS_TOTAL: AtomicU64 = AtomicU64::new(0);
pub(super) static HOOK_PANICS: Mutex<Option<HashMap<&'static str, u64>>> = Mutex::new(None);

/// Record a panic caught at a hook's `extern "system"` boundary. `name` is the
/// hooked export, e.g. `"NtCreateFile"`.
pub fn note_hook_panic(name: &'static str) {
    HOOK_PANICS_TOTAL.fetch_add(1, Ordering::Relaxed);
    let Ok(mut g) = HOOK_PANICS.lock() else {
        return;
    };
    *g.get_or_insert_with(HashMap::new).entry(name).or_insert(0) += 1;
}

/// How many hook panics have been caught process-wide. Zero in a healthy run;
/// any other value is a bug that already happened.
pub fn hook_panics_total() -> u64 {
    HOOK_PANICS_TOTAL.load(Ordering::Relaxed)
}

/// How many were caught in one named entry point. `pub` for the same reason
/// [`outcome_count`] is: a test can pin a class at zero, or watch it move.
pub fn hook_panic_count(name: &str) -> u64 {
    HOOK_PANICS
        .lock()
        .ok()
        .and_then(|g| g.as_ref().and_then(|m| m.get(name).copied()))
        .unwrap_or(0)
}

/// Caught panics, rendered **first** in the report rather than with the other
/// outcome tables at the bottom.
///
/// Position is the point. The sections below this one describe a shim that is
/// working — which paths were opened, which were routed, how long each hook
/// took. A caught panic invalidates that reading for whichever call hit it: the
/// hook returned `STATUS_UNSUCCESSFUL` without doing its job, and the game saw
/// a file operation fail for a reason nothing else in the report explains. A
/// reader who scrolls past a `TOTAL` line and a thousand path rows before
/// reaching it has already formed the wrong conclusion.
pub(super) fn render_hook_panics(snap: &Snapshot) -> String {
    let total = snap.hook_panics_total;
    if total == 0 {
        return String::new();
    }
    let mut rows: Vec<(&&'static str, &u64)> = snap.hook_panics.iter().collect();
    rows.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let mut s = format!(
        "CAUGHT PANICS: {total} hook invocation(s) panicked and were contained. Each one is a \
         bug, and each returned a failure status to the game instead of doing its job — see the \
         shim panic log (VFS_SHIM_PANIC_LOG) for messages and locations.\n"
    );
    for (name, c) in rows {
        s.push_str(&format!("  {name:<32} {c:>8}\n"));
    }
    s
}

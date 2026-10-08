//! Serialising and bounding the tests that launch a real child process.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use vfs_embed::{LaunchOpts, Session};

/// `Session::launch` configures the injected child through **process-global**
/// environment variables (`IpcServe::apply_env_roots`'s own comment: "for the
/// injected child (and single-session hosts)"). Any two tests in this binary
/// that each create a session and launch a real child process race on that
/// global env under the default (parallel) test harness — whichever
/// session's `apply_env_roots` fires last wins for the whole process, so a child
/// can silently connect to the *other* test's ring/session instead of its
/// own. Flip-tested: without this lock, running this file's launching tests
/// together is intermittently flaky (a write lands nowhere the assertions
/// expect) even though each passes reliably alone.
///
/// The director's open counts (`vfs_embed::open_totals`) are process-global
/// too, and the reconciliation tests read a delta of them across one launch,
/// which is a second reason a launch must not overlap another.
///
/// This is the project's stated convention for a test touching process-global
/// state (see `VA_LOCK` in `vfs-shim::lazy_section`) rather than moving every
/// launching test into its own binary. Take it with [`lock_launches`].
pub static LAUNCH_LOCK: Mutex<()> = Mutex::new(());

/// Hold [`LAUNCH_LOCK`] for the rest of the calling test. A test that panicked
/// while holding it poisons it; the next one takes it anyway, since the lock
/// guards no data, only the order of launches.
pub fn lock_launches() -> MutexGuard<'static, ()> {
    LAUNCH_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// How long a launched fixture may take before this file calls it wedged.
///
/// Override with `VFS_TEST_LAUNCH_TIMEOUT_SECS` (the `VFS_TEST_` prefix is
/// exempt from `vfs-env`'s registry lint by design, for exactly this kind of
/// harness-only knob).
pub fn launch_timeout() -> Duration {
    std::env::var("VFS_TEST_LAUNCH_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(Duration::from_secs(240), Duration::from_secs)
}

/// Launch under `session` and wait for the child's exit code, **bounded**.
///
/// `Session::launch` with `wait: true` carries no timeout of its own, so a
/// fixture that wedges — or an injected shim that never lets it exit — hangs
/// the awaiting test forever. That is not a private failure. Every launching
/// test in this file serialises on [`LAUNCH_LOCK`], so one wedge queues all
/// the others behind it and the binary stops with nothing to diagnose from.
///
/// Measured on CI 2026-09-02: four tests reported "running for over 60 seconds"
/// at the same instant, and the job was still stuck 43 minutes later having
/// produced no further output. Three of those four were victims of the lock, not
/// independent failures.
///
/// A bound turns that into one legible failure, releases the lock so the
/// remaining tests still report honestly, and — the reason this matters — makes
/// the underlying wedge diagnosable at all, because the panic says what was
/// being launched. The launch runs on its own thread, which a stall leaves
/// behind (holding its clone of the session) when the test panics.
///
/// A launch that returns an error panics with it: a launch that could not
/// start is never an exit code.
pub fn launch_bounded(session: &Arc<Session>, opts: LaunchOpts, label: &str) -> i32 {
    let (tx, rx) = std::sync::mpsc::channel();
    let s = Arc::clone(session);
    let image = opts.image.clone();
    std::thread::spawn(move || {
        let _ = tx.send(s.launch(&opts));
    });
    match rx.recv_timeout(launch_timeout()) {
        Ok(Ok(code)) => code,
        Ok(Err(e)) => panic!("launch of {image} ({label}) failed: {e}"),
        Err(_) => panic!(
            "launch of {image} stalled after {:?} ({label}): the child never exited, so it is \
             wedged, died without the launch noticing, or the launch itself never got going — \
             suspect artifact staging before the fixture. Raise VFS_TEST_LAUNCH_TIMEOUT_SECS \
             if this machine is merely slow.",
            launch_timeout(),
        ),
    }
}

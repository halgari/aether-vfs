//! Serialising and bounding the tests that launch a real child process.

use std::time::Duration;

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
/// This is the project's stated convention for a test touching process-global
/// state (see `VA_LOCK` in `vfs-shim::lazy_section`) rather than moving every
/// launching test into its own binary. An async `tokio::sync::Mutex`, not
/// `std::sync::Mutex`: the guard is held across `.await` points for this
/// test's whole session lifecycle, which clippy's `await_holding_lock` rightly
/// refuses for a std lock.
pub static LAUNCH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

/// Drain a launch event stream to completion, **bounded**.
///
/// `stream.message().await` carries no timeout of its own, so a fixture that
/// wedges — or an injected shim that never lets it exit — hangs the awaiting
/// test forever. That is not a private failure. Every launching test in this
/// file serialises on [`LAUNCH_LOCK`], so one wedge queues all the others behind
/// it and the binary stops with nothing to diagnose from.
///
/// Measured on CI 2026-09-02: four tests reported "running for over 60 seconds"
/// at the same instant, and the job was still stuck 43 minutes later having
/// produced no further output. Three of those four were victims of the lock, not
/// independent failures.
///
/// A bound turns that into one legible failure, releases the lock so the
/// remaining tests still report honestly, and — the reason this matters — makes
/// the underlying wedge diagnosable at all, because the panic says what had been
/// seen before the stall.
pub async fn drain_launch_events(
    stream: &mut tonic::Streaming<vfs_control::pb::LaunchEvent>,
    label: &str,
    exit_code: &mut Option<i32>,
) {
    let mut events = 0usize;
    // `Started` is the discriminator that makes a future stall conclusive:
    // seen-but-no-Exited means the child launched and then wedged, while never
    // seeing it means the launch itself never got off the ground. Without this
    // the panic can only say "no Exited", which fits both causes.
    let mut saw_started = false;
    let drain = async {
        while let Some(ev) = stream.message().await.expect("stream") {
            events += 1;
            match ev.event {
                Some(vfs_control::pb::launch_event::Event::Exited(x)) => *exit_code = Some(x.code),
                Some(vfs_control::pb::launch_event::Event::Started(_)) => saw_started = true,
                Some(vfs_control::pb::launch_event::Event::Log(l)) => {
                    eprintln!("{label}: {}", l.line)
                }
                None => {}
            }
        }
    };
    if tokio::time::timeout(launch_timeout(), drain).await.is_err() {
        panic!(
            "launch event stream stalled after {:?} ({label}): {events} event(s) seen,              saw_started={saw_started}, exit_code={exit_code:?}. {}. Raise              VFS_TEST_LAUNCH_TIMEOUT_SECS if this machine is merely slow.",
            launch_timeout(),
            if saw_started {
                "The child STARTED and then never reported Exited, so it is wedged or died                  without the daemon noticing"
            } else {
                "The child never even reported Started, so the launch itself did not get                  going — suspect artifact staging or the daemon, not the fixture"
            }
        );
    }
}

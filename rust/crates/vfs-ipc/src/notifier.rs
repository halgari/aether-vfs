//! Notifier trait + SpinNotifier.

/// Advisory wakeups. Correctness rests on the ring atomics; a `Notifier` only
/// avoids busy-spinning. The real implementation (deferred) uses Nt events /
/// NtWaitForAlertByThreadId. Default methods are no-ops so impls override only
/// what they need.
pub trait Notifier {
    fn notify_server(&self) {}
    fn wait_server(&self) {}
    fn notify_client(&self, _slot: u32) {}
    fn wait_client(&self, _slot: u32) {}
    fn notify_slot_free(&self) {}
}

/// Ships this slice: pure spin. Waits are `spin_loop()` hints; notifies are
/// no-ops. Endpoints stay correct because they re-check the atomics in a loop.
pub struct SpinNotifier;

impl Notifier for SpinNotifier {
    fn wait_server(&self) {
        core::hint::spin_loop();
    }
    fn wait_client(&self, _slot: u32) {
        core::hint::spin_loop();
    }
}

/// Spin while the ring is hot, sleep once it goes quiet. Needs no OS object.
///
/// This exists for the Wine path. There the shim's `notify_server` cannot signal
/// anything a native Linux Director could wait on — it would be setting a *Wine*
/// event object — so the server has no waker and [`SpinNotifier`] was the only
/// correct choice. Correct, but expensive: `DEFAULT_WORKER_COUNT` is 4 and every
/// worker spins, so an idle session pinned four cores at 100%.
///
/// The trade this makes instead is the one `vfs_win::EventNotifier` makes, minus
/// the event: spin for a short window after the last completed request, where
/// the next is very likely to arrive (measured round trips are 20-209
/// microseconds), and sleep once that window lapses. Idle cost falls to roughly
/// nothing; the worst case adds one sleep quantum to a request arriving just
/// after the window closes.
///
/// A sleeping server is safe here **only** because notifiers are advisory —
/// correctness rests on the ring atomics and both ends re-check them in a loop.
/// A missed wakeup costs latency, never a lost request.
///
/// Strictly better on Linux than the Windows equivalent, which asks for a 1 ms
/// timeout and gets ~15.6 ms because Windows' default timer resolution rounds it
/// up. `thread::sleep` does not.
pub struct AdaptiveNotifier;

/// How long after a completed request a server thread keeps spinning.
///
/// Overridable through the same `VFS_RING_SPIN_US` switch `EventNotifier` reads,
/// so one knob tunes both transports. Read directly rather than through
/// `vfs-env` because this crate deliberately carries no dependency but
/// `vfs-protocol`; the name is registered there, which is what the lint needs.
fn spin_window() -> core::time::Duration {
    static US: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    core::time::Duration::from_micros(*US.get_or_init(|| {
        std::env::var("VFS_RING_SPIN_US")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(400)
    }))
}

std::thread_local! {
    /// When this server thread last completed a request. Drives hot vs idle.
    static LAST_SERVED: core::cell::Cell<Option<std::time::Instant>> =
        const { core::cell::Cell::new(None) };
}

/// Idle sleep, grown with the length of the quiet period.
///
/// Short right after activity so a returning burst is picked up promptly,
/// longer once the ring has been silent, capped so a woken game never waits
/// long. The cap matters more than the floor: a session idle for an hour should
/// cost nothing, but must not add a visible stall to the next file open.
fn idle_sleep(quiet_for: core::time::Duration) -> core::time::Duration {
    const CAP: core::time::Duration = core::time::Duration::from_micros(2000);
    const FLOOR: core::time::Duration = core::time::Duration::from_micros(100);
    if quiet_for > core::time::Duration::from_millis(50) {
        CAP
    } else if quiet_for > core::time::Duration::from_millis(5) {
        core::time::Duration::from_micros(500)
    } else {
        FLOOR
    }
}

impl Notifier for AdaptiveNotifier {
    fn wait_server(&self) {
        let window = spin_window();
        let last = LAST_SERVED.with(|c| c.get());
        let hot = !window.is_zero() && last.is_some_and(|t| t.elapsed() < window);
        if hot {
            // Return promptly so the caller re-checks the ring atomics.
            for _ in 0..64 {
                core::hint::spin_loop();
            }
            return;
        }
        let quiet_for = last.map_or(core::time::Duration::MAX, |t| t.elapsed());
        std::thread::sleep(idle_sleep(quiet_for));
    }

    /// Called on the server thread once a request completes, so it is the
    /// natural place to record that the ring is active.
    fn notify_client(&self, _slot: u32) {
        LAST_SERVED.with(|c| c.set(Some(std::time::Instant::now())));
    }

    fn wait_client(&self, _slot: u32) {
        core::hint::spin_loop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_notifier_methods_are_callable() {
        let n = AdaptiveNotifier;
        n.notify_server();
        n.notify_client(0);
        n.wait_server();
        n.wait_client(0);
        n.notify_slot_free();
    }

    #[test]
    fn adaptive_spins_while_hot_rather_than_sleeping() {
        let n = AdaptiveNotifier;
        // A completion was just recorded, so the wait must return faster than
        // even the 100us sleep floor — proving it took the spin path.
        n.notify_client(0);
        let t = std::time::Instant::now();
        n.wait_server();
        assert!(
            t.elapsed() < core::time::Duration::from_micros(100),
            "a hot wait must spin, not sleep: took {:?}",
            t.elapsed()
        );
    }

    #[test]
    fn idle_sleep_grows_with_the_quiet_period_and_stays_capped() {
        use core::time::Duration;
        let floor = idle_sleep(Duration::from_micros(0));
        let mid = idle_sleep(Duration::from_millis(10));
        let long = idle_sleep(Duration::from_secs(3600));
        assert!(floor < mid && mid < long, "{floor:?} {mid:?} {long:?}");
        assert!(
            long <= Duration::from_micros(2000),
            "an hour of silence must still not add a visible stall: {long:?}"
        );
    }

    #[test]
    fn spin_notifier_methods_are_callable() {
        let n = SpinNotifier;
        n.notify_server();
        n.wait_server();
        n.notify_client(0);
        n.wait_client(0);
        n.notify_slot_free();
    }
}

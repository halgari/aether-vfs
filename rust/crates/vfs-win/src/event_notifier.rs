//! **B4:** Windows event-based `Notifier` for the control ring.
#![allow(unsafe_code)]

use std::cell::Cell;
use std::io;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use vfs_ipc::Notifier;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Threading::{
    CreateEventW, OpenEventW, ResetEvent, SetEvent, WaitForSingleObject, INFINITE,
};

const EVENT_ALL_ACCESS: u32 = 0x1F_0003;

/// Named auto-reset events for server/client ring wakeups.
pub struct EventNotifier {
    server_ev: HANDLE,
    client_ev: HANDLE,
    owns: bool,
}

impl EventNotifier {
    /// Create the named events (director side).
    pub fn create(server_name: &str, client_name: &str) -> io::Result<Self> {
        let s = wide(server_name);
        let c = wide(client_name);
        // SAFETY: CreateEventW with valid names; bManualReset=FALSE (auto-reset).
        let server_ev = unsafe { CreateEventW(core::ptr::null(), 0, 0, s.as_ptr()) };
        if server_ev.is_null() {
            return Err(io::Error::last_os_error());
        }
        let client_ev = unsafe { CreateEventW(core::ptr::null(), 0, 0, c.as_ptr()) };
        if client_ev.is_null() {
            unsafe { CloseHandle(server_ev) };
            return Err(io::Error::last_os_error());
        }
        Ok(EventNotifier {
            server_ev,
            client_ev,
            owns: true,
        })
    }

    /// Open existing named events (shim / client side).
    pub fn open(server_name: &str, client_name: &str) -> io::Result<Self> {
        let s = wide(server_name);
        let c = wide(client_name);
        let server_ev = unsafe { OpenEventW(EVENT_ALL_ACCESS, 0, s.as_ptr()) };
        if server_ev.is_null() {
            return Err(io::Error::last_os_error());
        }
        let client_ev = unsafe { OpenEventW(EVENT_ALL_ACCESS, 0, c.as_ptr()) };
        if client_ev.is_null() {
            unsafe { CloseHandle(server_ev) };
            return Err(io::Error::last_os_error());
        }
        Ok(EventNotifier {
            server_ev,
            client_ev,
            owns: true,
        })
    }
}

/// How long a client thread spins for its response before sleeping.
///
/// Measured service time for a ring RPC is 20–209 µs (`docs/benchmarks/
/// c-throughput-delta.md`), so a budget a little above that covers the common
/// case without sleeping. Tunable via `VFS_RING_SPIN_US`; `0` restores the
/// old sleep-immediately behaviour.
fn spin_budget() -> Duration {
    static US: OnceLock<u64> = OnceLock::new();
    Duration::from_micros(*US.get_or_init(|| {
        std::env::var("VFS_RING_SPIN_US")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(400)
    }))
}

thread_local! {
    /// Deadline until which this thread spins instead of sleeping. Set when the
    /// thread submits a request, so each RPC gets a fresh budget.
    static SPIN_UNTIL: Cell<Option<Instant>> = const { Cell::new(None) };
    /// When this server thread last completed a request. Drives the hot/idle
    /// decision in `wait_server`.
    static SRV_ACTIVE: Cell<Option<Instant>> = const { Cell::new(None) };
}

impl Notifier for EventNotifier {
    fn notify_server(&self) {
        // The submitting thread calls this immediately before its wait loop, so
        // it is the natural place to open a fresh spin window for that RPC.
        let budget = spin_budget();
        if !budget.is_zero() {
            SPIN_UNTIL.with(|c| c.set(Some(Instant::now() + budget)));
        }
        unsafe {
            let _ = SetEvent(self.server_ev);
        }
    }
    /// Spin while the ring is hot, sleep once it goes quiet.
    ///
    /// The client half is `SpinNotifier` (see `vfs-shim`'s `fuse_client`), whose
    /// `notify_server` is a **no-op** — nothing ever signals `server_ev`. So this
    /// wait only ever ended on its 1 ms timeout, and a 1 ms timeout does not
    /// wake in 1 ms: Windows' default timer resolution is 15.6 ms. The director
    /// was therefore discovering requests at ~15 ms intervals, which is exactly
    /// the 1.6–9.4 ms per-RPC cost measured at the hooks.
    ///
    /// Spinning right after serving a request costs a little CPU during a burst
    /// and none at idle, where the timed wait still applies.
    fn wait_server(&self) {
        let budget = spin_budget();
        let hot = !budget.is_zero()
            && SRV_ACTIVE.with(|c| c.get()).is_some_and(|t| t.elapsed() < budget);
        if hot {
            // Return promptly so the caller re-checks the ring atomics.
            for _ in 0..64 {
                core::hint::spin_loop();
            }
            return;
        }
        unsafe {
            let _ = WaitForSingleObject(self.server_ev, 1);
        }
    }
    fn notify_client(&self, _slot: u32) {
        // Runs on the server thread once a request is complete: the ring is
        // active, so keep `wait_server` spinning for the next one.
        if !spin_budget().is_zero() {
            SRV_ACTIVE.with(|c| c.set(Some(Instant::now())));
        }
        unsafe {
            let _ = SetEvent(self.client_ev);
        }
    }
    /// Spin for the remaining budget, then fall back to the event.
    ///
    /// Sleeping first cost ~100x the work being waited for. `WaitForSingleObject`
    /// with a 1 ms timeout does **not** wake in 1 ms: Windows' default timer
    /// resolution is 15.6 ms, so a missed signal rounds up to the next tick. And
    /// signals are missable here — `client_ev` is a single auto-reset event
    /// shared by every client thread, so one `SetEvent` wakes one arbitrary
    /// waiter, not necessarily the thread whose slot completed.
    fn wait_client(&self, _slot: u32) {
        let spinning = SPIN_UNTIL.with(|c| match c.get() {
            Some(until) if Instant::now() < until => true,
            Some(_) => {
                c.set(None); // budget spent; sleep from here on
                false
            }
            None => false,
        });
        if spinning {
            // Return promptly so the caller re-checks the ring atomics.
            for _ in 0..64 {
                core::hint::spin_loop();
            }
            return;
        }
        unsafe {
            let _ = WaitForSingleObject(self.client_ev, 1);
        }
    }
    fn notify_slot_free(&self) {
        // Wake server if it was spinning on a full ring.
        unsafe {
            let _ = SetEvent(self.server_ev);
        }
    }
}

impl Drop for EventNotifier {
    fn drop(&mut self) {
        if self.owns {
            unsafe {
                let _ = CloseHandle(self.server_ev);
                let _ = CloseHandle(self.client_ev);
            }
        }
    }
}

// SAFETY: HANDLEs are used only via Notifier methods with atomics for correctness.
unsafe impl Send for EventNotifier {}
unsafe impl Sync for EventNotifier {}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(core::iter::once(0)).collect()
}

#[allow(dead_code)]
fn reset(ev: HANDLE) {
    unsafe {
        let _ = ResetEvent(ev);
    }
}

#[cfg(test)]
mod spin_tests {
    use super::*;

    #[test]
    fn spin_budget_defaults_to_covering_rpc_service_time() {
        // Measured RPC service is 20-209 us; the budget must exceed it or the
        // hybrid degrades to the sleep it replaces.
        assert!(spin_budget() >= Duration::from_micros(209), "{:?}", spin_budget());
    }

    #[test]
    fn spin_window_expires_and_latches_off() {
        SPIN_UNTIL.with(|c| c.set(Some(Instant::now() - Duration::from_micros(1))));
        // An expired budget must clear itself so the thread sleeps rather than
        // re-evaluating an old deadline on every wait.
        let expired = SPIN_UNTIL.with(|c| match c.get() {
            Some(until) if Instant::now() < until => true,
            Some(_) => {
                c.set(None);
                false
            }
            None => false,
        });
        assert!(!expired);
        assert!(SPIN_UNTIL.with(|c| c.get()).is_none(), "budget must latch off");
    }
}

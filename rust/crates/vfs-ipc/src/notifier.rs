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

#[cfg(test)]
mod tests {
    use super::*;

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

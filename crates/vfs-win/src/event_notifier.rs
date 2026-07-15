//! **B4:** Windows event-based `Notifier` for the control ring.
#![allow(unsafe_code)]

use std::io;

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

impl Notifier for EventNotifier {
    fn notify_server(&self) {
        unsafe {
            let _ = SetEvent(self.server_ev);
        }
    }
    fn wait_server(&self) {
        unsafe {
            let _ = WaitForSingleObject(self.server_ev, 1); // 1ms slice then re-check atomics
        }
    }
    fn notify_client(&self, _slot: u32) {
        unsafe {
            let _ = SetEvent(self.client_ev);
        }
    }
    fn wait_client(&self, _slot: u32) {
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

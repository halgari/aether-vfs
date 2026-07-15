//! Process VM helpers for director remote READ (WriteProcessMemory).

use std::io;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_OPERATION, PROCESS_VM_WRITE,
};

/// RAII process handle with rights to write into the remote address space.
pub struct ProcessVm {
    handle: HANDLE,
}

// SAFETY: HANDLE is process-scoped; we only call documented Win32 on it.
#[allow(unsafe_code)]
unsafe impl Send for ProcessVm {}
#[allow(unsafe_code)]
unsafe impl Sync for ProcessVm {}

impl ProcessVm {
    /// Open `pid` for `WriteProcessMemory`.
    pub fn open(pid: u32) -> io::Result<Self> {
        let access = PROCESS_VM_WRITE | PROCESS_VM_OPERATION | PROCESS_QUERY_INFORMATION;
        // SAFETY: OpenProcess with valid access mask; null return is failure.
        #[allow(unsafe_code)]
        let h = unsafe { OpenProcess(access, 0, pid) };
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(ProcessVm { handle: h })
    }

    /// Write `data` into the remote process at `va` (full write or error).
    pub fn write_at(&self, va: u64, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        if va == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "null VA"));
        }
        let mut written = 0usize;
        // SAFETY: process handle held by self; va is caller-validated by protocol.
        #[allow(unsafe_code)]
        let ok = unsafe {
            WriteProcessMemory(
                self.handle,
                va as *const core::ffi::c_void,
                data.as_ptr() as *const core::ffi::c_void,
                data.len(),
                &mut written,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if written != data.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!("short WPM: {written}/{}", data.len()),
            ));
        }
        Ok(())
    }
}

impl Drop for ProcessVm {
    fn drop(&mut self) {
        if !self.handle.is_null() && self.handle != INVALID_HANDLE_VALUE {
            // SAFETY: own handle from OpenProcess.
            #[allow(unsafe_code)]
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

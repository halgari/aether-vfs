//! Optional director → game memory writer for `FLAG_READ_REMOTE`.

/// Capability to place READ bytes at a virtual address in the registered game process.
///
/// Implemented by the director (Win32 `WriteProcessMemory`) or by benches/tests
/// (local pointer write via a thin adapter crate that allows `unsafe`).
pub trait RemoteMemWriter: Send + Sync {
    fn write_at(&self, va: u64, data: &[u8]) -> Result<(), i32>;
}

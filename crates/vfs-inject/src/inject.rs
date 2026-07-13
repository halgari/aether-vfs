//! All Win32 injection FFI (implemented in the next task).
use crate::{InjectError, RunConfig};

pub fn run_target_with_shim(_cfg: RunConfig) -> Result<i32, InjectError> {
    Err(InjectError::CreateProcess)
}

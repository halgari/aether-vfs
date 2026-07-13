#![deny(unsafe_code)]

//! Launch a target process, inject the shim DLL, and run it with file-open
//! redirection active.

use std::time::Duration;

mod inject;

/// Parameters for [`run_target_with_shim`].
pub struct RunConfig {
    pub target_exe: String,
    pub args: Vec<String>,
    pub dll_path: String,
    pub config_path: String,
    pub ready_path: String,
    pub ready_timeout: Duration,
}

/// Failure points in launch + inject + run.
#[derive(Debug)]
pub enum InjectError {
    CreateProcess,
    Alloc,
    Write,
    RemoteThread,
    Timeout,
    Wait,
    ExitCode,
}

pub use inject::run_target_with_shim;

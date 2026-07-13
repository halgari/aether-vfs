#![deny(unsafe_code)]

//! `vfs-shim`: installs an `NtCreateFile` detour that redirects opens of
//! virtualized paths to their mod backing files (in-process for now; injection
//! is a later slice).

mod engine;
mod hook;
mod ntdef;

pub use engine::{Engine, EngineError};
pub use hook::{install, HookGuard, InstallError};

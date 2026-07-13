#![deny(unsafe_code)]

//! `vfs-shim`: installs an `NtCreateFile` detour that redirects opens of
//! virtualized paths to their mod backing files (in-process for now; injection
//! is a later slice).

mod bootstrap;
mod engine;
mod hook;
mod ntdef;

pub use bootstrap::{bootstrap_from_config_path, decode_config, encode_config, BootstrapError};
pub use engine::{Engine, EngineError};
pub use hook::{install, HookGuard, InstallError};

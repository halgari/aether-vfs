//! Unix-side OS handles for the ring, and the process queries std lacks.
//!
//! The mirror of `vfs-win`: it owns this platform's shared-memory primitive and
//! exposes it as a [`vfs_ipc::SharedSeg`], so the ring code above
//! stays OS-independent. Everything here is `cfg(unix)`; on Windows this crate
//! builds to nothing.
#![deny(unsafe_code)]

#[cfg(unix)]
mod mapping;
#[cfg(unix)]
pub use mapping::FileMapping;
#[cfg(unix)]
mod process;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use process::{all_pids, process_argv0, process_cwd, process_name};
#[cfg(unix)]
pub use process::{effective_uid, process_alive, signal, Signal};

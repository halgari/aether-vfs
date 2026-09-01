//! Unix-side OS handles for the ring.
//!
//! The mirror of `vfs-win`: it owns this platform's shared-memory primitive and
//! exposes it as a [`vfs_ipc::SharedSeg`], so the ring and snapshot code above
//! stay OS-independent. Everything here is `cfg(unix)`; on Windows this crate
//! builds to nothing.
#![deny(unsafe_code)]

#[cfg(unix)]
mod mapping;
#[cfg(unix)]
pub use mapping::FileMapping;

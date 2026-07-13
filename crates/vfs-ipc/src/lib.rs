#![deny(unsafe_code)]
//! `vfs-ipc`: a recursion-free shared-memory control ring. Operates on a
//! caller-owned segment; imports no OS file/section/process API (G11). All
//! `unsafe` is confined to the `seg` module (`SharedSeg`).

pub mod endpoint;
pub mod layout;
pub mod notifier;
pub mod ring;
pub mod seg;

// pub use lines added by later tasks.

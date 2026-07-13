#![forbid(unsafe_code)]
//! `vfs-server`: authoritative side of the out-of-process VFS. Runs `vfs-core`,
//! publishes the `vfs-shared` snapshot, and services `vfs-ipc` requests.

pub mod handler;
pub mod proto;
pub mod server;

// pub use server::Server;  // uncommented in Task 4

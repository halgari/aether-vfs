#![forbid(unsafe_code)]
//! `vfs-server`: authoritative side of the out-of-process VFS. Runs `vfs-core`,
//! publishes the `vfs-shared` snapshot, and services `vfs-ipc` requests
//! including stateful OPEN/READ/CLOSE for the director FUSE path.

pub mod arena;
pub mod handler;
pub mod open_table;
pub mod proto;
pub mod server;

pub use arena::DataArena;
pub use open_table::OpenTable;
pub use server::{Server, DEFAULT_PAYLOAD_CAP, DEFAULT_WORKER_COUNT};

#![forbid(unsafe_code)]
//! Legacy tree + open-table ring server (`vfs-core` + zip-window sources).
//! Production hosts use `vfs-director` (`IpcServe` + backends).
//!
//! `DataArena` lives in `vfs-ipc` (re-exported here for older call sites).

pub mod handler;
pub mod open_table;
pub mod proto;
pub mod server;

pub use open_table::OpenTable;
pub use server::Server;
pub use vfs_ipc::{DataArena, DEFAULT_PAYLOAD_CAP, DEFAULT_WORKER_COUNT};

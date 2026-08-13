#![forbid(unsafe_code)]
//! Tree + open-table ring server (`vfs-core` + zip-window sources).
//!
//! **This is not the product path.** Hosts use `vfs-director` (`IpcServe` +
//! backends), which is the only thing that serves a real game. What keeps this
//! crate alive is `vfs-fuse-bench`: it measures ring RPC cost against a simple,
//! stable server, without the director's mounts, overlay resolution and
//! handle table in the measurement. Audited 2026-08-13 — `vfs-fuse-bench` and
//! this crate's own tests are the only dependents.
//!
//! Retire it when the benchmark can express the same thing against the
//! director, not before: the numbers in `docs/benchmarks/` are relative to it.
//!
//! `DataArena` lives in `vfs-ipc` (re-exported here for older call sites).

pub mod handler;
pub mod open_table;
pub mod proto;
pub mod server;

pub use open_table::OpenTable;
pub use server::Server;
pub use vfs_ipc::{DataArena, DEFAULT_PAYLOAD_CAP, DEFAULT_WORKER_COUNT};

#![deny(unsafe_code)]
//! `vfs-ipc`: a recursion-free shared-memory control ring + bulk data arena.
//! Operates on a caller-owned segment; imports no OS file/section/process API.
//! All `unsafe` is confined to the `seg` module (`SharedSeg`).

pub mod arena;
pub mod endpoint;
pub mod layout;
pub mod notifier;
pub mod ring;
pub mod seg;

pub use arena::{DataArena, DEFAULT_ARENA_BYTES, DEFAULT_PAYLOAD_CAP, DEFAULT_WORKER_COUNT};
pub use endpoint::{Request, Response, RingClient, RingServer};
pub use notifier::{Notifier, SpinNotifier};
pub use ring::{Geom, IpcError};
pub use seg::{OwnedSeg, SharedSeg};

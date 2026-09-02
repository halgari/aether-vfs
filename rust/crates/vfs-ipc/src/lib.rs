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

/// How long an endpoint waits for a published request to be answered.
///
/// Generous on purpose: a director that has not answered in a minute is broken,
/// not busy, and the point is only to convert a permanent hang into a reportable
/// error. Measured round trips are 20-209 microseconds.
pub const RESPONSE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

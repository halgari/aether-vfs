//! Director daemon library: discovery, gRPC service, session registry, and
//! helpers shared by the `vfs` CLI and integration tests.

mod apply;
mod client;
pub mod discovery;
mod flags;
mod server;
pub mod service;
pub mod sessions;
mod storage;

// compat: removed by cleanup stream I
#[doc(hidden)]
pub mod registry {
    pub use super::sessions::*;
}

pub use apply::*;
pub use client::*;
pub use discovery::{Discovery, default_discovery_path, read_discovery, write_discovery};
pub use flags::*;
pub use server::*;
pub use service::DirectorService;
pub use sessions::SessionRegistry;
pub use storage::*;

/// Bind address used when the caller does not pin one (ephemeral port).
pub const DEFAULT_BIND: &str = "127.0.0.1:0";

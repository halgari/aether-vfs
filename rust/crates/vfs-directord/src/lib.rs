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

pub use apply::*;
pub use client::*;
pub use discovery::{default_discovery_path, read_discovery, write_discovery, Discovery};
pub use flags::*;
pub use server::*;
pub use service::DirectorService;
pub use sessions::SessionRegistry;
pub use storage::*;

/// Bind address used when the caller does not pin one (ephemeral port).
pub const DEFAULT_BIND: &str = "127.0.0.1:0";

#[cfg(test)]
mod scratch_tmpdir {
    /// Sessions default to a directory under the system temp dir, and the daemons
    /// these tests spawn inherit this process's environment. Point both at `target/`.
    #[ctor::ctor]
    fn scratch_tmpdir() {
        vfs_testkit::use_scratch_as_tmpdir();
    }
}

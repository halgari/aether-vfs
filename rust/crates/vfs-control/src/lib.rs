//! Director control-plane: the generated gRPC contract (`pb`) plus the
//! declarative [`config`] schema shared by the daemon, the CLI, and tests.

/// Generated tonic types for the `vfs.director` package.
pub mod pb {
    tonic::include_proto!("vfs.director");
}

pub mod config;

pub use config::{
    load, CacheConfig, ConfigError, LaunchConfig, SessionConfig, SessionMeta, SourceEntry,
    SourceSpec,
};

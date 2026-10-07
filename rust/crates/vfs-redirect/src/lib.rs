#![forbid(unsafe_code)]

//! The injected shim's pure path core: which declared root, if any, an incoming NT path falls
//! under ([`RootMap`]), and its canonicalisation.

mod cache;
mod canon;
mod nt;
mod rootmap;
#[cfg(test)]
mod tests;
mod volumes;
mod whiteout;

pub use vfs_provider::RootId;

pub use cache::UncachedScope;
pub use canon::{canonicalise, split_stream_suffix, VolumeMap};
pub use nt::{counted_units, nt_to_volume_relative, to_nt, utf16_to_string, CountedErr};
pub use rootmap::RootMap;
pub use volumes::{expand_short_name, resolve_volume_map, resolve_volume_map_for};
pub use whiteout::{is_whiteout, WHITEOUT_SUFFIX};

/// What a [`RootMap`] lookup answers with: which declared root the path fell
/// under, and its folded remainder components beneath that root.
pub type RootHit = (RootId, Vec<String>);

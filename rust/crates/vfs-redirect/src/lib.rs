#![forbid(unsafe_code)]

//! The injected shim's pure path core: which declared root, if any, an incoming NT path falls
//! under ([`RootMap`]), its canonicalisation, and the NT layouts the hooks write.

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
pub use vfs_ntlayout::{
    filetime_of, write_dir_info, DirInfoClass, DirItem, DirStatus, DirWriteResult,
    FILE_APPEND_DATA, FILE_CREATE,
    FILE_OPEN, FILE_OPEN_IF, FILE_OVERWRITE, FILE_OVERWRITE_IF, FILE_SUPERSEDE, FILE_WRITE_DATA,
    GENERIC_ALL, GENERIC_WRITE, SYNTH_FILETIME, WRITE_ACCESS,
};
pub use volumes::{expand_short_name, resolve_volume_map, resolve_volume_map_for};
pub use whiteout::{is_whiteout, WHITEOUT_SUFFIX};

/// What a [`RootMap`] lookup answers with: which declared root the path fell
/// under, and its folded remainder components beneath that root.
pub type RootHit = (RootId, Vec<String>);

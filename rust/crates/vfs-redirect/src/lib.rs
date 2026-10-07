#![forbid(unsafe_code)]

//! The injected shim's pure redirect-decision core: map an incoming NT open path
//! + a published snapshot to pass-through vs redirect-to-backing-file.

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
pub use nt::{
    counted_units, nt_to_volume_relative, string_to_utf16, to_nt, utf16_to_string, CountedErr,
};
pub use rootmap::{Decision, RootMap};
pub use vfs_ntlayout::{
    classify_open, filetime_of, write_dir_info, write_file_name_info, DirInfoClass, DirItem,
    DirStatus, DirWriteResult, NameWriteResult, WriteIntent, FILE_APPEND_DATA, FILE_CREATE,
    FILE_OPEN, FILE_OPEN_IF, FILE_OVERWRITE, FILE_OVERWRITE_IF, FILE_SUPERSEDE, FILE_WRITE_DATA,
    GENERIC_ALL, GENERIC_WRITE, SYNTH_FILETIME, WRITE_ACCESS,
};
pub use volumes::{expand_short_name, resolve_volume_map, resolve_volume_map_for};
pub use whiteout::{is_whiteout, whiteout_marker, WHITEOUT_SUFFIX};

/// What a [`RootMap`] lookup answers with: which declared root the path fell
/// under, and its folded remainder components beneath that root.
pub type RootHit = (RootId, Vec<String>);

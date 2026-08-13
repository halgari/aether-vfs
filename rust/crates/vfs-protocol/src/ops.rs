//! Re-export of the provider contract, kept as a path for existing importers.
//!
//! The types live in `vfs-provider`; `vfs-protocol` owns only the ring wire
//! codecs and opcode catalog.

pub use vfs_provider::{
    bad_fh, bad_request, is_dir, map_io_err, not_a_dir, not_found, not_supported, ok, read_only,
    Access, Capabilities, DirEntry, Handle, Provider, RootId, SetAttr, Stat, VPath, KIND_DIR,
    KIND_FILE, KIND_TOMBSTONE,
};

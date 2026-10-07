//! Compat path for the provider contract; the items live in `vfs-provider`.

// compat: removed by cleanup stream I

pub use vfs_provider::{
    Access, Capabilities, CaseMatch, DirEntry, Handle, KIND_DIR, KIND_FILE, KIND_TOMBSTONE,
    Provider, RootId, SetAttr, Stat, VPath, bad_fh, bad_request, exists, is_dir, map_io_err,
    not_a_dir, not_found, not_supported, ok, read_only,
};

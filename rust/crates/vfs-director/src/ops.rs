//! Compat path for the provider contract; the items live in `vfs-provider`.
// compat: removed by cleanup stream I

pub use vfs_provider::{
    Access, Capabilities, CaseMatch, DirEntry, Handle, KIND_DIR, KIND_FILE, KIND_TOMBSTONE,
    OPEN_APPEND, OPEN_CREATE, OPEN_EXCL, OPEN_READ, OPEN_TRUNC, OPEN_WRITE, Provider, RootId,
    ST_BAD_FH, ST_BAD_REQUEST, ST_EXISTS, ST_IO_ERROR, ST_IS_DIR, ST_NO_SPACE, ST_NOT_A_DIRECTORY,
    ST_NOT_FOUND, ST_NOT_SUPPORTED, ST_OK, ST_READ_ONLY, ST_REPLY_TOO_LARGE, SetAttr, Stat, VPath,
    bad_fh, bad_request, exists, is_dir, map_io_err, not_a_dir, not_found, not_supported, ok,
    read_only,
};

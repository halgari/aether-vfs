//! Re-export the provider contract from `vfs-protocol` for a single import path.

pub use vfs_protocol::{
    bad_fh, bad_request, is_dir, map_io_err, not_a_dir, not_found, not_supported, ok, read_only,
    Access, Capabilities, DirEntry, Handle, Provider, RootId, SetAttr, Stat, VPath, KIND_DIR,
    KIND_FILE, KIND_TOMBSTONE, OPEN_CREATE, OPEN_EXCL, OPEN_READ, OPEN_TRUNC, OPEN_WRITE,
    ST_BAD_FH, ST_BAD_REQUEST, ST_IO_ERROR, ST_IS_DIR, ST_NOT_A_DIRECTORY, ST_NOT_FOUND,
    ST_NOT_SUPPORTED, ST_OK, ST_READ_ONLY,
};

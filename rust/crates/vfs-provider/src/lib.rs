#![forbid(unsafe_code)]
//! The provider contract: what a filesystem provider can do, how it is
//! addressed, and the conformance suite that holds every implementation —
//! Rust or host-language — to the same standard.

mod caps;
mod model;
mod path;
mod provider;
mod status;

pub use caps::{Access, Capabilities};
pub use model::{DirEntry, Handle, SetAttr, Stat, KIND_DIR, KIND_FILE, KIND_TOMBSTONE};
pub use path::{RootId, VPath};
pub use provider::Provider;
pub use status::{
    bad_fh, bad_request, is_dir, map_io_err, not_a_dir, not_found, not_supported, ok, read_only,
    OPEN_APPEND, OPEN_CREATE, OPEN_EXCL, OPEN_READ, OPEN_TRUNC, OPEN_WRITE, ST_BAD_FH,
    ST_BAD_REQUEST, ST_IO_ERROR, ST_IS_DIR, ST_NOT_A_DIRECTORY, ST_NOT_FOUND, ST_NOT_SUPPORTED,
    ST_NO_SPACE, ST_OK, ST_READ_ONLY,
};

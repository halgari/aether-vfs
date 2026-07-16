//! Backend operations for the userspace FUSE director.
//!
//! Shared by `vfs-director` (kernel) and backends (`vfs-zip`, disk, host C callbacks).
//! No OS I/O here — pure contract types + status helpers.
//!
//! # Kind constants
//! [`KIND_FILE`] / [`KIND_DIR`] / [`KIND_TOMBSTONE`] are **ops-layer** kinds used in
//! [`Stat`]. They are **not** the same encoding as `vfs-shared` snapshot node kinds.

use crate::{
    ST_BAD_FH, ST_BAD_REQUEST, ST_IO_ERROR, ST_IS_DIR, ST_NOT_A_DIRECTORY, ST_NOT_FOUND, ST_OK,
};

/// Ops-layer file kind (not `vfs-shared` snapshot kind).
pub const KIND_FILE: u8 = 1;
/// Ops-layer directory kind.
pub const KIND_DIR: u8 = 2;
/// Ops-layer tombstone kind (reserved for overlay delete).
pub const KIND_TOMBSTONE: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub kind: u8,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub stat: Stat,
}

pub type BackendHandle = u64;

/// Content backend: zip, disk, or host-provided C callbacks.
pub trait Backend: Send + Sync {
    fn getattr(&self, path: &str) -> Result<Option<Stat>, i32>;
    fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, i32>;
    /// Returns `(backend_handle, size, is_dir)`.
    fn open(&self, path: &str, flags: u32) -> Result<(BackendHandle, u64, bool), i32>;
    fn read(&self, bh: BackendHandle, offset: u64, buf: &mut [u8]) -> Result<usize, i32>;
    fn release(&self, bh: BackendHandle) -> Result<(), i32>;
}

pub fn map_io_err() -> i32 {
    ST_IO_ERROR
}
pub fn ok() -> i32 {
    ST_OK
}
pub fn not_found() -> i32 {
    ST_NOT_FOUND
}
pub fn bad_fh() -> i32 {
    ST_BAD_FH
}
pub fn is_dir() -> i32 {
    ST_IS_DIR
}
pub fn not_a_dir() -> i32 {
    ST_NOT_A_DIRECTORY
}
pub fn bad_request() -> i32 {
    ST_BAD_REQUEST
}

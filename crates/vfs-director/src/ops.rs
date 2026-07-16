//! Backend operations (Rust) — mirror of `include/vfs.h` ops.

use vfs_protocol::{
    ST_BAD_FH, ST_BAD_REQUEST, ST_IO_ERROR, ST_IS_DIR, ST_NOT_A_DIRECTORY, ST_NOT_FOUND, ST_OK,
};

pub const KIND_FILE: u8 = 1;
pub const KIND_DIR: u8 = 2;
pub const KIND_TOMBSTONE: u8 = 3;

pub const OPEN_READ: u32 = 1;
pub const OPEN_WRITE: u32 = 2;

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

/// Opaque handle owned by a backend until `release`.
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

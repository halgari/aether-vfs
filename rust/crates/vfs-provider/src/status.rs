//! Status codes crossing the provider boundary, and open-request flags.
//!
//! Values `0` through `-9` are fixed by the existing ring protocol — and by
//! the injected shim DLL, which matches statuses by number across the
//! process boundary — and must not be renumbered. New statuses append at the
//! next free negative number instead.

pub const ST_OK: i32 = 0;
pub const ST_NOT_FOUND: i32 = -1;
pub const ST_NOT_A_DIRECTORY: i32 = -2;
pub const ST_BAD_REQUEST: i32 = -3;
pub const ST_IO_ERROR: i32 = -4;
pub const ST_IS_DIR: i32 = -5;
pub const ST_BAD_FH: i32 = -6;
pub const ST_NO_SPACE: i32 = -7;
/// The provider does not implement this method.
pub const ST_NOT_SUPPORTED: i32 = -8;
/// No `ReadWrite` provider serves this path.
pub const ST_READ_ONLY: i32 = -9;
/// `OPEN_EXCL` (create-new) refused because the path already exists.
pub const ST_EXISTS: i32 = -10;

pub fn ok() -> i32 { ST_OK }
pub fn not_found() -> i32 { ST_NOT_FOUND }
pub fn not_a_dir() -> i32 { ST_NOT_A_DIRECTORY }
pub fn bad_request() -> i32 { ST_BAD_REQUEST }
pub fn map_io_err() -> i32 { ST_IO_ERROR }
pub fn is_dir() -> i32 { ST_IS_DIR }
pub fn bad_fh() -> i32 { ST_BAD_FH }
pub fn not_supported() -> i32 { ST_NOT_SUPPORTED }
pub fn read_only() -> i32 { ST_READ_ONLY }
pub fn exists() -> i32 { ST_EXISTS }

/// Open wants read access.
pub const OPEN_READ: u32 = 1;
/// Open wants write access.
pub const OPEN_WRITE: u32 = 2;
/// Create if absent (`OPEN_ALWAYS` / `CREATE_ALWAYS`).
pub const OPEN_CREATE: u32 = 4;
/// Fail if present (`CREATE_NEW`).
pub const OPEN_EXCL: u32 = 8;
/// Truncate on open (`TRUNCATE_EXISTING`).
pub const OPEN_TRUNC: u32 = 16;
/// Append-only writes (`FILE_APPEND_DATA`); the director resolves the offset.
pub const OPEN_APPEND: u32 = 32;

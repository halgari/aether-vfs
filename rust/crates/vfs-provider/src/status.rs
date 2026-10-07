//! Status codes crossing the provider boundary, and open-request flags.
//!
//! Values `0` through `-11` are fixed by the ring protocol — and by the
//! injected shim DLL, which matches statuses by number across the process
//! boundary — and must not be renumbered. New statuses append at the next
//! free negative number instead. Statuses stay plain `i32`, so a `Provider`
//! implemented outside this workspace needs no wrapper type.
//!
//! The helper functions below name each status, and [`from_io`] and
//! [`lock_or_status`] cover the two ways a provider usually produces one.

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
/// The answer does not fit an inline ring reply (`payload_cap - 8` bytes).
/// Used by the registry `REG_KEY` op for a key whose overlay data exceeds the
/// ring's payload; the shim treats it like a failed director read and serves
/// the real key.
pub const ST_REPLY_TOO_LARGE: i32 = -11;

use std::io;
use std::sync::{Mutex, MutexGuard};

pub fn not_found() -> i32 {
    ST_NOT_FOUND
}
pub fn not_a_dir() -> i32 {
    ST_NOT_A_DIRECTORY
}
pub fn bad_request() -> i32 {
    ST_BAD_REQUEST
}
pub fn map_io_err() -> i32 {
    ST_IO_ERROR
}
pub fn is_dir() -> i32 {
    ST_IS_DIR
}
pub fn bad_fh() -> i32 {
    ST_BAD_FH
}
pub fn not_supported() -> i32 {
    ST_NOT_SUPPORTED
}
pub fn read_only() -> i32 {
    ST_READ_ONLY
}
pub fn exists() -> i32 {
    ST_EXISTS
}
pub fn no_space() -> i32 {
    ST_NO_SPACE
}
pub fn reply_too_large() -> i32 {
    ST_REPLY_TOO_LARGE
}

/// The status for a failure that has no better description: `ST_IO_ERROR`.
/// Same value as [`map_io_err`], which is kept under its old name.
pub fn io_error() -> i32 {
    ST_IO_ERROR
}

/// Map a `std::io::Error` to the closest status. `NotFound`, `AlreadyExists`,
/// `IsADirectory`, `NotADirectory` and `StorageFull` get their own status;
/// everything else is `ST_IO_ERROR`.
pub fn from_io(e: &io::Error) -> i32 {
    match e.kind() {
        io::ErrorKind::NotFound => ST_NOT_FOUND,
        io::ErrorKind::AlreadyExists => ST_EXISTS,
        io::ErrorKind::IsADirectory => ST_IS_DIR,
        io::ErrorKind::NotADirectory => ST_NOT_A_DIRECTORY,
        io::ErrorKind::StorageFull => ST_NO_SPACE,
        _ => ST_IO_ERROR,
    }
}

/// Lock `m`, answering `ST_IO_ERROR` if a panicking holder poisoned it.
pub fn lock_or_status<T>(m: &Mutex<T>) -> Result<MutexGuard<'_, T>, i32> {
    m.lock().map_err(|_| ST_IO_ERROR)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_io_maps_kinds() {
        let k = |kind| from_io(&io::Error::from(kind));
        assert_eq!(k(io::ErrorKind::NotFound), ST_NOT_FOUND);
        assert_eq!(k(io::ErrorKind::AlreadyExists), ST_EXISTS);
        assert_eq!(k(io::ErrorKind::IsADirectory), ST_IS_DIR);
        assert_eq!(k(io::ErrorKind::NotADirectory), ST_NOT_A_DIRECTORY);
        assert_eq!(k(io::ErrorKind::StorageFull), ST_NO_SPACE);
        assert_eq!(k(io::ErrorKind::PermissionDenied), ST_IO_ERROR);
    }

    #[test]
    fn named_helpers_match_constants() {
        assert_eq!(no_space(), ST_NO_SPACE);
        assert_eq!(reply_too_large(), ST_REPLY_TOO_LARGE);
        assert_eq!(io_error(), map_io_err());
    }

    #[test]
    fn lock_or_status_reports_poison() {
        let m = Mutex::new(1);
        assert_eq!(*lock_or_status(&m).unwrap(), 1);
        let _ = std::panic::catch_unwind(|| {
            let _g = m.lock().unwrap();
            panic!("poison");
        });
        assert_eq!(lock_or_status(&m).err(), Some(ST_IO_ERROR));
    }
}

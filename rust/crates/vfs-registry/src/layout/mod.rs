//! NT information-class layouts for `NtQueryKey`, `NtEnumerateKey`, `NtQueryValueKey`,
//! `NtEnumerateValueKey` and `NtQueryMultipleValueKey`, written as explicit little-endian
//! bytes at the documented offsets (x64 layouts).
//!
//! Buffer rules follow the NT configuration manager (WRK `CmpQueryKeyData`,
//! `CmpQueryKeyDataFromCache`, `CmpQueryKeyValueData`, `CmQueryKey`, `CmQueryMultipleValueKey`):
//! - `result_length` is always the full size;
//! - a buffer shorter than the fixed part gives `STATUS_BUFFER_TOO_SMALL` and nothing is written;
//! - otherwise the fixed part is written with the full lengths, as much of each variable part as
//!   fits is copied, and a short buffer gives `STATUS_BUFFER_OVERFLOW`;
//! - bytes the structure does not define (alignment padding) are left as the caller had them.

mod key;
#[cfg(test)]
mod tests;
mod value;

pub use key::{write_key_info, write_subkey_info};
pub use value::{KEY_VALUE_ENTRY_SIZE, write_multiple_values, write_value_info};

pub const STATUS_SUCCESS: i32 = 0;
pub const STATUS_BUFFER_OVERFLOW: i32 = 0x8000_0005_u32 as i32;
pub const STATUS_INVALID_PARAMETER: i32 = 0xC000_000D_u32 as i32;
pub const STATUS_BUFFER_TOO_SMALL: i32 = 0xC000_0023_u32 as i32;
pub const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034_u32 as i32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum KeyInfoClass {
    Basic = 0,
    Node = 1,
    Full = 2,
    Name = 3,
    Cached = 4,
    Flags = 5,
    Virtualization = 6,
    HandleTags = 7,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ValueInfoClass {
    Basic = 0,
    Full = 1,
    Partial = 2,
    FullAlign64 = 3,
    PartialAlign64 = 4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    pub status: i32,
    pub result_length: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ValueEntry {
    pub data_length: u32,
    pub data_offset: u32,
    pub ty: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MultipleWritten {
    pub status: i32,
    pub buffer_length: u32,
    pub result_length: u32,
}

/// Little-endian writes at fixed offsets. Callers only write the fixed part after checking the
/// buffer holds it; `tail` copies as much of a variable part as fits.
struct Out<'a>(&'a mut [u8]);

impl Out<'_> {
    fn u32(&mut self, off: usize, v: u32) {
        self.0[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, off: usize, v: u64) {
        self.0[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }
    /// Copy `bytes` at `off`, truncated to the buffer. Returns whether it all fit.
    fn tail(&mut self, off: usize, bytes: &[u8]) -> bool {
        let room = self.0.len().saturating_sub(off);
        let k = bytes.len().min(room);
        if k > 0 {
            self.0[off..off + k].copy_from_slice(&bytes[..k]);
        }
        k == bytes.len()
    }
}

fn len32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

fn utf16_bytes(s: &str) -> u32 {
    len32(s.encode_utf16().count() * 2)
}

fn align(n: usize, to: usize) -> usize {
    n.div_ceil(to) * to
}

fn status(fit: bool) -> i32 {
    if fit {
        STATUS_SUCCESS
    } else {
        STATUS_BUFFER_OVERFLOW
    }
}

fn too_small(result_length: usize) -> Written {
    Written {
        status: STATUS_BUFFER_TOO_SMALL,
        result_length: len32(result_length),
    }
}

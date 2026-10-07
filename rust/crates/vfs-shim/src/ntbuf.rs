//! Raw NT buffers the hooks read and write: `UNICODE_STRING` decoding, an owned
//! `OBJECT_ATTRIBUTES` for the shim's own opens, and `IO_STATUS_BLOCK` writes.
//!
//! One place for the rules, so the hooks cannot drift apart on them.
//!
//! **`UNICODE_STRING` rule** (NT's capture rule, decided in `vfs_redirect::counted_units`):
//! - a NULL pointer is "no string" (`Ok(None)`);
//! - an odd `Length` is `STATUS_OBJECT_NAME_INVALID`, never rounded down;
//! - `Length == 0` is the empty string, whether or not `Buffer` is NULL;
//! - a NULL `Buffer` with `Length != 0` is `STATUS_ACCESS_VIOLATION`.
//!
//! What a caller does with an `Err` is its own policy: a hook that only wants to decide whether
//! the name is ours treats it as "undecodable" and lets the real call judge it; a hook that
//! answers the call itself returns the status.
#![allow(unsafe_code)]

use core::ffi::c_void;

use windows_sys::Win32::Foundation::NTSTATUS;

use crate::ntdef::{
    ObjectAttributes, UnicodeString, OBJ_CASE_INSENSITIVE, STATUS_ACCESS_VIOLATION,
    STATUS_OBJECT_NAME_INVALID,
};
use vfs_redirect::{counted_units, utf16_to_string, CountedErr};

/// The status NT gives for a `UNICODE_STRING` header that fails the rule above.
fn counted_status(e: CountedErr) -> NTSTATUS {
    match e {
        CountedErr::OddLength => STATUS_OBJECT_NAME_INVALID,
        CountedErr::NullBuffer => STATUS_ACCESS_VIOLATION,
    }
}

/// The caller's `UNICODE_STRING` as UTF-16 units: `Ok(None)` for a NULL pointer.
///
/// # Safety
/// `us` is NULL or points to a readable `UNICODE_STRING` whose `Buffer`, if non-NULL, holds
/// `Length` readable bytes that stay valid for `'a`.
pub unsafe fn us_units<'a>(us: *const UnicodeString) -> Result<Option<&'a [u16]>, NTSTATUS> {
    if us.is_null() {
        return Ok(None);
    }
    let us = &*us;
    match counted_units(us.length, us.buffer.is_null()) {
        Ok(0) => Ok(Some(&[])),
        Ok(n) => Ok(Some(core::slice::from_raw_parts(us.buffer, n))),
        Err(e) => Err(counted_status(e)),
    }
}

/// [`us_units`], decoded lossily to a `String`.
///
/// # Safety
/// As [`us_units`].
pub unsafe fn us_string(us: *const UnicodeString) -> Result<Option<String>, NTSTATUS> {
    Ok(us_units(us)?.map(utf16_to_string))
}

/// The `ObjectName` of an `OBJECT_ATTRIBUTES`, ignoring `RootDirectory`: `Ok(None)` for a NULL
/// `oa` or a NULL `ObjectName`.
///
/// # Safety
/// `oa` is NULL or points to a readable `OBJECT_ATTRIBUTES`; its `ObjectName` is as for
/// [`us_units`].
pub unsafe fn oa_name_string(oa: *const ObjectAttributes) -> Result<Option<String>, NTSTATUS> {
    if oa.is_null() {
        return Ok(None);
    }
    us_string((*oa).object_name)
}

/// An absolute `OBJECT_ATTRIBUTES` the shim builds for its own opens. Boxed: the attributes
/// point at the string, which points at the buffer.
///
/// The name is NUL-terminated in the buffer, with `Length` excluding the NUL and `MaximumLength`
/// including it, as NT builds its own.
pub struct OwnedOa {
    buf: Vec<u16>,
    us: UnicodeString,
    oa: ObjectAttributes,
}

impl OwnedOa {
    /// `nt` (an absolute NT name) with a NULL `RootDirectory`. With a `template`, its length,
    /// attributes, security descriptor and QoS are carried over; without one the attributes are
    /// `OBJ_CASE_INSENSITIVE` alone. `OBJ_CASE_INSENSITIVE` is added either way when
    /// `case_insensitive` is set.
    ///
    /// A name too long for a `UNICODE_STRING` is cut at the longest even length that fits.
    pub fn absolute(
        template: Option<&ObjectAttributes>,
        nt: &str,
        case_insensitive: bool,
    ) -> Box<OwnedOa> {
        let mut buf: Vec<u16> = nt.encode_utf16().collect();
        let max_bytes = (u16::MAX & !1) as usize;
        buf.truncate(max_bytes / 2);
        let bytes = (buf.len() * 2) as u16;
        buf.push(0);
        let mut b = Box::new(OwnedOa {
            buf,
            us: UnicodeString {
                length: bytes,
                maximum_length: (bytes as usize + 2).min(max_bytes) as u16,
                buffer: core::ptr::null_mut(),
            },
            oa: ObjectAttributes {
                length: core::mem::size_of::<ObjectAttributes>() as u32,
                root_directory: core::ptr::null_mut(),
                object_name: core::ptr::null(),
                attributes: if template.is_none() {
                    OBJ_CASE_INSENSITIVE
                } else {
                    0
                },
                security_descriptor: core::ptr::null(),
                security_qos: core::ptr::null(),
            },
        });
        b.us.buffer = b.buf.as_mut_ptr();
        b.oa.object_name = &b.us;
        if let Some(t) = template {
            b.oa.length = t.length;
            b.oa.attributes = t.attributes;
            b.oa.security_descriptor = t.security_descriptor;
            b.oa.security_qos = t.security_qos;
        }
        if case_insensitive {
            b.oa.attributes |= OBJ_CASE_INSENSITIVE;
        }
        b
    }

    /// The attributes, valid while `self` is alive.
    pub fn as_ptr(&self) -> *const ObjectAttributes {
        &self.oa
    }
}

/// Write `IO_STATUS_BLOCK { Status, Information }` at `iosb`; a NULL `iosb` is a no-op.
///
/// The fields are written unaligned: the hooks take the block as an opaque `*mut c_void`
/// and a caller's block is not ours to assume aligned.
///
/// # Safety
/// `iosb` is NULL or points to 16 writable bytes (an `IO_STATUS_BLOCK`).
pub unsafe fn iosb_set(iosb: *mut c_void, status: NTSTATUS, info: usize) {
    if iosb.is_null() {
        return;
    }
    let p = iosb as *mut u8;
    core::ptr::write_unaligned(p as *mut u32, status as u32);
    core::ptr::write_unaligned(p.add(8) as *mut usize, info);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn us_of(units: &mut [u16], length: u16) -> UnicodeString {
        UnicodeString {
            length,
            maximum_length: length,
            buffer: units.as_mut_ptr(),
        }
    }

    #[test]
    fn null_pointer_is_no_string() {
        unsafe {
            assert_eq!(us_units(core::ptr::null()), Ok(None));
            assert_eq!(us_string(core::ptr::null()), Ok(None));
            assert_eq!(oa_name_string(core::ptr::null()), Ok(None));
        }
    }

    #[test]
    fn even_length_decodes() {
        let mut w: Vec<u16> = "abc".encode_utf16().collect();
        let us = us_of(&mut w, 6);
        assert_eq!(unsafe { us_string(&us) }, Ok(Some("abc".to_string())));
        // Length shorter than the buffer is honoured.
        let us = us_of(&mut w, 4);
        assert_eq!(unsafe { us_string(&us) }, Ok(Some("ab".to_string())));
    }

    #[test]
    fn odd_length_is_name_invalid_not_truncated() {
        let mut w: Vec<u16> = "abc".encode_utf16().collect();
        let us = us_of(&mut w, 5);
        assert_eq!(unsafe { us_units(&us) }, Err(STATUS_OBJECT_NAME_INVALID));
        assert_eq!(unsafe { us_string(&us) }, Err(STATUS_OBJECT_NAME_INVALID));
    }

    #[test]
    fn zero_length_is_empty_even_with_a_null_buffer() {
        let us = UnicodeString {
            length: 0,
            maximum_length: 0,
            buffer: core::ptr::null_mut(),
        };
        assert_eq!(unsafe { us_string(&us) }, Ok(Some(String::new())));
        let mut w = [0u16; 1];
        let us = us_of(&mut w, 0);
        assert_eq!(unsafe { us_string(&us) }, Ok(Some(String::new())));
    }

    #[test]
    fn null_buffer_with_length_is_access_violation() {
        let us = UnicodeString {
            length: 4,
            maximum_length: 4,
            buffer: core::ptr::null_mut(),
        };
        assert_eq!(unsafe { us_units(&us) }, Err(STATUS_ACCESS_VIOLATION));
    }

    #[test]
    fn oa_name_reads_the_object_name_only() {
        let mut w: Vec<u16> = "x\\y".encode_utf16().collect();
        let us = us_of(&mut w, 6);
        let oa = ObjectAttributes {
            length: core::mem::size_of::<ObjectAttributes>() as u32,
            root_directory: 0x1234 as _,
            object_name: &us,
            attributes: 0,
            security_descriptor: core::ptr::null(),
            security_qos: core::ptr::null(),
        };
        assert_eq!(unsafe { oa_name_string(&oa) }, Ok(Some("x\\y".to_string())));
        let none = ObjectAttributes {
            object_name: core::ptr::null(),
            ..oa
        };
        assert_eq!(unsafe { oa_name_string(&none) }, Ok(None));
    }

    #[test]
    fn owned_oa_is_absolute_and_nul_terminated() {
        let tmpl = ObjectAttributes {
            length: core::mem::size_of::<ObjectAttributes>() as u32,
            root_directory: 0x77 as _,
            object_name: core::ptr::null(),
            attributes: 0x2,
            security_descriptor: 0x10 as _,
            security_qos: 0x20 as _,
        };
        let name = r"\??\C:\a\b";
        let o = OwnedOa::absolute(Some(&tmpl), name, false);
        let oa = unsafe { &*o.as_ptr() };
        assert!(oa.root_directory.is_null());
        assert_eq!(oa.attributes, 0x2);
        assert_eq!(oa.security_descriptor, tmpl.security_descriptor);
        assert_eq!(oa.security_qos, tmpl.security_qos);
        let us = unsafe { &*oa.object_name };
        assert_eq!(us.length as usize, name.len() * 2);
        assert_eq!(us.maximum_length, us.length + 2);
        let units = unsafe { core::slice::from_raw_parts(us.buffer, name.len() + 1) };
        assert_eq!(units[name.len()], 0);
        assert_eq!(unsafe { oa_name_string(oa) }, Ok(Some(name.to_string())));
        // No template: case-insensitive by default; with a template the flag is added on request.
        let o = OwnedOa::absolute(None, name, false);
        assert_eq!(unsafe { (*o.as_ptr()).attributes }, OBJ_CASE_INSENSITIVE);
        let o = OwnedOa::absolute(Some(&tmpl), name, true);
        assert_eq!(
            unsafe { (*o.as_ptr()).attributes },
            0x2 | OBJ_CASE_INSENSITIVE
        );
    }

    #[test]
    fn owned_oa_cuts_an_overlong_name_at_an_even_length() {
        let name = "a".repeat(40_000);
        let o = OwnedOa::absolute(None, &name, true);
        let us = unsafe { &*(*o.as_ptr()).object_name };
        assert_eq!(us.length, 0xFFFE);
        assert_eq!(us.maximum_length, 0xFFFE);
    }

    #[test]
    fn iosb_set_writes_status_and_information() {
        let mut block = [0xAAu8; 24];
        unsafe { iosb_set(block.as_mut_ptr().cast(), -1073741823, 0x1234) };
        assert_eq!(&block[0..4], &(-1073741823i32).to_ne_bytes());
        assert_eq!(&block[8..16], &0x1234usize.to_ne_bytes());
        // Bytes outside the block are untouched.
        assert_eq!(&block[4..8], &[0xAA; 4]);
        assert_eq!(&block[16..], &[0xAA; 8]);
        unsafe { iosb_set(core::ptr::null_mut(), 0, 0) };
    }
}

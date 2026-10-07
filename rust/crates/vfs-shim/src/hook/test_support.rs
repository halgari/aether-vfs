//! Helpers shared by the unit tests of several hook modules.

use crate::ntdef::{ObjectAttributes, UnicodeString};

pub(super) fn us_raw(length: u16, buffer: *mut u16) -> UnicodeString {
    UnicodeString {
        length,
        maximum_length: length,
        buffer,
    }
}

pub(super) fn oa_named(us: &UnicodeString) -> ObjectAttributes {
    ObjectAttributes {
        length: core::mem::size_of::<ObjectAttributes>() as u32,
        root_directory: core::ptr::null_mut(),
        object_name: us,
        attributes: 0,
        security_descriptor: core::ptr::null(),
        security_qos: core::ptr::null(),
    }
}

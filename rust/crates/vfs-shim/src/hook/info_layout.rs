//! The shim's end of the file-information layouts: the writers are `vfs_ntlayout`'s, and what stays
//! here is turning the caller's raw pointer into the slice they write.
#![deny(unsafe_op_in_unsafe_fn)]

use core::ffi::c_void;

pub(super) use vfs_ntlayout::{
    attributes, put_all_prefix, put_attribute_tag, put_basic, put_file_name, put_id,
    put_network_open, put_object_name, put_standard, put_stat, Fit, ALL_PREFIX_LEN,
    ATTRIBUTE_TAG_LEN, BASIC_LEN, ID_LEN, NETWORK_OPEN_LEN, STANDARD_LEN, STAT_LEN,
};

/// The `len` bytes at `p` the caller handed an NT call, as a slice; empty for a NULL `p`.
///
/// # Safety
/// `p` is NULL or valid for `len` bytes of writes, for as long as the slice is used (the NT
/// caller's contract for its buffer, hook/mod.rs).
pub(super) unsafe fn caller_buf<'a>(p: *mut c_void, len: usize) -> &'a mut [u8] {
    if p.is_null() {
        return &mut [];
    }
    // SAFETY: as this function's contract says.
    unsafe { core::slice::from_raw_parts_mut(p as *mut u8, len) }
}

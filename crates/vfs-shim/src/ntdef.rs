//! Minimal `#[repr(C)]` NT type definitions used by the NtCreateFile hook.
//! No `unsafe` here — just layout-compatible structs and the fn signature.

use core::ffi::c_void;
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};

/// `STATUS_UNSUCCESSFUL` — returned only if the trampoline is somehow unset
/// (an invariant violation the hook must not panic on).
pub const STATUS_UNSUCCESSFUL: NTSTATUS = 0xC000_0001u32 as i32;

/// Layout-compatible with the NT `UNICODE_STRING`. `length`/`maximum_length`
/// are in BYTES; the u16 count is `length / 2`.
#[repr(C)]
pub struct UnicodeString {
    pub length: u16,
    pub maximum_length: u16,
    pub buffer: *mut u16,
}

/// Layout-compatible with the NT `OBJECT_ATTRIBUTES`.
#[repr(C)]
pub struct ObjectAttributes {
    pub length: u32,
    pub root_directory: HANDLE,
    pub object_name: *const UnicodeString,
    pub attributes: u32,
    pub security_descriptor: *const c_void,
    pub security_qos: *const c_void,
}

/// The `ntdll!NtCreateFile` signature. `IO_STATUS_BLOCK` is left opaque
/// (`*mut c_void`) — the hook never inspects it.
pub type NtCreateFileFn = unsafe extern "system" fn(
    *mut HANDLE, // FileHandle
    u32,         // DesiredAccess
    *const ObjectAttributes,
    *mut c_void, // IoStatusBlock
    *const i64,  // AllocationSize
    u32,         // FileAttributes
    u32,         // ShareAccess
    u32,         // CreateDisposition
    u32,         // CreateOptions
    *const c_void, // EaBuffer
    u32,         // EaLength
) -> NTSTATUS;

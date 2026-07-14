//! Minimal `#[repr(C)]` NT type definitions used by the NtCreateFile hook.
//! No `unsafe` here — just layout-compatible structs and the fn signature.

use core::ffi::c_void;
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};

/// `STATUS_UNSUCCESSFUL` — returned only if the trampoline is somehow unset
/// (an invariant violation the hook must not panic on).
pub const STATUS_UNSUCCESSFUL: NTSTATUS = 0xC000_0001u32 as i32;

/// `STATUS_OBJECT_NAME_NOT_FOUND` — returned for a tombstoned (mod-deleted) path
/// so the real on-disk file appears absent.
pub const STATUS_OBJECT_NAME_NOT_FOUND: NTSTATUS = 0xC000_0034u32 as i32;

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

/// `STATUS_SUCCESS`.
pub const STATUS_SUCCESS: NTSTATUS = 0;
/// `FILE_ATTRIBUTE_DIRECTORY` / `FILE_ATTRIBUTE_NORMAL`.
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

/// Layout-compatible with `FILE_BASIC_INFORMATION` (40 bytes).
#[repr(C)]
pub struct FileBasicInformation {
    pub creation_time: i64,
    pub last_access_time: i64,
    pub last_write_time: i64,
    pub change_time: i64,
    pub file_attributes: u32,
    pub _reserved: u32,
}

/// Layout-compatible with `FILE_NETWORK_OPEN_INFORMATION` (56 bytes).
#[repr(C)]
pub struct FileNetworkOpenInformation {
    pub creation_time: i64,
    pub last_access_time: i64,
    pub last_write_time: i64,
    pub change_time: i64,
    pub allocation_size: i64,
    pub end_of_file: i64,
    pub file_attributes: u32,
    pub _reserved: u32,
}

pub type NtQueryAttributesFileFn =
    unsafe extern "system" fn(*const ObjectAttributes, *mut FileBasicInformation) -> NTSTATUS;
pub type NtQueryFullAttributesFileFn =
    unsafe extern "system" fn(*const ObjectAttributes, *mut FileNetworkOpenInformation) -> NTSTATUS;

/// `ntdll!NtQueryDirectoryFileEx`. `FileName` is a `PUNICODE_STRING` (nullable);
/// `IoStatusBlock` and `FileInformation` are left opaque and touched by the hook
/// via raw offsets. `ApcRoutine`/`ApcContext`/`Event` are unused by our callers.
pub type NtQueryDirectoryFileExFn = unsafe extern "system" fn(
    HANDLE,               // FileHandle
    HANDLE,               // Event
    *const c_void,        // ApcRoutine
    *const c_void,        // ApcContext
    *mut c_void,          // IoStatusBlock
    *mut c_void,          // FileInformation
    u32,                  // Length
    u32,                  // FileInformationClass
    u32,                  // QueryFlags
    *const UnicodeString, // FileName
) -> NTSTATUS;

/// `ntdll!NtOpenFile` — the open path many callers (incl. Rust `std`'s
/// directory open) use instead of `NtCreateFile`.
pub type NtOpenFileFn = unsafe extern "system" fn(
    *mut HANDLE, // FileHandle
    u32,         // DesiredAccess
    *const ObjectAttributes,
    *mut c_void, // IoStatusBlock
    u32,         // ShareAccess
    u32,         // OpenOptions
) -> NTSTATUS;

/// `ntdll!NtClose`.
pub type NtCloseFn = unsafe extern "system" fn(HANDLE) -> NTSTATUS;

/// `ntdll!NtQueryInformationFile`.
pub type NtQueryInformationFileFn = unsafe extern "system" fn(
    HANDLE,      // FileHandle
    *mut c_void, // IoStatusBlock
    *mut c_void, // FileInformation
    u32,         // Length
    u32,         // FileInformationClass
) -> NTSTATUS;

/// `FileNormalizedNameInformation` — the class `GetFinalPathNameByHandleW` uses
/// as the authoritative path (spoof this; NOT class 9, which would break it).
pub const FILE_NORMALIZED_NAME_INFORMATION: u32 = 48;

/// `ntdll!NtSetInformationFile` (same ABI as NtQueryInformationFile).
pub type NtSetInformationFileFn = unsafe extern "system" fn(
    HANDLE,      // FileHandle
    *mut c_void, // IoStatusBlock
    *mut c_void, // FileInformation
    u32,         // Length
    u32,         // FileInformationClass
) -> NTSTATUS;

/// `FileDispositionInformation` (class 13): 1-byte BOOLEAN `DeleteFile`.
pub const FILE_DISPOSITION_INFORMATION: u32 = 13;
/// `FileDispositionInformationEx` (class 64): ULONG `Flags`; bit 0 = DELETE.
pub const FILE_DISPOSITION_INFORMATION_EX: u32 = 64;
/// `FILE_DISPOSITION_DELETE` flag for the Ex form.
pub const FILE_DISPOSITION_DELETE: u32 = 0x1;

/// `FileRenameInformation` (class 10) / `FileRenameInformationEx` (class 65).
/// Layout (x64): `[0] ReplaceIfExists/Flags`, `[8] RootDirectory (HANDLE)`,
/// `[16] FileNameLength (ULONG)`, `[20] FileName (WCHAR[])`.
pub const FILE_RENAME_INFORMATION: u32 = 10;
pub const FILE_RENAME_INFORMATION_EX: u32 = 65;

/// `NtQueryDirectoryFileEx` QueryFlags.
pub const SL_RESTART_SCAN: u32 = 0x01;
pub const SL_RETURN_SINGLE_ENTRY: u32 = 0x02;

/// `STATUS_NO_MORE_FILES` — enumeration cursor exhausted.
pub const STATUS_NO_MORE_FILES: NTSTATUS = 0x8000_0006u32 as i32;
/// `STATUS_BUFFER_OVERFLOW` — the caller buffer cannot hold even one entry.
pub const STATUS_BUFFER_OVERFLOW: NTSTATUS = 0x8000_0005u32 as i32;

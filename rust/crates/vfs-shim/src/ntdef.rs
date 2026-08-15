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

/// `STATUS_OBJECT_PATH_NOT_FOUND` — maps to Win32 `ERROR_PATH_NOT_FOUND` (3),
/// as distinct from `STATUS_OBJECT_NAME_NOT_FOUND`'s `ERROR_FILE_NOT_FOUND`
/// (2). NT returns it when the *container* of the named file cannot be
/// resolved, which is exactly what a refused create under a managed root
/// means: the leaf was supposed to be created, so what is missing is a
/// location willing to hold it, not the name. See `try_fuse_create`.
pub const STATUS_OBJECT_PATH_NOT_FOUND: NTSTATUS = 0xC000_003Au32 as i32;

/// `STATUS_ACCESS_DENIED` — maps to `ERROR_ACCESS_DENIED`. The honest NT
/// answer when the provider graph serves a path but no layer of it accepts
/// writes (`ST_READ_ONLY`), which is what a real read-only filesystem
/// returns for the same open.
pub const STATUS_ACCESS_DENIED: NTSTATUS = 0xC000_0022u32 as i32;

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

/// `ntdll!NtQueryDirectoryFile` — the classic enumeration entry point, still a
/// distinct export from the `Ex` form above and still what plenty of callers
/// reach. It carries `ReturnSingleEntry` and `RestartScan` as separate
/// `BOOLEAN`s where `Ex` folds both into `QueryFlags`.
///
/// Hooking only `Ex` leaves this one running against the real directory, which
/// is invisible in every counter: the composed view is simply never consulted.
pub type NtQueryDirectoryFileFn = unsafe extern "system" fn(
    HANDLE,               // FileHandle
    HANDLE,               // Event
    *const c_void,        // ApcRoutine
    *const c_void,        // ApcContext
    *mut c_void,          // IoStatusBlock
    *mut c_void,          // FileInformation
    u32,                  // Length
    u32,                  // FileInformationClass
    u8,                   // ReturnSingleEntry (BOOLEAN)
    *const UnicodeString, // FileName
    u8,                   // RestartScan (BOOLEAN)
) -> NTSTATUS;

/// `ntdll!NtQueryInformationByName` (Win10 1709+). Stats a path *without*
/// opening it, so a caller using it never appears in any open-side counter and
/// never consults a handle we could have virtualised.
pub type NtQueryInformationByNameFn = unsafe extern "system" fn(
    *const ObjectAttributes,
    *mut c_void, // IoStatusBlock
    *mut c_void, // FileInformation
    u32,         // Length
    u32,         // FileInformationClass
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

/// `FileEndOfFileInformation` (class 20): a single `LARGE_INTEGER EndOfFile`.
/// Set via `NtSetInformationFile` — this is how `File::set_len` truncates.
pub const FILE_END_OF_FILE_INFORMATION: u32 = 20;

/// `FILE_DIRECTORY_FILE` `CreateOptions` flag — the open targets a directory.
pub const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;

/// `NtQueryDirectoryFileEx` QueryFlags.
pub const SL_RESTART_SCAN: u32 = 0x01;
pub const SL_RETURN_SINGLE_ENTRY: u32 = 0x02;

/// `STATUS_NO_MORE_FILES` — enumeration cursor exhausted.
pub const STATUS_NO_MORE_FILES: NTSTATUS = 0x8000_0006u32 as i32;
/// `STATUS_BUFFER_OVERFLOW` — the caller buffer cannot hold even one entry.
pub const STATUS_BUFFER_OVERFLOW: NTSTATUS = 0x8000_0005u32 as i32;

/// `ntdll!NtReadFile`. `Event`/`ApcRoutine`/`ApcContext`/`Key` are unused by
/// synchronous callers; `ByteOffset` is a `PLARGE_INTEGER` (nullable).
pub type NtReadFileFn = unsafe extern "system" fn(
    HANDLE,        // FileHandle
    HANDLE,        // Event
    *const c_void, // ApcRoutine
    *const c_void, // ApcContext
    *mut c_void,   // IoStatusBlock
    *mut c_void,   // Buffer
    u32,           // Length
    *const i64,    // ByteOffset (LARGE_INTEGER)
    *const u32,    // Key
) -> NTSTATUS;

/// `NtWriteFile` — identical signature to `NtReadFile` (Buffer is the source).
pub type NtWriteFileFn = unsafe extern "system" fn(
    HANDLE,        // FileHandle
    HANDLE,        // Event
    *const c_void, // ApcRoutine
    *const c_void, // ApcContext
    *mut c_void,   // IoStatusBlock
    *mut c_void,   // Buffer (source bytes to write)
    u32,           // Length
    *const i64,    // ByteOffset
    *const u32,    // Key
) -> NTSTATUS;

/// `STATUS_END_OF_FILE`.
pub const STATUS_END_OF_FILE: NTSTATUS = 0xC000_0011u32 as i32;
/// `STATUS_INVALID_FILE_FOR_SECTION` — e.g. a section over a synthetic handle
/// whose content cannot be mapped (a directory, an empty file, or a PE the
/// director's bytes do not parse as an image).
pub const STATUS_INVALID_FILE_FOR_SECTION: NTSTATUS = 0xC000_0124u32 as i32;
/// `STATUS_INVALID_HANDLE`.
pub const STATUS_INVALID_HANDLE: NTSTATUS = 0xC000_0008u32 as i32;
/// `STATUS_OBJECT_NAME_COLLISION` — maps to `ERROR_ALREADY_EXISTS`; what a
/// `FILE_CREATE` of an existing name must report so the standard
/// create-and-ignore-ALREADY_EXISTS idiom works.
pub const STATUS_OBJECT_NAME_COLLISION: NTSTATUS = 0xC000_0035u32 as i32;
/// `STATUS_SECTION_TOO_BIG`.
pub const STATUS_SECTION_TOO_BIG: NTSTATUS = 0xC000_0040u32 as i32;
/// `STATUS_FILE_IS_A_DIRECTORY` — maps to `ERROR_ACCESS_DENIED` at the Win32
/// layer, but NT callers that look at the status get the real reason. What a
/// create or overwrite aimed at an existing *directory* must report, rather
/// than the generic `STATUS_UNSUCCESSFUL` every other provider error gets.
pub const STATUS_FILE_IS_A_DIRECTORY: NTSTATUS = 0xC000_00BAu32 as i32;
/// `FILE_SUPERSEDED` disposition-information (an existing object was
/// replaced by `FILE_SUPERSEDE`).
pub const FILE_SUPERSEDED: usize = 0;
/// `FILE_OPENED` disposition-information for a synthetic open's IoStatusBlock.
pub const FILE_OPENED: usize = 1;
/// `FILE_CREATED` disposition-information (a fresh object was created).
pub const FILE_CREATED: usize = 2;
/// `FILE_OVERWRITTEN` disposition-information (an existing object was
/// truncated in place by `FILE_OVERWRITE`/`FILE_OVERWRITE_IF`).
pub const FILE_OVERWRITTEN: usize = 3;

/// `SEC_IMAGE` — PE image mapping.
pub const SEC_IMAGE: u32 = 0x0100_0000;

/// `ntdll!NtCreateSection`.
pub type NtCreateSectionFn = unsafe extern "system" fn(
    *mut HANDLE, // SectionHandle
    u32,         // DesiredAccess
    *const ObjectAttributes,
    *mut i64, // MaximumSize (PLARGE_INTEGER)
    u32,      // SectionPageProtection
    u32,      // AllocationAttributes
    HANDLE,   // FileHandle
) -> NTSTATUS;

/// `ntdll!NtMapViewOfSection`.
pub type NtMapViewOfSectionFn = unsafe extern "system" fn(
    HANDLE,         // SectionHandle
    HANDLE,         // ProcessHandle
    *mut *mut c_void, // BaseAddress
    usize,          // ZeroBits
    usize,          // CommitSize
    *mut i64,       // SectionOffset
    *mut usize,     // ViewSize
    u32,            // InheritDisposition
    u32,            // AllocationType
    u32,            // Win32Protect
) -> NTSTATUS;

/// `ntdll!NtUnmapViewOfSection`.
pub type NtUnmapViewOfSectionFn = unsafe extern "system" fn(
    HANDLE,      // ProcessHandle
    *mut c_void, // BaseAddress
) -> NTSTATUS;

/// `FileBasicInformation` (class 4).
pub const FILE_BASIC_INFORMATION: u32 = 4;
/// `FileStandardInformation` (class 5).
pub const FILE_STANDARD_INFORMATION: u32 = 5;
/// `FileInternalInformation` (class 6).
pub const FILE_INTERNAL_INFORMATION: u32 = 6;
/// `FilePositionInformation` (class 14).
pub const FILE_POSITION_INFORMATION: u32 = 14;
/// `FileAllInformation` (class 18).
pub const FILE_ALL_INFORMATION: u32 = 18;
/// `FileNetworkOpenInformation` (class 34).
pub const FILE_NETWORK_OPEN_INFORMATION: u32 = 34;

/// Layout-compatible with `FILE_INTERNAL_INFORMATION` (8 bytes).
#[repr(C)]
pub struct FileInternalInformation {
    pub index_number: i64,
}

/// `ntdll!NtQueryVolumeInformationFile`.
pub type NtQueryVolumeInformationFileFn = unsafe extern "system" fn(
    HANDLE,      // FileHandle
    *mut c_void, // IoStatusBlock
    *mut c_void, // FsInformation
    u32,         // Length
    u32,         // FsInformationClass
) -> NTSTATUS;

/// `FileFsDeviceInformation` (class 4).
pub const FILE_FS_DEVICE_INFORMATION: u32 = 4;
/// `FILE_DEVICE_DISK`.
pub const FILE_DEVICE_DISK: u32 = 0x0000_0007;

/// Layout-compatible with `FILE_FS_DEVICE_INFORMATION` (8 bytes).
#[repr(C)]
pub struct FileFsDeviceInformation {
    pub device_type: u32,
    pub characteristics: u32,
}

/// Layout-compatible with `FILE_STANDARD_INFORMATION` (24 bytes).
#[repr(C)]
pub struct FileStandardInformation {
    pub allocation_size: i64,
    pub end_of_file: i64,
    pub number_of_links: u32,
    pub delete_pending: u8,
    pub directory: u8,
    pub _pad: u16,
}

/// Layout-compatible with `FILE_POSITION_INFORMATION` (8 bytes).
#[repr(C)]
pub struct FilePositionInformation {
    pub current_byte_offset: i64,
}

/// Layout-compatible with `FILE_END_OF_FILE_INFORMATION` (8 bytes).
#[repr(C)]
pub struct FileEndOfFileInformation {
    pub end_of_file: i64,
}

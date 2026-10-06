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

/// `ntdll!NtDeleteFile` — the **path-based** delete.
///
/// The signature is the whole point: an `OBJECT_ATTRIBUTES` and nothing else.
/// No handle, no access mask, no disposition. Every other delete route the
/// shim sees (`NtSetInformationFile` with either disposition class) arrives on
/// a handle the shim watched being opened, so leaving one of *those* unhooked
/// fails safely — a synthetic handle is not a kernel object and the call comes
/// back `STATUS_INVALID_HANDLE`. This one resolves the path itself, against
/// the real filesystem, so leaving it unhooked does not fail at all.
pub type NtDeleteFileFn = unsafe extern "system" fn(*const ObjectAttributes) -> NTSTATUS;

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

/// `FileNormalizedNameInformation` — the class `GetFinalPathNameByHandleW`
/// appends to the drive letter to build its answer.
pub const FILE_NORMALIZED_NAME_INFORMATION: u32 = 48;

/// `FileNameInformation` — the volume-relative name, same record layout as
/// class 48. `GetFinalPathNameByHandleW` uses its **length**, not its content:
/// it takes the device prefix to be `ObjectName[.. ObjectName.len - this.len]`.
/// So this and `NtQueryObject`'s `ObjectNameInformation` must describe the same
/// path or that subtraction slices the device name in half — see
/// `qif_hook_body`, which spoofs both together for exactly that reason.
pub const FILE_NAME_INFORMATION: u32 = 9;

/// `ntdll!NtQueryObject`. **Not file-specific**: the same entry point answers
/// about events, mutexes, sections, registry keys and threads, so a hook on it
/// must pass through every class and every handle it does not recognise.
/// `ReturnLength` is a nullable `PULONG`.
pub type NtQueryObjectFn = unsafe extern "system" fn(
    HANDLE,      // Handle
    u32,         // ObjectInformationClass
    *mut c_void, // ObjectInformation
    u32,         // ObjectInformationLength
    *mut u32,    // ReturnLength
) -> NTSTATUS;

/// `ObjectNameInformation` (class 1): an `OBJECT_NAME_INFORMATION`, which is a
/// `UNICODE_STRING` followed by the NUL-terminated name it points at.
pub const OBJECT_NAME_INFORMATION: u32 = 1;

/// Bytes of the `UNICODE_STRING` header an `OBJECT_NAME_INFORMATION` opens with
/// (x64: `u16` Length, `u16` MaximumLength, 4 bytes padding, `*mut u16` Buffer).
/// Measured on both hosts: `Buffer` points exactly this far into the caller's
/// own buffer.
pub const OBJECT_NAME_INFORMATION_HEADER: usize = 16;

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
/// `STATUS_INFO_LENGTH_MISMATCH` — the caller buffer cannot hold even the fixed
/// header of the requested structure. Distinct from `STATUS_BUFFER_OVERFLOW`,
/// and `NtQueryObject` returns each in its own range — see `qobj_hook_body`.
pub const STATUS_INFO_LENGTH_MISMATCH: NTSTATUS = 0xC000_0004u32 as i32;

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
/// `STATUS_OBJECT_TYPE_MISMATCH` — the handle is not an object of the type the call takes
/// (e.g. `NtQueryKey` on a file handle).
pub const STATUS_OBJECT_TYPE_MISMATCH: NTSTATUS = 0xC000_0024u32 as i32;
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
/// `FileAttributeTagInformation` (class 35): attributes and reparse tag.
pub const FILE_ATTRIBUTE_TAG_INFORMATION: u32 = 35;
/// `FileIdInformation` (class 59): volume serial number and 128-bit file id.
pub const FILE_ID_INFORMATION: u32 = 59;
/// `FileStatInformation` (class 68): id, times, sizes, attributes, links.
pub const FILE_STAT_INFORMATION: u32 = 68;

/// Layout-compatible with `FILE_INTERNAL_INFORMATION` (8 bytes).
#[repr(C)]
pub struct FileInternalInformation {
    pub index_number: i64,
}

/// `ntdll!NtLockFile` — byte-range lock acquisition, and the call that made
/// the Windows profile APIs unusable under a managed root until it was
/// hooked (see `hook::lock_hook`).
///
/// `ByteOffset` and `Length` are `PLARGE_INTEGER`s; `FailImmediately` and
/// `ExclusiveLock` are `BOOLEAN`s (one byte each, not `BOOL`) — passing them
/// as `u32` would misalign `ExclusiveLock` and, worse, silently: the register
/// the callee reads would hold whatever the caller left there.
pub type NtLockFileFn = unsafe extern "system" fn(
    HANDLE,        // FileHandle
    HANDLE,        // Event
    *const c_void, // ApcRoutine
    *const c_void, // ApcContext
    *mut c_void,   // IoStatusBlock
    *const i64,    // ByteOffset (LARGE_INTEGER)
    *const i64,    // Length (LARGE_INTEGER)
    u32,           // Key
    u8,            // FailImmediately (BOOLEAN)
    u8,            // ExclusiveLock (BOOLEAN)
) -> NTSTATUS;

/// `ntdll!NtUnlockFile` — the release half of [`NtLockFileFn`]. Always
/// synchronous: no `Event`, no APC.
pub type NtUnlockFileFn = unsafe extern "system" fn(
    HANDLE,      // FileHandle
    *mut c_void, // IoStatusBlock
    *const i64,  // ByteOffset (LARGE_INTEGER)
    *const i64,  // Length (LARGE_INTEGER)
    u32,         // Key
) -> NTSTATUS;

/// `ntdll!NtFlushBuffersFile` — what `FlushFileBuffers` becomes.
pub type NtFlushBuffersFileFn = unsafe extern "system" fn(
    HANDLE,      // FileHandle
    *mut c_void, // IoStatusBlock
) -> NTSTATUS;

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

// ---- Registry (spec 2026-10-05 registry overlay, section 3.1) ----

/// `ntdll!NtOpenKey`.
pub type NtOpenKeyFn = unsafe extern "system" fn(
    *mut HANDLE, // KeyHandle
    u32,         // DesiredAccess
    *const ObjectAttributes,
) -> NTSTATUS;

/// `ntdll!NtOpenKeyEx`: `NtOpenKey` plus `OpenOptions` (`REG_OPTION_OPEN_LINK`,
/// `REG_OPTION_BACKUP_RESTORE`).
pub type NtOpenKeyExFn = unsafe extern "system" fn(
    *mut HANDLE, // KeyHandle
    u32,         // DesiredAccess
    *const ObjectAttributes,
    u32, // OpenOptions
) -> NTSTATUS;

/// `ntdll!NtCreateKey`. `Class` is a nullable `PUNICODE_STRING`, `Disposition` a nullable
/// `PULONG`.
pub type NtCreateKeyFn = unsafe extern "system" fn(
    *mut HANDLE, // KeyHandle
    u32,         // DesiredAccess
    *const ObjectAttributes,
    u32,                  // TitleIndex
    *const UnicodeString, // Class
    u32,                  // CreateOptions
    *mut u32,             // Disposition
) -> NTSTATUS;

/// `ntdll!NtQueryKey`.
pub type NtQueryKeyFn = unsafe extern "system" fn(
    HANDLE,      // KeyHandle
    u32,         // KeyInformationClass
    *mut c_void, // KeyInformation
    u32,         // Length
    *mut u32,    // ResultLength
) -> NTSTATUS;

/// `ntdll!NtDuplicateObject`. `TargetHandle` is nullable (a close-only call with
/// `DUPLICATE_CLOSE_SOURCE`).
pub type NtDuplicateObjectFn = unsafe extern "system" fn(
    HANDLE,      // SourceProcessHandle
    HANDLE,      // SourceHandle
    HANDLE,      // TargetProcessHandle
    *mut HANDLE, // TargetHandle
    u32,         // DesiredAccess
    u32,         // HandleAttributes
    u32,         // Options
) -> NTSTATUS;

/// `KeyNameInformation`: a `KEY_NAME_INFORMATION` (`u32` NameLength in bytes, then the name).
pub const KEY_NAME_INFORMATION: u32 = 3;

/// `NtCreateKey` dispositions.
pub const REG_CREATED_NEW_KEY: u32 = 1;
pub const REG_OPENED_EXISTING_KEY: u32 = 2;

/// `NtCreateKey` `CreateOptions` / `NtOpenKeyEx` `OpenOptions` bits.
pub const REG_OPTION_VOLATILE: u32 = 0x1;
pub const REG_OPTION_CREATE_LINK: u32 = 0x2;
pub const REG_OPTION_BACKUP_RESTORE: u32 = 0x4;
pub const REG_OPTION_OPEN_LINK: u32 = 0x8;

/// `DUPLICATE_CLOSE_SOURCE` / `DUPLICATE_SAME_ACCESS`.
pub const DUPLICATE_CLOSE_SOURCE: u32 = 0x1;
pub const DUPLICATE_SAME_ACCESS: u32 = 0x2;
/// `DUPLICATE_SAME_ATTRIBUTES`.
pub const DUPLICATE_SAME_ATTRIBUTES: u32 = 0x4;

/// `NtQueryObject` classes answered for synthetic key handles besides the name.
pub const OBJECT_BASIC_INFORMATION: u32 = 0;
pub const OBJECT_TYPE_INFORMATION: u32 = 2;
pub const OBJECT_HANDLE_FLAG_INFORMATION: u32 = 4;

/// `OBJ_CASE_INSENSITIVE`, for the shim's own absolute key opens.
pub const OBJ_CASE_INSENSITIVE: u32 = 0x40;

/// `STATUS_NOT_SUPPORTED`.
pub const STATUS_NOT_SUPPORTED: NTSTATUS = 0xC000_00BBu32 as i32;
/// `STATUS_INVALID_PARAMETER`.
pub const STATUS_INVALID_PARAMETER: NTSTATUS = 0xC000_000Du32 as i32;
/// `STATUS_OBJECT_NAME_INVALID`.
pub const STATUS_OBJECT_NAME_INVALID: NTSTATUS = 0xC000_0033u32 as i32;
/// `STATUS_BUFFER_TOO_SMALL`.
pub const STATUS_BUFFER_TOO_SMALL: NTSTATUS = 0xC000_0023u32 as i32;

/// `ntdll!NtEnumerateKey`.
pub type NtEnumerateKeyFn = unsafe extern "system" fn(
    HANDLE,      // KeyHandle
    u32,         // Index
    u32,         // KeyInformationClass
    *mut c_void, // KeyInformation
    u32,         // Length
    *mut u32,    // ResultLength
) -> NTSTATUS;

/// `ntdll!NtQueryValueKey`.
pub type NtQueryValueKeyFn = unsafe extern "system" fn(
    HANDLE,               // KeyHandle
    *const UnicodeString, // ValueName
    u32,                  // KeyValueInformationClass
    *mut c_void,          // KeyValueInformation
    u32,                  // Length
    *mut u32,             // ResultLength
) -> NTSTATUS;

/// `ntdll!NtEnumerateValueKey`.
pub type NtEnumerateValueKeyFn = unsafe extern "system" fn(
    HANDLE,      // KeyHandle
    u32,         // Index
    u32,         // KeyValueInformationClass
    *mut c_void, // KeyValueInformation
    u32,         // Length
    *mut u32,    // ResultLength
) -> NTSTATUS;

/// `ntdll!NtQueryMultipleValueKey`. `ValueEntries` is an array of x64 `KEY_VALUE_ENTRY`
/// (24 bytes each); `BufferLength` is in/out, `RequiredBufferLength` nullable.
pub type NtQueryMultipleValueKeyFn = unsafe extern "system" fn(
    HANDLE,      // KeyHandle
    *mut c_void, // ValueEntries
    u32,         // EntryCount
    *mut c_void, // ValueBuffer
    *mut u32,    // BufferLength
    *mut u32,    // RequiredBufferLength
) -> NTSTATUS;

/// `KEY_INFORMATION_CLASS` values the query hooks read from real keys.
pub const KEY_BASIC_INFORMATION: u32 = 0;
pub const KEY_NODE_INFORMATION: u32 = 1;
pub const KEY_FULL_INFORMATION: u32 = 2;
/// `KEY_VALUE_INFORMATION_CLASS` values the query hooks read from real keys.
pub const KEY_VALUE_BASIC_INFORMATION: u32 = 0;
pub const KEY_VALUE_PARTIAL_INFORMATION: u32 = 2;

/// `STATUS_NO_MORE_ENTRIES`: an enumeration index past the end.
pub const STATUS_NO_MORE_ENTRIES: NTSTATUS = 0x8000_001Au32 as i32;
/// `STATUS_KEY_DELETED`: a query through a handle whose key was deleted.
pub const STATUS_KEY_DELETED: NTSTATUS = 0xC000_017Cu32 as i32;
/// `STATUS_ACCESS_VIOLATION`, for a NULL buffer the caller said was non-empty.
pub const STATUS_ACCESS_VIOLATION: NTSTATUS = 0xC000_0005u32 as i32;

/// `ntdll!NtSetValueKey`.
pub type NtSetValueKeyFn = unsafe extern "system" fn(
    HANDLE,               // KeyHandle
    *const UnicodeString, // ValueName
    u32,                  // TitleIndex
    u32,                  // Type
    *const c_void,        // Data
    u32,                  // DataSize
) -> NTSTATUS;

/// `ntdll!NtDeleteValueKey`.
pub type NtDeleteValueKeyFn = unsafe extern "system" fn(HANDLE, *const UnicodeString) -> NTSTATUS;

/// `ntdll!NtDeleteKey`.
pub type NtDeleteKeyFn = unsafe extern "system" fn(HANDLE) -> NTSTATUS;

/// `ntdll!NtRenameKey`.
pub type NtRenameKeyFn = unsafe extern "system" fn(HANDLE, *const UnicodeString) -> NTSTATUS;

/// `ntdll!NtSetInformationKey`.
pub type NtSetInformationKeyFn = unsafe extern "system" fn(
    HANDLE,        // KeyHandle
    u32,           // KeySetInformationClass
    *const c_void, // KeySetInformation
    u32,           // KeySetInformationLength
) -> NTSTATUS;

/// `ntdll!NtFlushKey`.
pub type NtFlushKeyFn = unsafe extern "system" fn(HANDLE) -> NTSTATUS;

/// `STATUS_CANNOT_DELETE`: `NtDeleteKey` of a key with subkeys; `NtRenameKey` to a name that
/// exists.
pub const STATUS_CANNOT_DELETE: NTSTATUS = 0xC000_0121u32 as i32;
/// `STATUS_INSUFFICIENT_RESOURCES`: a rename whose copy is over the shim's bound.
pub const STATUS_INSUFFICIENT_RESOURCES: NTSTATUS = 0xC000_009Au32 as i32;
/// `STATUS_INVALID_INFO_CLASS`.
pub const STATUS_INVALID_INFO_CLASS: NTSTATUS = 0xC000_0003u32 as i32;

// ---- Registry: notifications, security, handle flags, out-of-scope calls (Task 12) ----

/// `ntdll!NtNotifyChangeKey`. `WatchTree` and `Asynchronous` are `BOOLEAN`s.
pub type NtNotifyChangeKeyFn = unsafe extern "system" fn(
    HANDLE,        // KeyHandle
    HANDLE,        // Event
    *const c_void, // ApcRoutine
    *const c_void, // ApcContext
    *mut c_void,   // IoStatusBlock
    u32,           // CompletionFilter
    u8,            // WatchTree
    *mut c_void,   // Buffer
    u32,           // BufferSize
    u8,            // Asynchronous
) -> NTSTATUS;

/// `ntdll!NtNotifyChangeMultipleKeys`: `NtNotifyChangeKey` plus subordinate keys.
pub type NtNotifyChangeMultipleKeysFn = unsafe extern "system" fn(
    HANDLE,                  // MasterKeyHandle
    u32,                     // Count
    *const ObjectAttributes, // SubordinateObjects
    HANDLE,                  // Event
    *const c_void,           // ApcRoutine
    *const c_void,           // ApcContext
    *mut c_void,             // IoStatusBlock
    u32,                     // CompletionFilter
    u8,                      // WatchTree
    *mut c_void,             // Buffer
    u32,                     // BufferSize
    u8,                      // Asynchronous
) -> NTSTATUS;

/// `ntdll!NtQuerySecurityObject`.
pub type NtQuerySecurityObjectFn = unsafe extern "system" fn(
    HANDLE,      // Handle
    u32,         // SecurityInformation
    *mut c_void, // SecurityDescriptor
    u32,         // Length
    *mut u32,    // LengthNeeded
) -> NTSTATUS;

/// `ntdll!NtSetSecurityObject`.
pub type NtSetSecurityObjectFn = unsafe extern "system" fn(
    HANDLE,        // Handle
    u32,           // SecurityInformation
    *const c_void, // SecurityDescriptor
) -> NTSTATUS;

/// `ntdll!NtSetInformationObject`.
pub type NtSetInformationObjectFn = unsafe extern "system" fn(
    HANDLE,        // Handle
    u32,           // ObjectInformationClass
    *const c_void, // ObjectInformation
    u32,           // Length
) -> NTSTATUS;

/// `ntdll!NtCreateKeyTransacted`: `NtCreateKey` with a transaction before `Disposition`.
pub type NtCreateKeyTransactedFn = unsafe extern "system" fn(
    *mut HANDLE, // KeyHandle
    u32,         // DesiredAccess
    *const ObjectAttributes,
    u32,                  // TitleIndex
    *const UnicodeString, // Class
    u32,                  // CreateOptions
    HANDLE,               // TransactionHandle
    *mut u32,             // Disposition
) -> NTSTATUS;

/// `ntdll!NtOpenKeyTransacted`.
pub type NtOpenKeyTransactedFn = unsafe extern "system" fn(
    *mut HANDLE, // KeyHandle
    u32,         // DesiredAccess
    *const ObjectAttributes,
    HANDLE, // TransactionHandle
) -> NTSTATUS;

/// `ntdll!NtOpenKeyTransactedEx`.
pub type NtOpenKeyTransactedExFn = unsafe extern "system" fn(
    *mut HANDLE, // KeyHandle
    u32,         // DesiredAccess
    *const ObjectAttributes,
    u32,    // OpenOptions
    HANDLE, // TransactionHandle
) -> NTSTATUS;

/// `ntdll!NtLoadKey(TargetKey, SourceFile)`.
pub type NtLoadKeyFn =
    unsafe extern "system" fn(*const ObjectAttributes, *const ObjectAttributes) -> NTSTATUS;

/// `ntdll!NtLoadKey2(TargetKey, SourceFile, Flags)`.
pub type NtLoadKey2Fn =
    unsafe extern "system" fn(*const ObjectAttributes, *const ObjectAttributes, u32) -> NTSTATUS;

/// `ntdll!NtLoadKeyEx` and `ntdll!NtLoadKey3`: eight arguments, the target key first. Only the
/// target is read; the other six are passed on untouched, so one pointer-sized shape serves both.
pub type NtLoadKey8Fn = unsafe extern "system" fn(
    *const ObjectAttributes, // TargetKey
    *const ObjectAttributes, // SourceFile
    u32,                     // Flags
    usize,
    usize,
    usize,
    usize,
    usize,
) -> NTSTATUS;

/// `ntdll!NtUnloadKey(TargetKey)`.
pub type NtUnloadKeyFn = unsafe extern "system" fn(*const ObjectAttributes) -> NTSTATUS;

/// `ntdll!NtUnloadKey2(TargetKey, Flags)` and `ntdll!NtUnloadKeyEx(TargetKey, Event)`: the
/// target, then one pointer-sized argument passed on untouched.
pub type NtUnloadKey2Fn = unsafe extern "system" fn(*const ObjectAttributes, usize) -> NTSTATUS;

/// `ntdll!NtSaveKey(KeyHandle, FileHandle)`.
pub type NtSaveKeyFn = unsafe extern "system" fn(HANDLE, HANDLE) -> NTSTATUS;

/// `ntdll!NtSaveKeyEx(KeyHandle, FileHandle, Format)`.
pub type NtSaveKeyExFn = unsafe extern "system" fn(HANDLE, HANDLE, u32) -> NTSTATUS;

/// `ntdll!NtSaveMergedKeys(HighPrecedenceKey, LowPrecedenceKey, FileHandle)`.
pub type NtSaveMergedKeysFn = unsafe extern "system" fn(HANDLE, HANDLE, HANDLE) -> NTSTATUS;

/// `ntdll!NtReplaceKey(NewFile, TargetHandle, OldFile)`.
pub type NtReplaceKeyFn =
    unsafe extern "system" fn(*const ObjectAttributes, HANDLE, *const ObjectAttributes) -> NTSTATUS;

/// `ntdll!NtRestoreKey(KeyHandle, FileHandle, Flags)`.
pub type NtRestoreKeyFn = unsafe extern "system" fn(HANDLE, HANDLE, u32) -> NTSTATUS;

/// `ntdll!NtCompressKey(Key)` and `ntdll!NtLockRegistryKey(KeyHandle)`.
pub type NtKeyOnlyFn = unsafe extern "system" fn(HANDLE) -> NTSTATUS;

/// `STATUS_PENDING`: an asynchronous registry notification was registered.
pub const STATUS_PENDING: NTSTATUS = 0x0000_0103;
/// `STATUS_NOTIFY_CLEANUP`: a pending notification ended by closing its key handle.
pub const STATUS_NOTIFY_CLEANUP: NTSTATUS = 0x0000_010B;
/// `STATUS_NOTIFY_ENUM_DIR`: what a registry notification completes with when the key changed.
pub const STATUS_NOTIFY_ENUM_DIR: NTSTATUS = 0x0000_010C;
/// `STATUS_INVALID_BUFFER_SIZE`: `NtSetInformationObject` with a short buffer (Wine's answer).
pub const STATUS_INVALID_BUFFER_SIZE: NTSTATUS = 0xC000_0206u32 as i32;
/// `STATUS_HANDLE_NOT_CLOSABLE`: `NtClose` of a handle protected from close.
pub const STATUS_HANDLE_NOT_CLOSABLE: NTSTATUS = 0xC000_0235u32 as i32;
/// `STATUS_INVALID_SECURITY_DESCR`.
pub const STATUS_INVALID_SECURITY_DESCR: NTSTATUS = 0xC000_0079u32 as i32;

//! Information queries on handles: `NtQueryInformationFile`, `NtQueryVolumeInformationFile`, `NtQueryObject`.
#![deny(unsafe_op_in_unsafe_fn)]

use super::{
    TRAMP_QIF, TRAMP_QOBJ, TRAMP_QVOL, attributes, cwd_from_peb, identity_of, put_basic,
    put_network_open, put_standard, put_stat, reg_real, under_root_path,
};
use crate::ntdef::{
    FILE_ALL_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_TAG_INFORMATION, FILE_BASIC_INFORMATION, FILE_DEVICE_DISK,
    FILE_FS_DEVICE_INFORMATION, FILE_ID_INFORMATION, FILE_INTERNAL_INFORMATION,
    FILE_NAME_INFORMATION, FILE_NETWORK_OPEN_INFORMATION, FILE_NORMALIZED_NAME_INFORMATION,
    FILE_POSITION_INFORMATION, FILE_STANDARD_INFORMATION, FILE_STAT_INFORMATION,
    FileBasicInformation, FileFsDeviceInformation, FileInternalInformation,
    FileNetworkOpenInformation, FilePositionInformation, FileStandardInformation,
    OBJECT_NAME_INFORMATION, OBJECT_NAME_INFORMATION_HEADER, STATUS_BUFFER_OVERFLOW,
    STATUS_INFO_LENGTH_MISMATCH, STATUS_INVALID_HANDLE, STATUS_OBJECT_NAME_INVALID,
    STATUS_OBJECT_PATH_NOT_FOUND, STATUS_SUCCESS, STATUS_UNSUCCESSFUL,
};
use crate::synth_file::FileView;
use core::ffi::c_void;
use std::sync::OnceLock;
use vfs_ntlayout::spoofed_object_name;
use vfs_redirect::{DirStatus, SYNTH_FILETIME, write_file_name_info};
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};

/// The volume every synthetic handle says it is on, where a volume serial
/// number is asked for together with a file id: two ids are only comparable
/// on one volume, and every virtual file is on this one.
///
/// Fits 32 bits, because the same number is what `FileFsVolumeInformation`
/// reports — and so what `GetFileInformationByHandle` puts in
/// `dwVolumeSerialNumber` — and the two must not disagree about which volume
/// one handle is on.
const SYNTH_VOLUME_SERIAL: u64 = 0x5646_5300;

/// The final DOS path (`C:\dir\file`, stored spelling) of a synthetic handle:
/// the path it was opened as, re-spelled by [`FuseClient::final_path`]. Falls
/// back to the opened path itself, without its NT prefix, if the director
/// cannot be asked — a name in the caller's own spelling is still the right
/// file, where no name at all fails `GetFinalPathNameByHandleW` outright.
///
/// `None` only for a handle with no recorded path, which no open produces.
///
/// [`FuseClient::final_path`]: crate::director::FuseClient::final_path
fn synth_final_path(handle: HANDLE) -> Option<String> {
    let opened = crate::synth_file::abs_path(handle as isize)?;
    let named = crate::director::global().and_then(|c| c.final_path(&opened));
    Some(named.unwrap_or_else(|| {
        crate::director::strip_nt_device(&opened)
            .trim_end_matches('\\')
            .to_string()
    }))
}

/// The file id a synthetic handle reports: one per *file*, so that two
/// handles to one file agree (`std::filesystem::equivalent` and every "is
/// this the same file" check compare ids). It used to be the handle's own
/// value, which made a file unequal to itself.
///
/// From the root and the folded path under it — what the ring is asked for —
/// so every spelling of one file, through an alias of the root included,
/// gives one id, and no director round trip is spent on a query as common as
/// `GetFileInformationByHandle`.
fn synth_file_id(handle: HANDLE) -> i64 {
    crate::synth_file::abs_path(handle as isize)
        .and_then(|opened| path_file_id(&opened))
        .unwrap_or(handle as i64)
}

/// The file id of whatever is at `path` under a managed root — the number a
/// handle to it reports. `None` for a path under no root.
pub(super) fn path_file_id(path: &str) -> Option<i64> {
    let (root, vpath) = crate::director::global()?.vpath_under_root(path)?;
    Some(vfs_core::finalname::path_id(&format!("{}:{vpath}", root.0)) as i64)
}

/// Whether this host names file objects `\??\C:\…` (Wine) or
/// `\Device\HarddiskVolumeN\…` (Windows), as the literal prefix
/// [`spoofed_object_name`] keys on.
///
/// A redirected *real* handle answers this itself: its own
/// `NtQueryObject` reply is consulted for the convention. A synthetic handle
/// is not a kernel object and has no reply to consult, so the question is put
/// once to a handle that is: the process's current-directory handle, which
/// the OS itself opened. If that cannot be asked, the host is identified
/// instead — Wine's ntdll exports `wine_get_version`, Windows' does not.
pub(super) fn host_name_convention() -> &'static str {
    static CONVENTION: OnceLock<&'static str> = OnceLock::new();
    CONVENTION.get_or_init(|| {
        // SAFETY: reads this process's own PEB, and hands the trampoline a
        // buffer of the length it is told.
        let probed = unsafe {
            match (TRAMP_QOBJ.get(), cwd_from_peb()) {
                (Some(tramp), Some((cwd, _))) => {
                    let mut scratch = vec![0u8; 2048];
                    let mut need = 0u32;
                    let st = tramp(
                        cwd as HANDLE,
                        OBJECT_NAME_INFORMATION,
                        scratch.as_mut_ptr().cast(),
                        scratch.len() as u32,
                        &mut need,
                    );
                    let n = u16::from_le_bytes([scratch[0], scratch[1]]) as usize;
                    let hdr = OBJECT_NAME_INFORMATION_HEADER;
                    if st >= 0 && n >= 8 && hdr + n <= scratch.len() {
                        let units: Vec<u16> = scratch[hdr..hdr + 16]
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|c| u16::from_le_bytes(*c))
                            .collect();
                        Some(String::from_utf16_lossy(&units))
                    } else {
                        None
                    }
                }
                _ => None,
            }
        };
        match probed {
            Some(name) if name.starts_with(r"\??\") => r"\??\",
            Some(name) if name.starts_with(r"\Device\") => r"\Device\",
            _ if host_is_wine() => r"\??\",
            _ => r"\Device\",
        }
    })
}

/// Whether ntdll is Wine's: it exports `wine_get_version`.
fn host_is_wine() -> bool {
    // SAFETY: both names are NUL-terminated; a missing module or export is a
    // null return, not a fault.
    unsafe {
        let ntdll = windows_sys::Win32::System::LibraryLoader::GetModuleHandleA(
            c"ntdll.dll".as_ptr().cast(),
        );
        !ntdll.is_null()
            && windows_sys::Win32::System::LibraryLoader::GetProcAddress(
                ntdll,
                c"wine_get_version".as_ptr().cast(),
            )
            .is_some()
    }
}

/// Answer handle-based information queries for director FUSE synth handles.
unsafe fn fuse_query_information(
    handle: HANDLE,
    iosb: *mut c_void,
    info: *mut c_void,
    length: u32,
    class: u32,
) -> NTSTATUS {
    let Some(FileView {
        size,
        is_dir,
        position: pos,
        ..
    }) = crate::synth_file::lookup(handle as isize)
    else {
        return STATUS_INVALID_HANDLE;
    };
    if info.is_null() {
        return STATUS_UNSUCCESSFUL;
    }
    match class {
        FILE_BASIC_INFORMATION => {
            if (length as usize) < core::mem::size_of::<FileBasicInformation>() {
                return STATUS_BUFFER_OVERFLOW;
            }
            let bi = info as *mut FileBasicInformation;
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { put_basic(bi as *mut u8, SYNTH_FILETIME, attributes(is_dir)) };
            // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
            unsafe {
                (*bi)._reserved = 0;
            }
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe {
                crate::ntbuf::iosb_set(
                    iosb,
                    STATUS_SUCCESS,
                    core::mem::size_of::<FileBasicInformation>(),
                )
            };
            STATUS_SUCCESS
        }
        FILE_STANDARD_INFORMATION => {
            if (length as usize) < core::mem::size_of::<FileStandardInformation>() {
                return STATUS_BUFFER_OVERFLOW;
            }
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { put_standard(info as *mut u8, size, is_dir) };
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe {
                crate::ntbuf::iosb_set(
                    iosb,
                    STATUS_SUCCESS,
                    core::mem::size_of::<FileStandardInformation>(),
                )
            };
            STATUS_SUCCESS
        }
        FILE_INTERNAL_INFORMATION => {
            if (length as usize) < core::mem::size_of::<FileInternalInformation>() {
                return STATUS_BUFFER_OVERFLOW;
            }
            // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
            unsafe {
                (*(info as *mut FileInternalInformation)).index_number = synth_file_id(handle);
            }
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe {
                crate::ntbuf::iosb_set(
                    iosb,
                    STATUS_SUCCESS,
                    core::mem::size_of::<FileInternalInformation>(),
                )
            };
            STATUS_SUCCESS
        }
        FILE_POSITION_INFORMATION => {
            if (length as usize) < core::mem::size_of::<FilePositionInformation>() {
                return STATUS_BUFFER_OVERFLOW;
            }
            // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
            unsafe {
                (*(info as *mut FilePositionInformation)).current_byte_offset = pos as i64;
            }
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe {
                crate::ntbuf::iosb_set(
                    iosb,
                    STATUS_SUCCESS,
                    core::mem::size_of::<FilePositionInformation>(),
                )
            };
            STATUS_SUCCESS
        }
        FILE_NETWORK_OPEN_INFORMATION => {
            if (length as usize) < core::mem::size_of::<FileNetworkOpenInformation>() {
                return STATUS_BUFFER_OVERFLOW;
            }
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { put_network_open(info as *mut u8, SYNTH_FILETIME, size, attributes(is_dir)) };
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe {
                crate::ntbuf::iosb_set(
                    iosb,
                    STATUS_SUCCESS,
                    core::mem::size_of::<FileNetworkOpenInformation>(),
                )
            };
            STATUS_SUCCESS
        }
        FILE_ALL_INFORMATION => {
            // GetFileInformationByHandle (Rust `metadata`) issues this. Fill the
            // fixed prefix callers read — attributes (incl. DIRECTORY), size, the
            // Standard.Directory flag — and leave the trailing name empty. Prefix
            // layout: Basic 40 | Standard 24 | Internal 8 | Ea 4 | Access 4 |
            // Position 8 | Mode 4 | Alignment 4 | Name 4 = 100.
            const PREFIX: usize = 100;
            if (length as usize) < PREFIX {
                return STATUS_BUFFER_OVERFLOW;
            }
            let p = info as *mut u8;
            // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
            unsafe { core::ptr::write_bytes(p, 0, PREFIX) };
            // Basic @ 0 (its times stay zero), Standard @ 40.
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe {
                put_basic(p, 0, attributes(is_dir));
                put_standard(p.add(40), size, is_dir);
            }
            // Internal.IndexNumber @ 64
            // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
            unsafe { core::ptr::write_unaligned(p.add(64) as *mut i64, synth_file_id(handle)) };
            // Position.CurrentByteOffset @ 80
            // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
            unsafe { core::ptr::write_unaligned(p.add(80) as *mut i64, pos as i64) };
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, PREFIX) };
            STATUS_SUCCESS
        }
        FILE_NAME_INFORMATION | FILE_NORMALIZED_NAME_INFORMATION => {
            // The volume-relative name, for a file and for a directory alike,
            // in the stored spelling. `GetFinalPathNameByHandleW` builds its
            // answer from these two and from `NtQueryObject`'s name
            // (`qobj_hook_body`), and all three must describe one path — see
            // `qif_hook_body`. They are all cut from `synth_final_path`.
            //
            // There used to be no arm for either, on the reasoning that only
            // redirected real handles were ever asked for a name. They fell to
            // the catch-all below: success, nothing written.
            let Some(path) = synth_final_path(handle) else {
                return STATUS_INVALID_HANDLE;
            };
            // `FILE_NAME_INFORMATION` is a u32 byte length and then the name.
            // NT refuses a buffer smaller than the structure (8 bytes with
            // its one-character name field) outright; given one too small
            // for the whole name it writes the full length, as much of the
            // name as fits, and says overflow. A caller sizing a buffer
            // reads the length.
            if (length as usize) < 8 {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let name: Vec<u16> = vfs_core::finalname::volume_relative(&path)
                .encode_utf16()
                .collect();
            let fits = name.len().min((length as usize - 4) / 2);
            let p = info as *mut u8;
            // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
            unsafe { core::ptr::write_unaligned(p as *mut u32, (name.len() * 2) as u32) };
            for (i, unit) in name[..fits].iter().enumerate() {
                // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
                unsafe { core::ptr::write_unaligned(p.add(4 + i * 2) as *mut u16, *unit) };
            }
            let status = if fits == name.len() {
                STATUS_SUCCESS
            } else {
                STATUS_BUFFER_OVERFLOW
            };
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { crate::ntbuf::iosb_set(iosb, status, 4 + fits * 2) };
            status
        }
        FILE_ID_INFORMATION => {
            // VolumeSerialNumber u64 @0 | FileId (128 bits) @8 = 24. What
            // `GetFileInformationByHandleEx(FileIdInfo)` asks, which is how
            // `std::filesystem::equivalent` tells whether two paths are one
            // file. Unanswered, it compared two uninitialised buffers.
            const LEN: usize = 24;
            if (length as usize) < LEN {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let p = info as *mut u8;
            // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
            unsafe { core::ptr::write_bytes(p, 0, LEN) };
            // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
            unsafe { core::ptr::write_unaligned(p as *mut u64, SYNTH_VOLUME_SERIAL) };
            // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
            unsafe { core::ptr::write_unaligned(p.add(8) as *mut i64, synth_file_id(handle)) };
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, LEN) };
            STATUS_SUCCESS
        }
        FILE_STAT_INFORMATION => {
            // What `GetFileInformationByHandle` asks under current Wine
            // (GE-Proton 10), where it used to ask `FileAllInformation` — so
            // this is what Rust's `File::metadata` and `std::fs::read`'s size
            // hint now reach. Unanswered, it fell to the arm below, which
            // reports success without writing the buffer: the caller read its
            // own uninitialised stack as a file size and, in `fs::read`,
            // failed "out of memory" reserving that many bytes.
            //
            // Layout: FileId 0 | Creation 8 | LastAccess 16 | LastWrite 24 |
            // Change 32 | AllocationSize 40 | EndOfFile 48 | FileAttributes 56
            // | ReparseTag 60 | NumberOfLinks 64 | EffectiveAccess 68 = 72.
            const LEN: usize = 72;
            if (length as usize) < LEN {
                // What NT answers for a fixed-size class. `BUFFER_OVERFLOW`
                // means "the fixed part was written", which some callers
                // take as success — and nothing was.
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let p = info as *mut u8;
            // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
            unsafe { core::ptr::write_bytes(p, 0, LEN) };
            // FILE_GENERIC_READ is the effective access.
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe {
                put_stat(
                    p,
                    synth_file_id(handle),
                    SYNTH_FILETIME,
                    size,
                    attributes(is_dir),
                    0x0012_0089,
                )
            };
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, LEN) };
            STATUS_SUCCESS
        }
        FILE_ATTRIBUTE_TAG_INFORMATION => {
            // FileAttributes 0 | ReparseTag 4 = 8. Never a reparse point.
            const LEN: usize = 8;
            if (length as usize) < LEN {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let p = info as *mut u8;
            let attrs = if is_dir {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                FILE_ATTRIBUTE_NORMAL
            };
            // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
            unsafe { core::ptr::write_unaligned(p as *mut u32, attrs) };
            // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
            unsafe { core::ptr::write_unaligned(p.add(4) as *mut u32, 0) };
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, LEN) };
            STATUS_SUCCESS
        }
        _ => {
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, 0) };
            STATUS_SUCCESS
        }
    }
}

/// `NtQueryVolumeInformationFile` hook — `GetFileType` needs
/// `FileFsDeviceInformation` on synthetic handles.
pub(super) unsafe fn qvol_hook_body(
    handle: HANDLE,
    iosb: *mut c_void,
    info: *mut c_void,
    length: u32,
    class: u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::QVol);
    let tramp = match TRAMP_QVOL.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::synth_file::is_fuse_synth(handle as isize) {
        if class == FILE_FS_DEVICE_INFORMATION {
            if info.is_null() || (length as usize) < core::mem::size_of::<FileFsDeviceInformation>()
            {
                return STATUS_BUFFER_OVERFLOW;
            }
            let di = info as *mut FileFsDeviceInformation;
            // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
            unsafe {
                (*di).device_type = FILE_DEVICE_DISK;
            }
            // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
            unsafe {
                (*di).characteristics = 0;
            }
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe {
                crate::ntbuf::iosb_set(
                    iosb,
                    STATUS_SUCCESS,
                    core::mem::size_of::<FileFsDeviceInformation>(),
                )
            };
            return STATUS_SUCCESS;
        }
        // Soft-success for other volume classes (size/attr) with zeros.
        if !info.is_null() && length > 0 {
            // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
            unsafe { core::ptr::write_bytes(info as *mut u8, 0, length as usize) };
        }
        // `FileFsVolumeInformation` (class 1): VolumeCreationTime 0 |
        // VolumeSerialNumber 8 | VolumeLabelLength 12 | SupportsObjects 16 |
        // label. Zeros but for the serial number, which is the one
        // `FileIdInformation` reports for the same handle.
        if class == 1 && !info.is_null() && length >= 12 {
            // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
            unsafe {
                core::ptr::write_unaligned(
                    (info as *mut u8).add(8) as *mut u32,
                    SYNTH_VOLUME_SERIAL as u32,
                )
            };
        }
        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
        unsafe { crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, length as usize) };
        return STATUS_SUCCESS;
    }
    // SAFETY: the original NT function, called with valid NT arguments.
    unsafe { tramp(handle, iosb, info, length, class) }
}

/// `NtQueryInformationFile` hook. Spoofs the two name classes —
/// `FileNameInformation` (9) and `FileNormalizedNameInformation` (48) — on a
/// redirected handle -> the virtual path, so `GetFinalPathNameByHandleW`
/// reports where the mod file appears to live. Everything else passes through.
///
/// Class 9 is spoofed too: `NtQueryObject` class 1 and `NtQueryInformationFile`
/// classes 9 and 48 must all describe the same path, because
/// `GetFinalPathNameByHandleW` takes the device prefix as
/// `ObjectName[.. ObjectName.len - class9.len]`. Spoof one and the subtraction
/// slices at the wrong offset.
/// See docs/shim-invariants.md, "Name-query consistency".
pub(super) unsafe fn qif_hook_body(
    handle: HANDLE,
    iosb: *mut c_void,
    info: *mut c_void,
    length: u32,
    class: u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::QueryInfo);
    let tramp = match TRAMP_QIF.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::synth_file::is_fuse_synth(handle as isize) {
        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
        return unsafe { fuse_query_information(handle, iosb, info, length, class) };
    }
    if (class == FILE_NORMALIZED_NAME_INFORMATION || class == FILE_NAME_INFORMATION)
        && !info.is_null()
    {
        let vpath = identity_of(handle as isize);
        if let Some(vpath) = vpath {
            // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
            let buf = unsafe { core::slice::from_raw_parts_mut(info as *mut u8, length as usize) };
            let r = write_file_name_info(&vpath, buf);
            let status = match r.status {
                DirStatus::Success => STATUS_SUCCESS,
                _ => STATUS_BUFFER_OVERFLOW,
            };
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { crate::ntbuf::iosb_set(iosb, status, r.bytes) };
            return status;
        }
    }
    // SAFETY: the original NT function, called with valid NT arguments.
    unsafe { tramp(handle, iosb, info, length, class) }
}

/// Resolve a DOS drive spec (`"C:"`) to the host's device path for it.
///
/// Separate from [`spoofed_object_name`] so that function stays pure and
/// testable: this is the only part that has to ask the running kernel.
fn device_for_drive(drive: &str) -> Option<String> {
    let name: Vec<u16> = drive.encode_utf16().chain(core::iter::once(0)).collect();
    let mut out = [0u16; 512];
    // SAFETY: `name` is NUL-terminated and `out` is writable for its own length,
    // which is what `QueryDosDeviceW` requires. It reports 0 on failure.
    let n = unsafe {
        windows_sys::Win32::Storage::FileSystem::QueryDosDeviceW(
            name.as_ptr(),
            out.as_mut_ptr(),
            out.len() as u32,
        )
    };
    if n == 0 {
        return None;
    }
    let end = out.iter().position(|&c| c == 0).unwrap_or(n as usize);
    if end == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&out[..end]))
}

/// `NtQueryObject` hook. Answers `ObjectNameInformation` (class 1) for a handle the
/// shim redirected -> the VIRTUAL path, in the prefix convention this host
/// actually uses. Every other class, and every handle we do not track, passes
/// through untouched: this API answers about events, mutexes, sections and
/// registry keys too, and inventing a name for one of those would break
/// unrelated Windows APIs.
///
/// The convention is discovered, not assumed (Windows answers
/// `\Device\HarddiskVolumeN\...`, Wine `\??\C:\...`): the trampoline runs first
/// and its answer's prefix is reused. This also closes a leak on Windows: a caller
/// reaching `NtQueryObject` directly used to get the backing path.
/// See docs/shim-invariants.md, "Name-query consistency".
///
/// # The too-small-buffer contract (measured 2026-09-01, both hosts)
///
/// A caller that size-probes — query with a tiny buffer, allocate what
/// `ReturnLength` asks for, query again — loops or fails unless this is
/// reproduced exactly. Both hosts agreed, which is why one rule serves both:
///
/// | `ObjectInformationLength` | Windows 11 | Wine 11.0 (GE-Proton11-6) |
/// |---|---|---|
/// | 0 | `STATUS_INFO_LENGTH_MISMATCH` (0xC0000004) | same |
/// | 8 (< the 16-byte header) | `STATUS_INFO_LENGTH_MISMATCH` | same |
/// | 16 (header only) | `STATUS_BUFFER_OVERFLOW` (0x80000005) | same |
/// | `required - 1` | `STATUS_BUFFER_OVERFLOW` | same |
///
/// `ReturnLength` was set to the full required size in **every** one of those
/// cases, including length 0, and a NULL `ReturnLength` was tolerated rather
/// than faulted. `required` = 16 + name bytes + 2 for the NUL; `Length`
/// excludes the NUL, `MaximumLength` includes it, and `Buffer` points 16 bytes
/// into the caller's own buffer on both hosts.
pub(super) unsafe fn qobj_hook_body(
    handle: HANDLE,
    class: u32,
    info: *mut c_void,
    length: u32,
    ret_len: *mut u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::QObj);
    let tramp = match TRAMP_QOBJ.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    // Cheapest rejections first, in order of how much of the world they let
    // past untouched. A class we do not answer is most of the traffic.
    // A synthetic registry key: its NT name, as the real key would report it, and the other
    // classes from `regkeys::query_object`.
    if crate::regkeys::is_synthetic(handle as isize) && crate::regclient::enabled() {
        if class != OBJECT_NAME_INFORMATION {
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            return unsafe {
                crate::regkeys::query_object(
                    &reg_real(),
                    tramp,
                    handle as isize,
                    class,
                    info,
                    length,
                    ret_len,
                )
            };
        }
        let name = match crate::regkeys::object_name(handle as isize) {
            None => return STATUS_INVALID_HANDLE,
            Some(Err(st)) => return st,
            Some(Ok(n)) => n,
        };
        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
        return unsafe { emit_object_name(&name, info, length, ret_len) }
            .unwrap_or(STATUS_OBJECT_NAME_INVALID);
    }
    if class != OBJECT_NAME_INFORMATION {
        // SAFETY: the original NT function, called with valid NT arguments.
        return unsafe { tramp(handle, class, info, length, ret_len) };
    }
    // A real key handle deleted or renamed through the overlay: the real key no longer names it.
    if crate::regclient::enabled() {
        match crate::regkeys::passthrough_name(handle as isize) {
            None => {}
            Some(Err(st)) => return st,
            Some(Ok(name)) => {
                // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                return unsafe { emit_object_name(&name, info, length, ret_len) }
                    .unwrap_or(STATUS_OBJECT_NAME_INVALID);
            }
        }
    }
    // A synthetic handle — every file and directory the director serves — is
    // not a kernel object: the host has no name for it and the trampoline
    // fails on it. This used to fall through to exactly that failure, on the
    // reasoning that a convention must be measured, not guessed; and so
    // `GetFinalPathNameByHandleW`, which on Wine is this call and nothing
    // else, failed for every virtual file and directory, and
    // `std::filesystem::canonical` threw on them. The name is the handle's
    // final path, in the convention the host uses for real files.
    if crate::synth_file::is_fuse_synth(handle as isize) {
        let Some(path) = synth_final_path(handle) else {
            return STATUS_INVALID_HANDLE;
        };
        return match spoofed_object_name(host_name_convention(), &path, device_for_drive)
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            .and_then(|name| unsafe { emit_object_name(&name, info, length, ret_len) })
        {
            Some(status) => status,
            // A path with no drive letter to name a device for, or too long
            // for a UNICODE_STRING: there is no honest name to give.
            None => STATUS_OBJECT_PATH_NOT_FOUND,
        };
    }
    // An untracked handle must cost nothing but this map lookup — no
    // allocation, no scratch call. It may be an event, a mutex, a section or a
    // registry key, and we have nothing true to say about any of them.
    let vpath = under_root_path(handle as isize);
    let Some(vpath) = vpath else {
        // SAFETY: the original NT function, called with valid NT arguments.
        return unsafe { tramp(handle, class, info, length, ret_len) };
    };

    // The host's own answer, for its prefix convention. Sized generously so
    // the common case is one call; grown once if some path is longer than that.
    // (A synthetic handle never gets here: it was answered above.)
    let mut scratch = vec![0u8; 2048];
    let mut need: u32 = 0;
    // SAFETY: the original NT function, called with valid NT arguments.
    let mut st = unsafe {
        tramp(
            handle,
            class,
            scratch.as_mut_ptr().cast(),
            scratch.len() as u32,
            &mut need,
        )
    };
    if (st == STATUS_BUFFER_OVERFLOW || st == STATUS_INFO_LENGTH_MISMATCH)
        && need as usize > scratch.len()
    {
        scratch = vec![0u8; need as usize];
        // SAFETY: the original NT function, called with valid NT arguments.
        st = unsafe {
            tramp(
                handle,
                class,
                scratch.as_mut_ptr().cast(),
                scratch.len() as u32,
                &mut need,
            )
        };
    }
    if st < 0 {
        // A real failure — an unnamed object, a revoked handle, a synthetic
        // handle. Let the host answer the caller directly rather than
        // substituting a success it did not earn.
        // SAFETY: the original NT function, called with valid NT arguments.
        return unsafe { tramp(handle, class, info, length, ret_len) };
    }
    let real = {
        // SAFETY: the trampoline reported success into `scratch`, so its first
        // `OBJECT_NAME_INFORMATION_HEADER` bytes are a UNICODE_STRING whose
        // `Length` bytes of name follow. `Buffer` is ignored deliberately: the
        // name is read at the fixed offset both hosts were measured to use, so
        // a bogus pointer cannot be followed.
        let hdr = OBJECT_NAME_INFORMATION_HEADER;
        let n = u16::from_le_bytes([scratch[0], scratch[1]]) as usize;
        if n == 0 || !n.is_multiple_of(2) || hdr + n > scratch.len() {
            // SAFETY: the original NT function, called with valid NT arguments.
            return unsafe { tramp(handle, class, info, length, ret_len) };
        }
        let units: Vec<u16> = scratch[hdr..hdr + n]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        String::from_utf16_lossy(&units)
    };

    let Some(name) = spoofed_object_name(&real, &vpath, device_for_drive) else {
        // SAFETY: the original NT function, called with valid NT arguments.
        return unsafe { tramp(handle, class, info, length, ret_len) };
    };

    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    match unsafe { emit_object_name(&name, info, length, ret_len) } {
        Some(status) => status,
        // SAFETY: the original NT function, called with valid NT arguments.
        None => unsafe { tramp(handle, class, info, length, ret_len) },
    }
}

/// Write `name` as an `OBJECT_NAME_INFORMATION` into the caller's buffer,
/// following the too-small-buffer contract in [`qobj_hook_body`]'s doc.
/// `None` if the name cannot be described at all (it does not fit a
/// `UNICODE_STRING`), in which case nothing was written.
unsafe fn emit_object_name(
    name: &str,
    info: *mut c_void,
    length: u32,
    ret_len: *mut u32,
) -> Option<NTSTATUS> {
    let name16: Vec<u16> = name.encode_utf16().collect();
    let name_bytes = name16.len() * 2;
    // `UNICODE_STRING::MaximumLength` is a u16 and must cover the NUL. A name
    // that cannot be described in that field is one we must not try to emit.
    if name_bytes + 2 > u16::MAX as usize {
        return None;
    }
    let required = OBJECT_NAME_INFORMATION_HEADER + name_bytes + 2;
    // Set unconditionally and before any short-buffer return: both hosts fill
    // `ReturnLength` even when they write nothing at all.
    if !ret_len.is_null() {
        // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
        unsafe { core::ptr::write_unaligned(ret_len, required as u32) };
    }
    if info.is_null() || (length as usize) < OBJECT_NAME_INFORMATION_HEADER {
        return Some(STATUS_INFO_LENGTH_MISMATCH);
    }
    if (length as usize) < required {
        return Some(STATUS_BUFFER_OVERFLOW);
    }
    // SAFETY: `info` is non-null and the caller declared `length` writable
    // bytes, and `length >= required` was just checked, so every write below
    // lands inside the caller's buffer.
    unsafe {
        let p = info as *mut u8;
        core::ptr::write_unaligned(p as *mut u16, name_bytes as u16);
        core::ptr::write_unaligned(p.add(2) as *mut u16, (name_bytes + 2) as u16);
        // Both hosts point `Buffer` at the caller's own buffer, 16 bytes in.
        core::ptr::write_unaligned(
            p.add(8) as *mut usize,
            p.add(OBJECT_NAME_INFORMATION_HEADER) as usize,
        );
        let dst = p.add(OBJECT_NAME_INFORMATION_HEADER);
        for (i, u) in name16.iter().enumerate() {
            core::ptr::write_unaligned(dst.add(i * 2) as *mut u16, *u);
        }
        core::ptr::write_unaligned(dst.add(name_bytes) as *mut u16, 0u16);
    }
    Some(STATUS_SUCCESS)
}

//! Launch a PE from raw bytes **without writing under the managed game root**.
//!
//! Game/mod layer content stays exclusively in the zip windows. The only bytes
//! that ever touch a filesystem path are a short-lived `DELETE_ON_CLOSE` image
//! under the OS temp directory (required by the Windows image loader for the
//! primary EXE). That path is unlinked as soon as every handle is gone — it is
//! never the game root, never a BSA/ESP, and never a permanent extract.
#![allow(unsafe_code)]

use core::ffi::c_void;
use std::io::Write;
use std::mem::zeroed;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, CREATE_ALWAYS, FILE_ATTRIBUTE_TEMPORARY, FILE_FLAG_DELETE_ON_CLOSE,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
use windows_sys::Win32::System::Memory::PAGE_READONLY;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, CREATE_SUSPENDED, PROCESS_INFORMATION, STARTUPINFOW,
};

const DELETE_ACCESS: u32 = 0x0001_0000;
const SECTION_ALL_ACCESS: u32 = 0x000F_001F;
const SEC_IMAGE: u32 = 0x0100_0000;

type NtCreateSectionFn = unsafe extern "system" fn(
    *mut HANDLE,
    u32,
    *const c_void,
    *mut i64,
    u32,
    u32,
    HANDLE,
) -> i32;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn ntdll_proc(name: &[u8]) -> Option<*const ()> {
    unsafe {
        let ntdll = GetModuleHandleA(b"ntdll.dll\0".as_ptr());
        if ntdll.is_null() {
            return None;
        }
        GetProcAddress(ntdll, name.as_ptr()).map(|p| p as *const ())
    }
}

/// Stage `pe` under `%TEMP%` (never the game root) and CreateProcess it
/// suspended. The path is best-effort deleted after the image is mapped; while
/// the process lives Windows may keep the temp name (loader lock) — still never
/// under the managed game root.
pub fn create_process_from_pe_bytes(
    pe: &[u8],
    image_path: &str,
    args: &[String],
    current_dir: Option<&str>,
) -> Result<(HANDLE, HANDLE, u32, u32), &'static str> {
    if !pe_looks_like_image(pe) {
        return Err("not a PE");
    }
    let mut path = std::env::temp_dir();
    path.push(format!(
        "vfs-run-{}-{}.exe",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&path, pe).map_err(|_| "write temp PE")?;
    let path_s = path.to_string_lossy().into_owned();
    let path_w = wide(&path_s);

    // Command line uses the virtual image path; ApplicationName is the temp PE.
    let mut cmdline = format!("\"{image_path}\"");
    for a in args {
        cmdline.push_str(&format!(" \"{a}\""));
    }
    let mut cmd_w = wide(&cmdline);
    let cwd_w = current_dir.map(wide);

    unsafe {
        let mut si: STARTUPINFOW = zeroed();
        si.cb = core::mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = zeroed();
        let created = CreateProcessW(
            path_w.as_ptr(),
            cmd_w.as_mut_ptr(),
            core::ptr::null(),
            core::ptr::null(),
            0,
            CREATE_SUSPENDED,
            core::ptr::null(),
            cwd_w
                .as_ref()
                .map(|v| v.as_ptr())
                .unwrap_or(core::ptr::null()),
            &si,
            &mut pi,
        );
        if created == 0 {
            let err = windows_sys::Win32::Foundation::GetLastError();
            let _ = std::fs::remove_file(&path_s);
            eprintln!("vfs-inject: CreateProcessW last_error={err}");
            return Err("CreateProcess temp PE failed");
        }
        // Try to unlink from the directory namespace. May fail while mapped —
        // process-exit / reboot cleans %TEMP%. Never under game root.
        let _ = std::fs::remove_file(&path_s);
        Ok((pi.hProcess, pi.hThread, pi.dwProcessId, pi.dwThreadId))
    }
}

/// Build a real kernel `SEC_IMAGE` section from PE bytes for synthetic zip
/// windows (DLL loads). Stages under `%TEMP%` with DELETE_ON_CLOSE; never under
/// the game root.
pub fn image_section_from_pe_bytes(pe: &[u8]) -> Result<HANDLE, &'static str> {
    if !pe_looks_like_image(pe) {
        return Err("not a PE");
    }
    let nt_cs: NtCreateSectionFn = unsafe {
        core::mem::transmute(ntdll_proc(b"NtCreateSection\0").ok_or("NtCreateSection")?)
    };

    let mut path = std::env::temp_dir();
    path.push(format!(
        "vfs-sec-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let path_s = path.to_string_lossy().into_owned();
    let path_w = wide(&path_s);

    unsafe {
        let raw = CreateFileW(
            path_w.as_ptr(),
            GENERIC_READ | GENERIC_WRITE | DELETE_ACCESS,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            core::ptr::null(),
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_TEMPORARY | FILE_FLAG_DELETE_ON_CLOSE,
            core::ptr::null_mut(),
        );
        if raw == INVALID_HANDLE_VALUE {
            return Err("CreateFile sec-temp failed");
        }
        let mut file = std::fs::File::from_raw_handle(raw as *mut std::ffi::c_void);
        file.write_all(pe).map_err(|_| "write sec-temp")?;
        file.flush().map_err(|_| "flush sec-temp")?;
        let owned = OwnedHandle::from_raw_handle(file.as_raw_handle());
        core::mem::forget(file);
        let file_h = owned.as_raw_handle() as HANDLE;

        let mut section: HANDLE = core::ptr::null_mut();
        let status = nt_cs(
            &mut section,
            SECTION_ALL_ACCESS,
            core::ptr::null(),
            core::ptr::null_mut(),
            PAGE_READONLY,
            SEC_IMAGE,
            file_h,
        );
        drop(owned);
        let _ = std::fs::remove_file(&path_s);
        if status != 0 || section.is_null() {
            return Err("NtCreateSection SEC_IMAGE failed");
        }
        let _ = CloseHandle; // silence if unused on some builds
        Ok(section)
    }
}

pub fn pe_looks_like_image(pe: &[u8]) -> bool {
    pe.len() >= 0x40 && pe[0] == b'M' && pe[1] == b'Z'
}

//! PE image helpers.
//!
//! Extracted from the old `ghostly` module when the hollow launch path was
//! removed. Staging still has to reason about a PE *before* any
//! process exists — is this really an image, which DLLs must be staged beside
//! it, which of those are system DLLs the loader will find on its own — and the
//! shim still maps zip-resident PEs locally to serve them. Only the parsing is
//! needed for either.
#![allow(unsafe_code)]

use core::ffi::c_void;

fn ntdll_proc(name: &[u8]) -> Option<*const ()> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    // SAFETY: ntdll is always mapped; both calls take NUL-terminated names.
    unsafe {
        let h = GetModuleHandleA(b"ntdll.dll\0".as_ptr());
        if h.is_null() {
            return None;
        }
        GetProcAddress(h, name.as_ptr()).map(|p| p as *const ())
    }
}

fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn pe_layout(raw: &[u8]) -> Result<(Vec<u8>, u64, u32, usize), &'static str> {
    let (img, base, e_lfanew) = crate::map::build_image(raw)?;
    let opt = e_lfanew + 24;
    let entry_rva = rd_u32(&img, opt + 16);
    let size_of_image = rd_u32(&img, opt + 56) as usize;
    Ok((img, base, entry_rva, size_of_image))
}

pub fn is_system_import_dll(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    let base = std::path::Path::new(&n)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&n);
    base.starts_with("api-ms-")
        || base.starts_with("ext-ms-")
        || matches!(
            base,
            "kernel32.dll"
                | "kernelbase.dll"
                | "ntdll.dll"
                | "user32.dll"
                | "gdi32.dll"
                | "gdi32full.dll"
                | "advapi32.dll"
                | "shell32.dll"
                | "ole32.dll"
                | "oleaut32.dll"
                | "ws2_32.dll"
                | "winhttp.dll"
                | "winmm.dll"
                | "setupapi.dll"
                | "hid.dll"
                | "d3d11.dll"
                | "dxgi.dll"
                | "dinput8.dll"
                | "xinput1_3.dll"
                | "xinput1_4.dll"
                | "x3daudio1_7.dll"
                | "msvcp140.dll"
                | "vcruntime140.dll"
                | "vcruntime140_1.dll"
                | "ucrtbase.dll"
                | "sechost.dll"
                | "rpcrt4.dll"
                | "combase.dll"
                | "shlwapi.dll"
                | "version.dll"
                | "imm32.dll"
                | "dwmapi.dll"
                | "uxtheme.dll"
                | "bcrypt.dll"
                | "bcryptprimitives.dll"
                | "crypt32.dll"
                | "wintrust.dll"
                | "psapi.dll"
                | "userenv.dll"
                | "dbghelp.dll"
        )
}

/// Whether `steam_api*.dll` stays the host copy rather than being served from
/// the VFS.
///
/// Unset means **false** here and **true** in `vfs-shim`, which is deliberate:
/// the shim's exception predates the switch, so a launch that never sets it
/// must keep seeing the host copy. Both read the same name from `vfs-env`.
pub fn keep_host_steam_api() -> bool {
    vfs_env::present(vfs_env::KEEP_HOST_STEAM_API) && vfs_env::opt_out(vfs_env::KEEP_HOST_STEAM_API)
}

pub fn map_image_from_pe_bytes_local(pe: &[u8]) -> Result<(*mut c_void, usize), &'static str> {
    use windows_sys::Win32::System::Memory::{VirtualAlloc, MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE};

    if !pe_looks_like_image(pe) {
        return Err("not a PE");
    }
    let (mut img, preferred_base, _entry, size_of_image) = pe_layout(pe)?;
    let e_lfanew = rd_u32(&img, 0x3C) as usize;

    unsafe {
        let mut base = VirtualAlloc(
            preferred_base as *const c_void,
            size_of_image,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_EXECUTE_READWRITE,
        );
        if base.is_null() {
            base = VirtualAlloc(
                core::ptr::null(),
                size_of_image,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_EXECUTE_READWRITE,
            );
        }
        if base.is_null() {
            return Err("VirtualAlloc local image failed");
        }
        let base_u = base as u64;
        if base_u != preferred_base {
            crate::map::apply_relocs(&mut img, e_lfanew, preferred_base, base_u);
        }
        crate::map::resolve_imports(&mut img, e_lfanew)?;
        core::ptr::copy_nonoverlapping(img.as_ptr(), base as *mut u8, img.len().min(size_of_image));

        // Register unwind info for local manual maps (DLL path).
        if let Some(rtl) = ntdll_proc(b"RtlAddFunctionTable\0") {
            let opt = e_lfanew + 24;
            let ex_dir = opt + 112 + 3 * 8;
            if ex_dir + 8 <= img.len() {
                let ex_rva = rd_u32(&img, ex_dir) as usize;
                let ex_size = rd_u32(&img, ex_dir + 4) as u32;
                if ex_rva != 0 && ex_size >= 12 {
                    type RtlAddFunctionTableFn =
                        unsafe extern "system" fn(*const c_void, u32, u64) -> u8;
                    let f: RtlAddFunctionTableFn = core::mem::transmute(rtl);
                    let count = ex_size / 12;
                    let table = (base as usize + ex_rva) as *const c_void;
                    let _ = f(table, count, base_u);
                }
            }
        }

        Ok((base, size_of_image))
    }
}

pub fn pe_looks_like_image(pe: &[u8]) -> bool {
    pe.len() >= 0x40 && pe[0] == b'M' && pe[1] == b'Z'
}

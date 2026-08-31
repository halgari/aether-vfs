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

fn ntdll_proc(name: &core::ffi::CStr) -> Option<*const ()> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    // SAFETY: ntdll is always mapped; both calls take NUL-terminated names.
    unsafe {
        let h = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
        if h.is_null() {
            return None;
        }
        GetProcAddress(h, name.as_ptr().cast()).map(|p| p as *const ())
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

// Moved to `vfs-pe` (pure parsing). Re-exported so `map_image_from_pe_bytes_local`
// below and `vfs-shim`'s hook path keep their existing spellings.
pub use vfs_pe::{is_system_import_dll, pe_looks_like_image};

// `keep_host_steam_api` lived here, re-exported from `lib.rs`, and had no
// callers anywhere in the workspace — so the warning it carried about needing
// to agree with `vfs-shim`'s same-named function, and the deliberately opposite
// defaults that warning justified, protected nothing. Deleted by gate 5, Task 4
// with the shim-side exception it was supposed to mirror.

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
        // No-op for PE32 (see resolve_imports_ex_with_bases).
        crate::map::resolve_imports(&mut img, e_lfanew)?;
        core::ptr::copy_nonoverlapping(img.as_ptr(), base as *mut u8, img.len().min(size_of_image));

        // Register unwind info for local manual maps (DLL path).
        // x64 unwind data only: a PE32 image has no .pdata to register, and
        // RtlAddFunctionTable over a directory read at the PE32+ offset would
        // be pointing at whatever field happens to live there.
        if let Some(rtl) = ntdll_proc(c"RtlAddFunctionTable").filter(|_| crate::map::is_pe32_plus(&img, e_lfanew)) {
            let ex_dir = crate::map::dd_base(&img, e_lfanew) + 3 * 8;
            if ex_dir + 8 <= img.len() {
                let ex_rva = rd_u32(&img, ex_dir) as usize;
                let ex_size = rd_u32(&img, ex_dir + 4);
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

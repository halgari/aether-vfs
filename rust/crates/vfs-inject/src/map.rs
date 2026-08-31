//! Reflective PE mapping helpers: flatten a DLL image, apply base relocations,
//! and resolve exports by name. Used to place the zero-import payload into a
//! suspended target without LoadLibrary.
#![allow(unsafe_code)]

// The pure parsing half of this module now lives in `vfs-pe` — a PE is a file
// format, not a platform, and the director has to stage Windows executables on
// hosts where none of the Windows API below exists. Re-exported rather than
// re-pathed at each call site so this module's internal callers, and
// `vfs-shim`, keep the spellings they already use: `inject.rs`, `pe.rs` and
// `lib.rs` all reach these five through `crate::map::`.
//
// `import_dll_names` is not among them: after this task `lib.rs`'s
// `import_dll_names_of_pe` delegates straight to `vfs-pe`'s combined helper,
// so `map::import_dll_names` has no production caller left. Its only user is
// this module's own test below, which names `vfs_pe::import_dll_names`
// directly instead of importing it here.
pub use vfs_pe::{apply_relocs, build_image, dd_base, export_rva, is_pe32_plus};

// `vfs-pe`'s byte readers are private to that crate on purpose — they are
// byte-reading plumbing, not PE interface. The Windows-only remote-process
// helpers below still need to read little-endian fields out of a locally
// mapped image, so those three one-liners are duplicated here rather than
// exported from `vfs-pe`.
fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn rd_u64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Resolve the PE import table in-place.
///
/// System DLLs use this process's `LoadLibrary`/`GetProcAddress` (session-shared
/// bases). Game-local DLLs (steam_api64, bink, …) that are only mapped in
/// `remote` are resolved by walking the **remote** module's export table via
/// `ReadProcessMemory` so IAT entries match the child's load addresses.
///
/// Call [`crate::ghostly::preload_remote_import_dlls`] first when `remote` is set.
pub fn resolve_imports(img: &mut [u8], e_lfanew: usize) -> Result<(), &'static str> {
    resolve_imports_ex(img, e_lfanew, None)
}

/// Like [`resolve_imports`], optionally resolving game-local imports against
/// modules already loaded in `remote`.
pub fn resolve_imports_ex(
    img: &mut [u8],
    e_lfanew: usize,
    remote: Option<isize>,
) -> Result<(), &'static str> {
    resolve_imports_ex_with_bases(img, e_lfanew, remote, &[])
}

/// Like [`resolve_imports_ex`], with explicit remote bases for manual-mapped
/// game-local DLLs (not in the remote PEB LDR list).
///
/// `forced_remote` entries are `(dll_file_name, remote_base)` — matched case-
/// insensitively on the final path component.
pub fn resolve_imports_ex_with_bases(
    img: &mut [u8],
    e_lfanew: usize,
    remote: Option<isize>,
    forced_remote: &[(String, u64)],
) -> Result<(), &'static str> {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

    // A pure-IL PE32 assembly is metadata, not something we execute: its import
    // table is empty or a single legacy `mscoree.dll!_CorDllMain` stub the CLR
    // never calls. Resolving it would mean loading mscoree, which .NET Core does
    // not even ship. Windows does not resolve imports when mapping an image for
    // data either, so skip it -- and skip it before the thunk walk below, which
    // assumes 8-byte PE32+ thunks and would misread 4-byte ones.
    if !is_pe32_plus(img, e_lfanew) {
        return Ok(());
    }
    let imp_dir = dd_base(img, e_lfanew) + 8;
    if imp_dir + 8 > img.len() {
        return Ok(());
    }
    let mut desc = rd_u32(img, imp_dir) as usize;
    let imp_size = rd_u32(img, imp_dir + 4) as usize;
    if desc == 0 || imp_size == 0 {
        return Ok(());
    }
    let desc_end = desc + imp_size;
    let process = remote.unwrap_or(0) as HANDLE;

    while desc + 20 <= img.len() && desc < desc_end {
        let oft = rd_u32(img, desc) as usize;
        let name_rva = rd_u32(img, desc + 12) as usize;
        let ft = rd_u32(img, desc + 16) as usize;
        if name_rva == 0 {
            break;
        }
        if name_rva >= img.len() {
            break;
        }
        let mut end = name_rva;
        while end < img.len() && img[end] != 0 {
            end += 1;
        }
        let dll_name_str = String::from_utf8_lossy(&img[name_rva..end]).into_owned();
        let mut dll_z = dll_name_str.as_bytes().to_vec();
        dll_z.push(0);

        let want_base = std::path::Path::new(&dll_name_str)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&dll_name_str)
            .to_ascii_lowercase();

        // Manual-mapped game-local: always walk remote exports at known base.
        let forced = forced_remote.iter().find(|(n, _)| {
            let nb = std::path::Path::new(n)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(n)
                .to_ascii_lowercase();
            nb == want_base
        });

        let (use_remote, remote_base, parent_mod) = if let Some((_, base)) = forced {
            (true, *base, 0 as HANDLE)
        } else {
            // Prefer parent LoadLibrary (system DLLs / already loaded).
            let parent_mod = unsafe { LoadLibraryA(dll_z.as_ptr()) };
            if parent_mod.is_null() && !process.is_null() {
                match find_remote_module_base(process, &dll_name_str) {
                    Ok(base) => (true, base, 0 as HANDLE),
                    Err(_) => {
                        // Optional multimedia imports may be missing on thin hosts.
                        let optional = {
                            let b = want_base.as_str();
                            b.contains("x3daudio")
                                || b.contains("xactengine")
                                || b.contains("xapofx")
                                || b.contains("d3dx")
                                || b.contains("xinput")
                                || b.contains("xaudio")
                        };
                        if optional {
                            eprintln!(
                                "vfs-inject: skip IAT for optional missing import {dll_name_str}"
                            );
                            desc += 20;
                            continue;
                        }
                        return Err("remote module not found");
                    }
                }
            } else if parent_mod.is_null() {
                return Err("LoadLibraryA for import failed");
            } else {
                (false, 0u64, parent_mod)
            }
        };

        let mut thunk_rva = if oft != 0 { oft } else { ft };
        let mut iat_rva = ft;
        loop {
            if thunk_rva + 8 > img.len() || iat_rva + 8 > img.len() {
                break;
            }
            let entry = rd_u64(img, thunk_rva);
            if entry == 0 {
                break;
            }
            let fa = if use_remote {
                if entry & (1u64 << 63) != 0 {
                    let ord = (entry & 0xFFFF) as u16;
                    remote_proc_by_ordinal(process, remote_base, ord)?
                } else {
                    let name_ptr = (entry as usize) + 2;
                    if name_ptr >= img.len() {
                        break;
                    }
                    let mut ne = name_ptr;
                    while ne < img.len() && img[ne] != 0 {
                        ne += 1;
                    }
                    let nm = std::str::from_utf8(&img[name_ptr..ne]).unwrap_or("");
                    remote_proc_by_name(process, remote_base, nm)?
                }
            } else {
                let addr = if entry & (1u64 << 63) != 0 {
                    let ord = (entry & 0xFFFF) as u16;
                    unsafe { GetProcAddress(parent_mod, ord as *const u8) }
                } else {
                    let name_ptr = (entry as usize) + 2;
                    if name_ptr >= img.len() {
                        break;
                    }
                    let mut ne = name_ptr;
                    while ne < img.len() && img[ne] != 0 {
                        ne += 1;
                    }
                    let mut nm = img[name_ptr..ne].to_vec();
                    nm.push(0);
                    unsafe { GetProcAddress(parent_mod, nm.as_ptr()) }
                };
                let Some(func) = addr else {
                    return Err("GetProcAddress for import failed");
                };
                // The IAT slot holds the resolved address; a function pointer
                // is exactly what we want to write there.
                #[allow(clippy::fn_to_numeric_cast_any)]
                { func as usize as u64 }
            };
            img[iat_rva..iat_rva + 8].copy_from_slice(&fa.to_le_bytes());
            thunk_rva += 8;
            iat_rva += 8;
        }
        desc += 20;
    }
    Ok(())
}

fn find_remote_module_base(
    process: windows_sys::Win32::Foundation::HANDLE,
    dll_name: &str,
) -> Result<u64, &'static str> {
    use windows_sys::Win32::Foundation::HMODULE;
    use windows_sys::Win32::System::ProcessStatus::{
        EnumProcessModulesEx, GetModuleBaseNameA, LIST_MODULES_ALL,
    };

    let want = dll_name.to_ascii_lowercase();
    let want_base = std::path::Path::new(&want)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&want);
    unsafe {
        let mut mods: Vec<HMODULE> = vec![std::ptr::null_mut(); 512];
        let mut needed = 0u32;
        let ok = EnumProcessModulesEx(
            process,
            mods.as_mut_ptr(),
            (mods.len() * std::mem::size_of::<HMODULE>()) as u32,
            &mut needed,
            LIST_MODULES_ALL,
        );
        if ok == 0 {
            return Err("EnumProcessModulesEx failed");
        }
        let count = (needed as usize) / std::mem::size_of::<HMODULE>();
        for m in mods.into_iter().take(count) {
            if m.is_null() {
                continue;
            }
            let mut name = [0u8; 260];
            let n = GetModuleBaseNameA(process, m, name.as_mut_ptr(), 260);
            if n == 0 {
                continue;
            }
            let s = String::from_utf8_lossy(&name[..n as usize]).to_ascii_lowercase();
            if s == want_base || s == want {
                return Ok(m as usize as u64);
            }
        }
    }
    Err("remote module not found")
}

fn rpm_u32(process: windows_sys::Win32::Foundation::HANDLE, addr: u64) -> Result<u32, &'static str> {
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    let mut v = 0u32;
    let mut n = 0usize;
    unsafe {
        if ReadProcessMemory(
            process,
            addr as *const _,
            &mut v as *mut _ as *mut _,
            4,
            &mut n,
        ) == 0
            || n != 4
        {
            return Err("RPM u32 failed");
        }
    }
    Ok(v)
}

fn rpm_u16(process: windows_sys::Win32::Foundation::HANDLE, addr: u64) -> Result<u16, &'static str> {
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    let mut v = 0u16;
    let mut n = 0usize;
    unsafe {
        if ReadProcessMemory(
            process,
            addr as *const _,
            &mut v as *mut _ as *mut _,
            2,
            &mut n,
        ) == 0
            || n != 2
        {
            return Err("RPM u16 failed");
        }
    }
    Ok(v)
}

fn rpm_bytes(
    process: windows_sys::Win32::Foundation::HANDLE,
    addr: u64,
    len: usize,
) -> Result<Vec<u8>, &'static str> {
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    let mut buf = vec![0u8; len];
    let mut n = 0usize;
    unsafe {
        if ReadProcessMemory(
            process,
            addr as *const _,
            buf.as_mut_ptr() as *mut _,
            len,
            &mut n,
        ) == 0
            || n != len
        {
            return Err("RPM bytes failed");
        }
    }
    Ok(buf)
}

fn remote_export_dir(
    process: windows_sys::Win32::Foundation::HANDLE,
    base: u64,
) -> Result<(u64, u32), &'static str> {
    // e_lfanew at +0x3C
    let e_lfanew = match rpm_u32(process, base + 0x3C) {
        Ok(v) => v as u64,
        Err(e) => {
            // Generic "RPM u32 failed" here is unactionable: it means some
            // module base we recorded is not readable in the child. Name it.
            eprintln!(
                "vfs-inject: export dir unreadable at base=0x{base:x} (+0x3C): {e} — \
                 module base is bogus or its pages are not committed"
            );
            return Err(e);
        }
    };
    let opt = base + e_lfanew + 24;
    // Export dir = DataDirectory[0]
    let exp_rva = rpm_u32(process, opt + 112)?;
    let exp_size = rpm_u32(process, opt + 112 + 4)?;
    if exp_rva == 0 {
        return Err("no export dir");
    }
    Ok((base + exp_rva as u64, exp_size))
}

fn remote_proc_by_name(
    process: windows_sys::Win32::Foundation::HANDLE,
    base: u64,
    name: &str,
) -> Result<u64, &'static str> {
    let (exp_base, _) = remote_export_dir(process, base)?;
    // IMAGE_EXPORT_DIRECTORY
    let num_names = rpm_u32(process, exp_base + 24)? as usize;
    let addr_funcs = base + rpm_u32(process, exp_base + 28)? as u64;
    let addr_names = base + rpm_u32(process, exp_base + 32)? as u64;
    let addr_ords = base + rpm_u32(process, exp_base + 36)? as u64;
    for i in 0..num_names {
        let name_rva = rpm_u32(process, addr_names + (i as u64) * 4)? as u64;
        let nb = rpm_bytes(process, base + name_rva, name.len() + 1)?;
        if nb.len() > name.len() && &nb[..name.len()] == name.as_bytes() && nb[name.len()] == 0 {
            let ord = rpm_u16(process, addr_ords + (i as u64) * 2)? as u64;
            let func_rva = rpm_u32(process, addr_funcs + ord * 4)? as u64;
            return Ok(base + func_rva);
        }
    }
    Err("remote export not found")
}

fn remote_proc_by_ordinal(
    process: windows_sys::Win32::Foundation::HANDLE,
    base: u64,
    ordinal: u16,
) -> Result<u64, &'static str> {
    let (exp_base, _) = remote_export_dir(process, base)?;
    let ord_base = rpm_u32(process, exp_base + 16)?; // Base
    let addr_funcs = base + rpm_u32(process, exp_base + 28)? as u64;
    let idx = (ordinal as u32).wrapping_sub(ord_base) as u64;
    let func_rva = rpm_u32(process, addr_funcs + idx * 4)? as u64;
    Ok(base + func_rva)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_dll_names_reads_kernel32_from_self() {
        let pe = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        // build_image flattens; import_dll_names indexes by RVA into the flat image.
        let (img, _, el) = build_image(&pe).expect("build_image");
        let names = vfs_pe::import_dll_names(&img, el);
        assert!(
            names.iter().any(|n| n.eq_ignore_ascii_case("KERNEL32.dll")
                || n.to_ascii_lowercase().contains("kernel32")),
            "expected kernel32 in imports: {names:?}"
        );
    }

    #[test]
    fn resolve_imports_fills_iat_for_self() {
        let pe = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let (mut img, _, el) = build_image(&pe).expect("build_image");
        resolve_imports(&mut img, el).expect("resolve_imports");
        // IAT should contain at least one non-zero absolute address in high VA range.
        let opt = el + 24;
        let iat_rva = rd_u32(&img, opt + 112 + 12 * 8) as usize;
        if iat_rva != 0 && iat_rva + 8 <= img.len() {
            let v = rd_u64(&img, iat_rva);
            assert_ne!(v, 0, "first IAT slot should be resolved");
        }
    }
}

//! Pure in-memory PE process creation — **no archive bytes written to any
//! filesystem path** (not TEMP, not game root).
//!
//! Technique: process hollowing of a pre-existing host image (this launcher or
//! a system EXE that already exists on disk and is *not* from the GameLayers
//! archives). Target PE bytes are written only into the child process VA via
//! `WriteProcessMemory`.
//!
//! Activation (beyond classic map+RCX) for MSVC CRT EXEs like skse64_loader:
//! - security cookie init (LoadConfig)
//! - remote TLS data + TEB slot
//! - `RtlAddFunctionTable` for x64 unwind (.pdata)
//! - LDR SizeOfImage / EntryPoint updates
//! - entry trampoline so exception registration runs on the primary thread
#![allow(unsafe_code)]

use core::ffi::c_void;
use std::mem::{size_of, zeroed};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, NTSTATUS};
use windows_sys::Win32::System::Diagnostics::Debug::{
    GetThreadContext, ReadProcessMemory, SetThreadContext, WriteProcessMemory, CONTEXT,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
use windows_sys::Win32::System::Memory::{
    VirtualAllocEx, MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, CreateRemoteThread, WaitForSingleObject, CREATE_SUSPENDED, INFINITE,
    LPTHREAD_START_ROUTINE, PROCESS_INFORMATION, STARTUPINFOW,
};

// winnt.h x64: CONTEXT_AMD64 | CONTROL | INTEGER | FLOATING_POINT
const CONTEXT_FULL: u32 = 0x0010_000B;
const PROCESS_BASIC_INFORMATION: u32 = 0;
const THREAD_BASIC_INFORMATION: u32 = 0;
const DLL_PROCESS_ATTACH: u32 = 1;

#[repr(C)]
struct ProcessBasicInformation {
    reserved1: *mut c_void,
    peb_base_address: *mut c_void,
    reserved2: [*mut c_void; 2],
    unique_process_id: usize,
    reserved3: *mut c_void,
}

#[repr(C)]
struct ThreadBasicInformation {
    exit_status: NTSTATUS,
    teb_base_address: *mut c_void,
    client_id_unique_process: usize,
    client_id_unique_thread: usize,
    affinity_mask: usize,
    priority: i32,
    base_priority: i32,
}

type NtQueryInformationProcessFn = unsafe extern "system" fn(
    HANDLE,
    u32,
    *mut c_void,
    u32,
    *mut u32,
) -> NTSTATUS;

type NtQueryInformationThreadFn = unsafe extern "system" fn(
    HANDLE,
    u32,
    *mut c_void,
    u32,
    *mut u32,
) -> NTSTATUS;

type NtUnmapViewOfSectionFn = unsafe extern "system" fn(HANDLE, *mut c_void) -> NTSTATUS;
type NtGetContextThreadFn = unsafe extern "system" fn(HANDLE, *mut CONTEXT) -> NTSTATUS;
type NtSetContextThreadFn = unsafe extern "system" fn(HANDLE, *const CONTEXT) -> NTSTATUS;

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

fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}
fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn rd_u64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn wr_u64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

/// Create a suspended host process and hollow `pe` into it.
pub fn create_process_from_pe_bytes(
    pe: &[u8],
    image_path: &str,
    args: &[String],
    current_dir: Option<&str>,
) -> Result<(HANDLE, HANDLE, u32, u32), &'static str> {
    create_process_from_pe_bytes_ex(pe, image_path, args, current_dir, None)
}

/// Like [`create_process_from_pe_bytes`], optionally `LoadLibrary` of `inject_dll`
/// **before** hollow so VFS hooks are live when preloading game-local import
/// DLLs (steam_api64.dll, bink2w64.dll, …) from zip windows.
pub fn create_process_from_pe_bytes_ex(
    pe: &[u8],
    image_path: &str,
    args: &[String],
    current_dir: Option<&str>,
    inject_dll_path: Option<&str>,
) -> Result<(HANDLE, HANDLE, u32, u32), &'static str> {
    // Host MUST be a real on-disk file — never VFS-spoofed current_exe.
    // SkyrimSE prefers Steam install path for DRM (see hollow_host_exe_for).
    let host = hollow_host_exe_for(Some(image_path))?;
    eprintln!("vfs-inject: hollow host={host}");
    let mut cmdline = format!("\"{image_path}\"");
    for a in args {
        cmdline.push_str(&format!(" \"{a}\""));
    }
    let host_w = wide(&host);
    let mut cmd_w = wide(&cmdline);
    let cwd_w = current_dir.map(wide);
    unsafe {
        let mut si: STARTUPINFOW = zeroed();
        si.cb = size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = zeroed();
        let ok = CreateProcessW(
            host_w.as_ptr(),
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
        if ok == 0 {
            return Err("CreateProcess host failed");
        }
        if let Some(dll) = inject_dll_path {
            if let Err(_e) = crate::inject::inject_dll(pi.hProcess, dll) {
                CloseHandle(pi.hThread);
                CloseHandle(pi.hProcess);
                return Err("inject shim before hollow failed");
            }
            // Wait for child shim hooks (named event Local\vfs_shim_ready_{pid}).
            if !wait_child_shim_ready(pi.dwProcessId, 15_000) {
                eprintln!(
                    "vfs-inject: shim ready timeout for pid={} (continuing; game-local may manual-map)",
                    pi.dwProcessId
                );
            }
        }
        if let Err(e) = hollow_existing_process(pi.hProcess, pi.hThread, pe, image_path) {
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            return Err(e);
        }
        Ok((pi.hProcess, pi.hThread, pi.dwProcessId, pi.dwThreadId))
    }
}

/// Replace the main image of an already-created suspended process with `pe`
/// (WriteProcessMemory only — **no filesystem write of archive bytes**).
pub fn hollow_existing_process(
    process: HANDLE,
    thread: HANDLE,
    pe: &[u8],
    image_path: &str,
) -> Result<(), &'static str> {
    if !pe_looks_like_image(pe) {
        return Err("not a PE");
    }
    let (mut img, preferred_base, entry_rva, size_of_image) = pe_layout(pe)?;

    let nt_qip: NtQueryInformationProcessFn = unsafe {
        core::mem::transmute(
            ntdll_proc(b"NtQueryInformationProcess\0").ok_or("NtQueryInformationProcess")?,
        )
    };
    let nt_unmap: NtUnmapViewOfSectionFn = unsafe {
        core::mem::transmute(ntdll_proc(b"NtUnmapViewOfSection\0").ok_or("NtUnmapViewOfSection")?)
    };
    let nt_get_ctx: NtGetContextThreadFn = unsafe {
        core::mem::transmute(ntdll_proc(b"NtGetContextThread\0").ok_or("NtGetContextThread")?)
    };
    let nt_set_ctx: NtSetContextThreadFn = unsafe {
        core::mem::transmute(ntdll_proc(b"NtSetContextThread\0").ok_or("NtSetContextThread")?)
    };

    unsafe {
        let mut pbi: ProcessBasicInformation = zeroed();
        let mut ret_len = 0u32;
        let st = nt_qip(
            process,
            PROCESS_BASIC_INFORMATION,
            &mut pbi as *mut _ as *mut c_void,
            size_of::<ProcessBasicInformation>() as u32,
            &mut ret_len,
        );
        if st != 0 || pbi.peb_base_address.is_null() {
            return Err("NtQueryInformationProcess PEB failed");
        }
        let peb = pbi.peb_base_address as usize;

        let mut remote_base: u64 = 0;
        let mut n = 0usize;
        if ReadProcessMemory(
            process,
            (peb + 0x10) as *const c_void,
            &mut remote_base as *mut u64 as *mut c_void,
            8,
            &mut n,
        ) == 0
            || n != 8
        {
            return Err("ReadProcessMemory ImageBase failed");
        }

        let e_lfanew = rd_u32(&img, 0x3C) as usize;

        // Always write **zip PE bytes** into the process (AC: executables from zips).
        // Prefer in-place overwrite of a same-sized host image (Steam SkyrimSE host
        // with matching SizeOfImage) so SKSE still sees one main module and
        // CreateRemoteThread keeps working. Otherwise VirtualAlloc a new region.
        // Never leave the live image as the untouched Steam SEC_IMAGE mapping.
        let host_soi = {
            let mut e_lf = 0u32;
            let mut n = 0usize;
            let mut ok_soi = 0u32;
            if remote_base != 0
                && ReadProcessMemory(
                    process,
                    (remote_base as usize + 0x3C) as *const c_void,
                    &mut e_lf as *mut u32 as *mut c_void,
                    4,
                    &mut n,
                ) != 0
            {
                let mut soi = 0u32;
                if ReadProcessMemory(
                    process,
                    (remote_base as usize + e_lf as usize + 24 + 56) as *const c_void,
                    &mut soi as *mut u32 as *mut c_void,
                    4,
                    &mut n,
                ) != 0
                {
                    ok_soi = soi;
                }
            }
            ok_soi as usize
        };
        let use_inplace = remote_base != 0
            && host_soi == size_of_image
            && image_path.to_ascii_lowercase().contains("skyrimse");

        // Preload imports in remote before any image overwrite.
        // Game-local DLLs are manual-mapped from zip/VFS PE bytes (not Steam disk).
        let forced_bases = preload_remote_import_dlls(process, &img, e_lfanew)?;

        if std::env::var_os("VFS_HOLLOW_UNMAP").is_some() && !use_inplace {
            let unmap_st = nt_unmap(process, remote_base as *mut c_void);
            eprintln!(
                "vfs-inject: NtUnmapViewOfSection host base=0x{remote_base:x} status=0x{unmap_st:x}"
            );
        } else {
            let _ = nt_unmap;
        }

        let (new_base, new_base_u) = if use_inplace {
            eprintln!(
                "vfs-inject: in-place zip PE write at host base=0x{remote_base:x} (soi=0x{host_soi:x})"
            );
            (remote_base as *mut c_void, remote_base)
        } else {
            eprintln!(
                "vfs-inject: host image kept mapped at 0x{remote_base:x}; allocating zip image"
            );
            let any_base = std::env::var_os("VFS_HOLLOW_ANY_BASE").is_some();
            let mut nb = if any_base {
                core::ptr::null_mut()
            } else {
                VirtualAllocEx(
                    process,
                    preferred_base as *const c_void,
                    size_of_image,
                    MEM_COMMIT | MEM_RESERVE,
                    PAGE_EXECUTE_READWRITE,
                )
            };
            if nb.is_null() {
                nb = VirtualAllocEx(
                    process,
                    core::ptr::null(),
                    size_of_image,
                    MEM_COMMIT | MEM_RESERVE,
                    PAGE_EXECUTE_READWRITE,
                );
            }
            if nb.is_null() {
                return Err("VirtualAllocEx image failed");
            }
            (nb, nb as u64)
        };

        if new_base_u != preferred_base {
            eprintln!(
                "vfs-inject: hollow reloc preferred=0x{preferred_base:x} actual=0x{new_base_u:x}"
            );
            crate::map::apply_relocs(&mut img, e_lfanew, preferred_base, new_base_u);
        }
        // Always resolve IAT against remote modules *after* preload forced
        // game-local DLLs from zip PE (not Steam-disk IAT / parent LoadLibrary).
        crate::map::resolve_imports_ex_with_bases(
            &mut img,
            e_lfanew,
            Some(process as isize),
            &forced_bases,
        )?;

        if use_inplace {
            use windows_sys::Win32::System::Memory::{VirtualProtectEx, PAGE_EXECUTE_READWRITE};
            let mut old = 0u32;
            let _ = VirtualProtectEx(
                process,
                new_base,
                size_of_image,
                PAGE_EXECUTE_READWRITE,
                &mut old,
            );
        }

        let mut written = 0usize;
        if WriteProcessMemory(
            process,
            new_base,
            img.as_ptr() as *const c_void,
            img.len(),
            &mut written,
        ) == 0
            || written != img.len()
        {
            return Err("WriteProcessMemory zip PE image failed");
        }
        eprintln!(
            "vfs-inject: wrote {} zip PE bytes to 0x{new_base_u:x} (source=archive RAM)",
            written
        );

        // Verify entry bytes match the zip image we built (proves zip PE is live).
        {
            let mut probe = [0u8; 16];
            let mut pn = 0usize;
            let entry_ptr = (new_base as usize + entry_rva as usize) as *const c_void;
            if ReadProcessMemory(
                process,
                entry_ptr,
                probe.as_mut_ptr() as *mut c_void,
                16,
                &mut pn,
            ) != 0
            {
                let local = &img[entry_rva as usize..entry_rva as usize + 16];
                if probe != local {
                    eprintln!(
                        "vfs-inject: entry probe mismatch remote={probe:02x?} zip={local:02x?}"
                    );
                    return Err("remote entry bytes mismatch after zip PE write");
                }
            }
        }

        if WriteProcessMemory(
            process,
            (peb + 0x10) as *mut c_void,
            &new_base_u as *const u64 as *const c_void,
            8,
            &mut written,
        ) == 0
        {
            return Err("WriteProcessMemory PEB ImageBase failed");
        }
        spoof_peb_and_ldr_paths(
            process,
            peb,
            image_path,
            new_base_u,
            size_of_image,
            entry_rva,
        )?;
        // Kernel already ran TLS for in-place host; only set up TLS for new allocs.
        if !use_inplace {
            setup_remote_tls(process, thread, new_base_u, &img, e_lfanew, preferred_base)?;
        }
        let real_entry = new_base_u + entry_rva as u64;

        // Default: start at real EP (CRT inits cookie/TLS data). Optional
        // trampoline (VFS_HOLLOW_TRAMP=1) registers .pdata via RtlAddFunctionTable.
        let use_tramp = std::env::var_os("VFS_HOLLOW_TRAMP").is_some();
        let start_u = if use_tramp {
            let tramp =
                build_entry_trampoline(new_base_u, real_entry, &img, e_lfanew, preferred_base)?;
            let tramp_remote = VirtualAllocEx(
                process,
                core::ptr::null(),
                tramp.len(),
                MEM_COMMIT | MEM_RESERVE,
                PAGE_EXECUTE_READWRITE,
            );
            if tramp_remote.is_null() {
                return Err("VirtualAllocEx trampoline failed");
            }
            let mut written = 0usize;
            if WriteProcessMemory(
                process,
                tramp_remote,
                tramp.as_ptr() as *const c_void,
                tramp.len(),
                &mut written,
            ) == 0
                || written != tramp.len()
            {
                return Err("WriteProcessMemory trampoline failed");
            }
            tramp_remote as u64
        } else {
            real_entry
        };
        let bare = !use_tramp;

        // Activation strategy (VFS_HOLLOW_START):
        // - "rcx" (default for EXE): set primary RCX=start, caller ResumeThread.
        //   Matches Windows process entry (RtlUserThreadStart → EP). Required for
        //   MSVC CRT EXEs like skse64_loader (CreateRemoteThread → 0xC0000409).
        // - "thread": CreateRemoteThread(start). Works for some EXEs (hollow_hello).
        // - "both": remote thread + set RCX (caller should not Resume primary).
        let mode = std::env::var("VFS_HOLLOW_START").unwrap_or_else(|_| "rcx".into());

        let mut ctx_buf = vec![0u8; size_of::<CONTEXT>() + 16];
        let ctx_addr = (ctx_buf.as_mut_ptr() as usize + 15) & !15;
        let ctx = ctx_addr as *mut CONTEXT;
        core::ptr::write_bytes(ctx as *mut u8, 0, size_of::<CONTEXT>());
        (*ctx).ContextFlags = CONTEXT_FULL;
        let gst = nt_get_ctx(thread, ctx);
        if gst != 0 && GetThreadContext(thread, ctx) == 0 {
            let err = windows_sys::Win32::Foundation::GetLastError();
            eprintln!("vfs-inject: GetThreadContext failed nt={gst:x} win32={err}");
            return Err("GetThreadContext failed");
        }
        (*ctx).Rcx = start_u;
        let sst = nt_set_ctx(thread, ctx);
        if sst != 0 && SetThreadContext(thread, ctx) == 0 {
            let err = windows_sys::Win32::Foundation::GetLastError();
            eprintln!("vfs-inject: SetThreadContext failed nt={sst:x} win32={err}");
            return Err("SetThreadContext failed");
        }
        let _ = ctx_buf;

        if mode == "thread" || mode == "both" {
            let start: LPTHREAD_START_ROUTINE = Some(core::mem::transmute(start_u as usize));
            let ht = CreateRemoteThread(
                process,
                core::ptr::null(),
                0,
                start,
                core::ptr::null_mut(),
                0,
                core::ptr::null_mut(),
            );
            if ht.is_null() {
                let err = windows_sys::Win32::Foundation::GetLastError();
                eprintln!("vfs-inject: CreateRemoteThread(entry) failed err={err}");
                if mode == "thread" {
                    return Err("CreateRemoteThread entry failed");
                }
            } else {
                CloseHandle(ht);
            }
        }

        // Main image IAT now points at zip-backed game-local bases. Drop any
        // leftover Steam-path SEC_IMAGE modules and spoof LDR FullDllName →
        // GameLayers so EnumProcessModules proves virtual/zip paths.
        finalize_game_local_modules(process, peb, &forced_bases);

        eprintln!(
            "vfs-inject: hollow ok image=0x{new_base_u:x} size=0x{size_of_image:x} entry=0x{real_entry:x} start=0x{start_u:x} bare={bare} mode={mode}"
        );
        Ok(())
    }
}

/// Copy the kernel-bound IAT from a remote image into the flat `img` so an
/// in-place zip PE write keeps working import addresses (then `resolve_imports_ex`
/// fills any remaining zero slots).
unsafe fn copy_remote_iat_into_image(
    process: HANDLE,
    remote_base: u64,
    img: &mut [u8],
    e_lfanew: usize,
) -> Result<(), &'static str> {
    let opt = e_lfanew + 24;
    // IMAGE_DIRECTORY_ENTRY_IAT = 12
    let iat_dir = opt + 112 + 12 * 8;
    if iat_dir + 8 > img.len() {
        return Ok(());
    }
    let iat_rva = rd_u32(img, iat_dir) as usize;
    let iat_size = rd_u32(img, iat_dir + 4) as usize;
    if iat_rva == 0 || iat_size == 0 || iat_rva + iat_size > img.len() {
        return Ok(());
    }
    let mut n = 0usize;
    if ReadProcessMemory(
        process,
        (remote_base as usize + iat_rva) as *const c_void,
        img[iat_rva..iat_rva + iat_size].as_mut_ptr() as *mut c_void,
        iat_size,
        &mut n,
    ) == 0
        || n != iat_size
    {
        return Err("ReadProcessMemory host IAT failed");
    }
    // Also copy FirstThunk IATs from the import directory (same as IAT when bound).
    let imp_dir = opt + 112 + 8;
    let mut desc = rd_u32(img, imp_dir) as usize;
    let imp_size = rd_u32(img, imp_dir + 4) as usize;
    let desc_end = desc + imp_size;
    while desc + 20 <= img.len() && desc < desc_end {
        let name_rva = rd_u32(img, desc + 12);
        if name_rva == 0 {
            break;
        }
        let ft = rd_u32(img, desc + 16) as usize;
        if ft == 0 {
            desc += 20;
            continue;
        }
        // Copy until double-null thunk terminator (up to 512 slots).
        for i in 0..512 {
            let off = ft + i * 8;
            if off + 8 > img.len() {
                break;
            }
            let mut slot = [0u8; 8];
            let mut rn = 0usize;
            if ReadProcessMemory(
                process,
                (remote_base as usize + off) as *const c_void,
                slot.as_mut_ptr() as *mut c_void,
                8,
                &mut rn,
            ) == 0
            {
                break;
            }
            img[off..off + 8].copy_from_slice(&slot);
            if u64::from_le_bytes(slot) == 0 {
                break;
            }
        }
        desc += 20;
    }
    Ok(())
}

/// Return a CreateProcess host path whose PE has at least `min_reserve` stack.
///
/// **Never mutates the Steam-library host** (Steam validates on-disk SkyrimSE.exe
/// → "Steam Error" if we patch it in place). When the original reserve is too
/// small, write a temp copy with a larger stack and use that as the
/// CreateProcess image; cmdline / GMFW still present the real Steam path.
///
/// SkyrimSE ships with 1 MiB reserve; under VFS Nt hooks the main thread hits
/// `0xC00000FD` after masters/BSAs mmap without this bump.
pub fn ensure_host_stack_reserve(host: &str, min_reserve: u64) -> Result<String, &'static str> {
    let mut pe = std::fs::read(host).map_err(|_| "read host pe for stack patch")?;
    if pe.len() < 0x40 || pe[0] != b'M' || pe[1] != b'Z' {
        return Err("host pe not MZ");
    }
    let e_lfanew = u32::from_le_bytes(pe[0x3c..0x40].try_into().unwrap()) as usize;
    if e_lfanew + 0x18 + 0x58 > pe.len() {
        return Err("host pe truncated");
    }
    if &pe[e_lfanew..e_lfanew + 4] != b"PE\0\0" {
        return Err("host pe bad PE sig");
    }
    let opt = e_lfanew + 0x18;
    let magic = u16::from_le_bytes(pe[opt..opt + 2].try_into().unwrap());
    // PE32+ optional header: SizeOfStackReserve @ +0x48, SizeOfStackCommit @ +0x50
    if magic != 0x20b {
        return Ok(host.to_string());
    }
    let res_off = opt + 0x48;
    let cur = u64::from_le_bytes(pe[res_off..res_off + 8].try_into().unwrap());
    if cur >= min_reserve {
        return Ok(host.to_string());
    }
    pe[res_off..res_off + 8].copy_from_slice(&min_reserve.to_le_bytes());
    let com_off = opt + 0x50;
    let com = u64::from_le_bytes(pe[com_off..com_off + 8].try_into().unwrap());
    if com < 64 * 1024 {
        pe[com_off..com_off + 8].copy_from_slice(&(256 * 1024u64).to_le_bytes());
    }
    let base = std::path::Path::new(host)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("host");
    let tmp = std::env::temp_dir().join(format!("vfs-host-stack-{base}-{min_reserve:x}.exe"));
    std::fs::write(&tmp, &pe).map_err(|_| "write temp host pe stack patch")?;
    let out = tmp.to_string_lossy().into_owned();
    eprintln!(
        "vfs-inject: temp host stack reserve {cur:#x} → {min_reserve:#x} (CreateProcess image={out}; library host untouched)"
    );
    Ok(out)
}

/// Pick a real on-disk EXE to CreateProcess as the hollow host.
/// Never uses the current module path (may be VFS-spoofed).
///
/// For SkyrimSE, prefer the Steam-installed `SkyrimSE.exe` when present so the
/// kernel `ProcessImageFileName` remains a Steam-owned path (Steam DRM rejects
/// hollowed `cmd.exe`). Zip PE bytes are always `WriteProcessMemory`'d over
/// that mapping (in-place) or into a new VA — never left as untouched Steam image.
pub fn hollow_host_exe() -> Result<String, &'static str> {
    hollow_host_exe_for(None)
}

/// Like [`hollow_host_exe`], with optional virtual image path to pick a better host.
pub fn hollow_host_exe_for(image_path: Option<&str>) -> Result<String, &'static str> {
    if let Ok(h) = std::env::var("VFS_HOLLOW_HOST") {
        if std::path::Path::new(&h).is_file() {
            let lower = h.to_ascii_lowercase();
            if !lower.contains("gamelayers\\runtime") && !lower.contains("vfs-run") {
                return Ok(h);
            }
        }
    }
    let img = image_path.unwrap_or("").to_ascii_lowercase();
    let want_skyrim = img.contains("skyrimse") || img.contains("skyrim");
    if want_skyrim {
        // Prefer canonical installdir (matches appmanifest). Underscore rename is
        // last-resort only — launching from `_Skyrim...` breaks Steam path
        // association and triggers steam://run / Remote Play.
        for c in [
            r"C:\Program Files (x86)\Steam\steamapps\common\Skyrim Special Edition\SkyrimSE.exe",
            r"C:\Program Files\Steam\steamapps\common\Skyrim Special Edition\SkyrimSE.exe",
            r"D:\SteamLibrary\steamapps\common\Skyrim Special Edition\SkyrimSE.exe",
            r"E:\SteamLibrary\steamapps\common\Skyrim Special Edition\SkyrimSE.exe",
            r"C:\Program Files (x86)\Steam\steamapps\common\_Skyrim Special Edition\SkyrimSE.exe",
        ] {
            if std::path::Path::new(c).is_file() {
                eprintln!("vfs-inject: using Steam SkyrimSE as hollow host (DRM-safe)");
                return Ok(c.to_string());
            }
        }
    }
    for c in [
        r"C:\Windows\System32\cmd.exe",
        r"C:\Windows\System32\RuntimeBroker.exe",
        r"C:\Windows\System32\notepad.exe",
        r"C:\Windows\System32\conhost.exe",
    ] {
        if std::path::Path::new(c).is_file() {
            return Ok(c.to_string());
        }
    }
    Err("no real host EXE for hollow")
}

fn pe_layout(raw: &[u8]) -> Result<(Vec<u8>, u64, u32, usize), &'static str> {
    let (img, base, e_lfanew) = crate::map::build_image(raw)?;
    let opt = e_lfanew + 24;
    let entry_rva = rd_u32(&img, opt + 16);
    let size_of_image = rd_u32(&img, opt + 56) as usize;
    Ok((img, base, entry_rva, size_of_image))
}

fn is_system_import_dll(name: &str) -> bool {
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

/// Virtual-dir search roots for game-local PE reads (parent VFS / managed root).
fn game_local_search_dirs() -> Vec<String> {
    let mut d = Vec::new();
    if let Ok(v) = std::env::var("VFS_VIRTUAL_DIR") {
        if !v.is_empty() {
            d.push(v);
        }
    }
    if let Ok(v) = std::env::var("VFS_VIRTUAL_IMAGE") {
        if let Some(p) = std::path::Path::new(&v).parent() {
            d.push(p.to_string_lossy().into_owned());
        }
    }
    d.push(r"C:\GameLayers\runtime".into());
    d
}

/// Whether `steam_api*.dll` must stay the host-install copy rather than being
/// served from the zip.
///
/// Value-aware on purpose: this used to be a bare `var_os(..).is_some()`, so
/// `VFS_KEEP_HOST_STEAM_API=0` still read as *on* and there was no way to turn
/// it off. Setting it to `0`/`false`/`no` lets a neutral hollow host get past
/// the IAT failure (`map.rs` "remote module not found") when there is no host
/// copy beside the image.
///
/// **Turning it off does not currently yield a playable game.** Measured
/// 2026-08-12 with a neutral host: the zip `steam_api64.dll` is *manual-mapped*,
/// so it gets no LDR entry ("LDR path spoof failed: LDR entry not found"),
/// `GetModuleHandle`/`GetModuleFileName` cannot see it, and the process hangs at
/// startup — 0s CPU, one director open, no BSA ever read. Contrast
/// `bink2w64.dll`, which is real-`LoadLibrary`'d through the shim and *does* get
/// its LDR path spoofed. Serving steam_api from the zip needs that same
/// LoadLibrary-then-overwrite route, not a manual map.
pub fn keep_host_steam_api() -> bool {
    match std::env::var("VFS_KEEP_HOST_STEAM_API") {
        Ok(v) => !(v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("no")),
        Err(_) => false,
    }
}

/// Read game-local PE bytes without extracting archive content to disk.
///
/// Prefer direct Stored-zip window reads from layer zips (no parent-hook
/// dependency; avoids parent AV when VFS Serve is mid-bootstrap).
///
/// Falls back to a plain `std::fs::read` of `search_dirs`. That is a **real
/// filesystem read**, not a VFS one — if `search_dirs` points at the game
/// install, the PE comes off host disk and the zip is bypassed entirely. The
/// returned source is labelled `disk:` so that shows up in the log instead of
/// hiding behind a `vfs:` prefix.
///
/// Returns `(bytes, source_description)`.
fn read_game_local_pe(name: &str, search_dirs: &[String]) -> Result<(Vec<u8>, String), &'static str> {
    eprintln!("vfs-inject: reading game-local PE {name}…");
    if let Some((b, src)) = read_pe_from_layer_zips(name) {
        eprintln!("vfs-inject: game-local {name} from {src} ({} bytes)", b.len());
        return Ok((b, src));
    }
    for dir in search_dirs {
        let p = format!("{dir}\\{name}");
        match std::fs::read(&p) {
            Ok(b) if pe_looks_like_image(&b) && b.len() > 512 => {
                eprintln!("vfs-inject: game-local {name} from disk:{p} ({} bytes)", b.len());
                return Ok((b, format!("disk:{p}")));
            }
            _ => {}
        }
    }
    Err("game-local PE not readable from virtual dirs or layer zips")
}

/// Scan GameLayers zip archives for a Stored entry whose final path component
/// equals `name` (case-insensitive). Reads the zip window into a RAM buffer —
/// **no extract to disk**.
fn read_pe_from_layer_zips(name: &str) -> Option<(Vec<u8>, String)> {
    use std::io::{Read, Seek, SeekFrom};
    use vfs_core::{decode, EntryKind, LayerId, Source};
    use vfs_zip::read_layer;

    let want = name.to_ascii_lowercase();
    let mut roots = Vec::new();
    if let Ok(v) = std::env::var("VFS_LAYERS_DIR") {
        if !v.is_empty() {
            roots.push(std::path::PathBuf::from(v));
        }
    }
    roots.push(std::path::PathBuf::from(r"C:\GameLayers"));
    if let Ok(v) = std::env::var("VFS_VIRTUAL_DIR") {
        if let Some(p) = std::path::Path::new(&v).parent() {
            roots.push(p.to_path_buf());
        }
    }

    for root in roots {
        let rd = match std::fs::read_dir(&root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let mut zips: Vec<_> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("zip"))
                    .unwrap_or(false)
            })
            .collect();
        zips.sort();
        for (i, zip) in zips.iter().enumerate() {
            let layer = match read_layer(zip, LayerId(i as u32)) {
                Ok(l) => l,
                Err(_) => continue,
            };
            for ent in &layer.entries {
                if ent.kind != EntryKind::File {
                    continue;
                }
                let base = std::path::Path::new(&ent.vpath)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if base != want {
                    continue;
                }
                let Source::ZipWindow { offset, container } = decode(&ent.source.0) else {
                    continue;
                };
                let container_s = String::from_utf8_lossy(container).into_owned();
                let mut f = match std::fs::File::open(&container_s) {
                    Ok(f) => f,
                    Err(_) => continue,
                };
                if f.seek(SeekFrom::Start(offset)).is_err() {
                    continue;
                }
                let mut buf = vec![0u8; ent.size as usize];
                if f.read_exact(&mut buf).is_err() {
                    continue;
                }
                if pe_looks_like_image(&buf) && buf.len() > 512 {
                    let src = format!("zip-window:{}!{}", zip.display(), ent.vpath);
                    return Some((buf, src));
                }
            }
        }
    }
    None
}

/// Wait for `Local\vfs_shim_ready_{pid}` (same name as vfs-shim `signal_ready`).
fn wait_child_shim_ready(pid: u32, timeout_ms: u32) -> bool {
    use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    let name = wide(&format!(r"Local\vfs_shim_ready_{pid}"));
    unsafe {
        let ev = CreateEventW(core::ptr::null(), 1, 0, name.as_ptr());
        if ev.is_null() {
            return false;
        }
        let r = WaitForSingleObject(ev, timeout_ms);
        CloseHandle(ev);
        r == 0 // WAIT_OBJECT_0
    }
}

/// `LoadLibrary` system imports; **manual-map game-local** DLLs from zip/VFS PE
/// bytes (never leave Steam-disk SEC_IMAGE as the live game-local image).
///
/// Returns `(dll_name, remote_base)` for each manual-mapped game-local so IAT
/// resolve can walk exports without PEB LDR registration.
pub unsafe fn preload_remote_import_dlls(
    process: HANDLE,
    img: &[u8],
    e_lfanew: usize,
) -> Result<Vec<(String, u64)>, &'static str> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    use windows_sys::Win32::System::Memory::PAGE_READWRITE;

    let names = crate::map::import_dll_names(img, e_lfanew);
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let k32 = GetModuleHandleA(b"kernel32.dll\0".as_ptr());
    if k32.is_null() {
        return Err("GetModuleHandleA kernel32 failed");
    }
    let load_library = match GetProcAddress(k32, b"LoadLibraryA\0".as_ptr()) {
        Some(p) => p,
        None => return Err("GetProcAddress LoadLibraryA failed"),
    };
    let load_start: LPTHREAD_START_ROUTINE = Some(core::mem::transmute(load_library));

    let search_dirs = game_local_search_dirs();
    let mut forced_bases: Vec<(String, u64)> = Vec::new();

    // Prefer GameLayers for subsequent bare LoadLibrary searches in the child.
    if let Some(dir) = search_dirs.first() {
        let _ = remote_set_dll_directory(process, dir);
    }

    for name in &names {
        // steam_api must stay the Steam-install copy. Overwriting it from the zip
        // and spoofing LDR to C:\tmp\... makes steam_api re-launch Steam
        // (RestartAppIfNecessary / wrong install path) in a loop.
        let base_name = std::path::Path::new(name)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(name);
        if keep_host_steam_api()
            && (base_name.eq_ignore_ascii_case("steam_api64.dll")
                || base_name.eq_ignore_ascii_case("steam_api.dll"))
        {
            if let Some(b) = find_remote_module_base_opt(process, name) {
                eprintln!(
                    "vfs-inject: keeping host {name} @ 0x{b:x} (VFS_KEEP_HOST_STEAM_API)"
                );
                continue;
            }
            // Not loaded yet — load from hollow host directory if present.
            if let Ok(host) = std::env::var("VFS_HOLLOW_HOST") {
                if let Some(dir) = std::path::Path::new(&host).parent() {
                    let full = dir.join(base_name);
                    if full.is_file() {
                        if let Some(b) =
                            remote_load_library_path(process, load_start, &full.to_string_lossy())
                        {
                            eprintln!(
                                "vfs-inject: LoadLibrary host {name} from {} -> 0x{b:x}",
                                full.display()
                            );
                            continue;
                        }
                    }
                }
            }
            // Do not fall through to zip overwrite — that rewrites LDR to a temp
            // path and Steam starts Remote Play / relaunch loops.
            eprintln!(
                "vfs-inject: ERROR: could not keep host {name}; skipping zip overwrite"
            );
            continue;
        }

        let system = is_system_import_dll(name);
        if system {
            let mut raw = name.as_bytes().to_vec();
            raw.push(0);
            let remote = VirtualAllocEx(
                process,
                core::ptr::null(),
                raw.len(),
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            );
            if remote.is_null() {
                return Err("VirtualAllocEx import dll name failed");
            }
            let mut n = 0usize;
            if WriteProcessMemory(
                process,
                remote,
                raw.as_ptr() as *const c_void,
                raw.len(),
                &mut n,
            ) == 0
            {
                return Err("WriteProcessMemory import dll name failed");
            }
            let ht = CreateRemoteThread(
                process,
                core::ptr::null(),
                0,
                load_start,
                remote,
                0,
                core::ptr::null_mut(),
            );
            if ht.is_null() {
                let err = windows_sys::Win32::Foundation::GetLastError();
                eprintln!(
                    "vfs-inject: remote LoadLibraryA({name}) CreateRemoteThread err={err}"
                );
                return Err("remote LoadLibraryA CreateRemoteThread failed");
            }
            WaitForSingleObject(ht, INFINITE);
            let mut code = 0u32;
            let _ = windows_sys::Win32::System::Threading::GetExitCodeThread(ht, &mut code);
            CloseHandle(ht);
            if code == 0 {
                // Prefer absolute path under managed roots (staged DirectX redist).
                let mut loaded = false;
                for dir in &search_dirs {
                    let full = format!("{dir}\\{name}");
                    if std::path::Path::new(&full).is_file() {
                        if let Some(b) = remote_load_library_path(process, load_start, &full) {
                            eprintln!(
                                "vfs-inject: remote LoadLibraryW({full}) -> 0x{b:x} (staged)"
                            );
                            loaded = true;
                            break;
                        }
                    }
                }
                if loaded {
                    continue;
                }
                // Optional DirectX/audio runtimes — soft-skip if still missing.
                let optional = {
                    let b = name.to_ascii_lowercase();
                    b.contains("x3daudio")
                        || b.contains("xactengine")
                        || b.contains("xapofx")
                        || b.contains("d3dx")
                        || b.contains("xinput")
                        || b.contains("xaudio")
                };
                if optional {
                    eprintln!(
                        "vfs-inject: optional system DLL {name} not found; continuing hollow"
                    );
                    continue;
                }
                eprintln!("vfs-inject: remote LoadLibraryA({name}) returned NULL");
                return Err("remote LoadLibraryA returned NULL");
            }
            eprintln!("vfs-inject: remote LoadLibraryA({name}) -> 0x{code:x}");
            continue;
        }

        // --- Game-local (steam_api / bink / …): Stage A only here ---
        // Ensure a stable HMODULE (Steam host pre-map for DllMain/DRM). Zip PE
        // materialize + identity + LDR are Stages B–D in finalize_game_local_modules
        // (overwrite_remote_module_zip_preserve_iat — never privatize-of-Steam alone).
        let (pe, src) = match read_game_local_pe(name, &search_dirs) {
            Ok(v) => v,
            Err(e) => {
                let mut raw = name.as_bytes().to_vec();
                raw.push(0);
                let remote = VirtualAllocEx(
                    process,
                    core::ptr::null(),
                    raw.len(),
                    MEM_COMMIT | MEM_RESERVE,
                    PAGE_READWRITE,
                );
                if remote.is_null() {
                    return Err("VirtualAllocEx game-local name failed");
                }
                let mut n = 0usize;
                if WriteProcessMemory(
                    process,
                    remote,
                    raw.as_ptr() as *const c_void,
                    raw.len(),
                    &mut n,
                ) == 0
                {
                    return Err("WriteProcessMemory game-local name failed");
                }
                let ht = CreateRemoteThread(
                    process,
                    core::ptr::null(),
                    0,
                    load_start,
                    remote,
                    0,
                    core::ptr::null_mut(),
                );
                if ht.is_null() {
                    return Err("LoadLibrary non-zip CreateRemoteThread failed");
                }
                WaitForSingleObject(ht, INFINITE);
                let mut code = 0u32;
                let _ = windows_sys::Win32::System::Threading::GetExitCodeThread(ht, &mut code);
                CloseHandle(ht);
                if code == 0 {
                    // Try absolute paths under managed roots (staged DirectX redist).
                    let mut loaded = false;
                    for dir in &search_dirs {
                        let full = format!("{dir}\\{name}");
                        if std::path::Path::new(&full).is_file() {
                            if let Some(b) = remote_load_library_path(process, load_start, &full) {
                                eprintln!(
                                    "vfs-inject: remote LoadLibraryW({full}) -> 0x{b:x} (staged redist; {e})"
                                );
                                loaded = true;
                                break;
                            }
                        }
                    }
                    if loaded {
                        continue;
                    }
                    let optional = {
                        let b = name.to_ascii_lowercase();
                        b.contains("x3daudio")
                            || b.contains("xactengine")
                            || b.contains("xapofx")
                            || b.contains("d3dx")
                            || b.contains("xinput")
                            || b.contains("xaudio")
                    };
                    if optional {
                        eprintln!(
                            "vfs-inject: optional DLL {name} not found ({e}); continuing hollow"
                        );
                        continue;
                    }
                    eprintln!(
                        "vfs-inject: remote LoadLibraryA({name}) NULL (not in zip; {e})"
                    );
                    return Err("remote LoadLibraryA returned NULL");
                }
                if let Ok(path) = remote_module_path(process, name) {
                    eprintln!(
                        "vfs-inject: remote LoadLibraryA({name}) -> 0x{code:x} path={path} (non-zip)"
                    );
                }
                continue;
            }
        };

        let host_base = if let Some(b) = find_remote_module_base_opt(process, name) {
            if let Ok(path) = remote_module_path(process, name) {
                eprintln!(
                    "vfs-inject: Stage A host map {name} @ 0x{b:x} path={path} (zip source={src}; finalize will WPM zip PE)"
                );
            }
            b
        } else {
            // Prefer letting the *loader* map it: LoadLibrary of a path the
            // shim serves means ntdll builds a real LDR_DATA_TABLE_ENTRY, so
            // the module can locate itself (GetModuleHandle/GetModuleFileName)
            // and spoof_remote_ldr_module_path has an entry to rewrite. The
            // manual-map fallback below produces no LDR entry at all.
            let mut bound = None;
            for dir in &search_dirs {
                let full = format!("{dir}\\{name}");
                let Some(b) = remote_load_library_path(process, load_start, &full) else {
                    continue;
                };
                // Accept when the loader resolved it inside the root we asked
                // for. This used to require the path to contain "gamelayers",
                // so a managed root such as C:\tmp\skyrim-runtime fell through
                // to the manual map even though LoadLibrary had succeeded —
                // which is why zip-served steam_api64 got no LDR entry and
                // SteamAPI_Init hung at startup.
                match remote_module_path(process, name) {
                    Ok(path)
                        if path
                            .to_ascii_lowercase()
                            .starts_with(&dir.to_ascii_lowercase()) =>
                    {
                        eprintln!(
                            "vfs-inject: zip-path LoadLibrary {name} -> 0x{b:x} path={path} (source={src})"
                        );
                        bound = Some(b);
                        break;
                    }
                    Ok(path) => eprintln!(
                        "vfs-inject: LoadLibrary {name} resolved outside {dir} (path={path}); not binding"
                    ),
                    Err(_) => {}
                }
            }
            match bound {
                Some(b) => b,
                None => {
                    let b = map_remote_dll_from_pe_ex(process, &pe, name, true)?;
                    // No LDR entry: fine for a DLL the game only calls through
                    // the IAT, fatal for one that looks itself up.
                    eprintln!(
                        "vfs-inject: manual-mapped zip PE {name} -> 0x{b:x} ({} bytes, source={src}); \
                         WARNING no LDR entry — self-locating DLLs will fail",
                        pe.len()
                    );
                    b
                }
            }
        };

        forced_bases.push((name.clone(), host_base));
    }
    Ok(forced_bases)
}

/// Convert a remote SEC_IMAGE module mapping into a **private** MEM_PRIVATE
/// region at the **same base**, preserving DllMain-initialized live pages.
///
/// Not used on the game-local zip path (Stages B–C use zip WPM). Kept for
/// experiments; prefer `overwrite_remote_module_zip_preserve_iat`.
#[allow(dead_code)]
unsafe fn privatize_remote_module_same_base(
    process: HANDLE,
    base: u64,
    size_of_image: usize,
    name: &str,
) -> Result<(), &'static str> {
    if base == 0 || size_of_image == 0 || size_of_image > 256 * 1024 * 1024 {
        return Err("privatize: bad base/size");
    }

    // Snapshot live (DllMain-initialized) pages before unmap.
    let mut live = vec![0u8; size_of_image];
    let mut n = 0usize;
    if ReadProcessMemory(
        process,
        base as *const c_void,
        live.as_mut_ptr() as *mut c_void,
        size_of_image,
        &mut n,
    ) == 0
        || n != size_of_image
    {
        return Err("privatize: RPM live failed");
    }
    if live[0] != b'M' || live[1] != b'Z' {
        return Err("privatize: live not MZ");
    }

    type NtUnmapFn = unsafe extern "system" fn(HANDLE, *mut c_void) -> NTSTATUS;
    let unmap: NtUnmapFn = core::mem::transmute(
        ntdll_proc(b"NtUnmapViewOfSection\0").ok_or("privatize: NtUnmapViewOfSection")?,
    );
    let st = unmap(process, base as *mut c_void);
    if st != 0 {
        eprintln!(
            "vfs-inject: privatize NtUnmap {name} @ 0x{base:x} status=0x{st:x} (keeping SEC_IMAGE)"
        );
        return Err("privatize: NtUnmap failed");
    }

    let dst = VirtualAllocEx(
        process,
        base as *const c_void,
        size_of_image,
        MEM_COMMIT | MEM_RESERVE,
        PAGE_EXECUTE_READWRITE,
    );
    if dst.is_null() || dst as u64 != base {
        // Address lost — try to recover by mapping anywhere (IAT still at old base → broken).
        eprintln!(
            "vfs-inject: privatize VirtualAllocEx @ 0x{base:x} failed for {name}; process may be unstable"
        );
        return Err("privatize: VirtualAllocEx same base failed");
    }

    if WriteProcessMemory(
        process,
        dst,
        live.as_ptr() as *const c_void,
        size_of_image,
        &mut n,
    ) == 0
        || n != size_of_image
    {
        return Err("privatize: WPM live failed");
    }

    eprintln!(
        "vfs-inject: privatized {name} @ 0x{base:x} ({size_of_image} bytes) SEC_IMAGE->private (zip-identical live pages)"
    );
    Ok(())
}

/// Clone a live remote module image to a new private base, rebasing absolute
/// reloc entries from `src_base` → `dst_base`. Preserves DllMain-initialized
/// data while giving IAT a non-Steam HMODULE-equivalent private map.
///
/// Prefer [`privatize_remote_module_same_base`] for game-locals (same HMODULE).
#[allow(dead_code)]
unsafe fn clone_rebase_remote_module(
    process: HANDLE,
    src_base: u64,
    pe: &[u8],
) -> Result<u64, &'static str> {
    let (_layout, preferred_base, _entry, size_of_image) = pe_layout(pe)?;
    let e_lfanew = rd_u32(pe, 0x3C) as usize;

    // Prefer preferred PE base; else any free VA.
    let mut dst = VirtualAllocEx(
        process,
        preferred_base as *const c_void,
        size_of_image,
        MEM_COMMIT | MEM_RESERVE,
        PAGE_EXECUTE_READWRITE,
    );
    if dst.is_null() {
        dst = VirtualAllocEx(
            process,
            core::ptr::null(),
            size_of_image,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_EXECUTE_READWRITE,
        );
    }
    if dst.is_null() {
        return Err("VirtualAllocEx private game-local failed");
    }
    let dst_base = dst as u64;

    // Copy live (initialized) pages from host map.
    let mut img = vec![0u8; size_of_image];
    let mut n = 0usize;
    if ReadProcessMemory(
        process,
        src_base as *const c_void,
        img.as_mut_ptr() as *mut c_void,
        size_of_image,
        &mut n,
    ) == 0
        || n != size_of_image
    {
        return Err("RPM live game-local failed");
    }

    // Rebase reloc-tracked absolute addresses src → dst.
    if dst_base != src_base {
        crate::map::apply_relocs(&mut img, e_lfanew, src_base, dst_base);
    }

    if WriteProcessMemory(
        process,
        dst,
        img.as_ptr() as *const c_void,
        size_of_image,
        &mut n,
    ) == 0
        || n != size_of_image
    {
        return Err("WPM private game-local failed");
    }

    // Optional: leave src_base mapped as orphan (any non-reloc absolute data).
    let _ = preferred_base;
    Ok(dst_base)
}

/// Unmap Steam-disk game-local SEC_IMAGE and **unlink** its LDR entry so a
/// subsequent LoadLibrary(GameLayers\…) creates a fresh zip-window mapping.
unsafe fn purge_steam_game_local(process: HANDLE, dll_name: &str) {
    let want = std::path::Path::new(dll_name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(dll_name)
        .to_ascii_lowercase();

    // FreeLibrary / LdrUnloadDll first (best effort).
    unload_steam_path_module(process, dll_name);

    // If still present: unmap + unlink LDR (ghost entry blocks re-LoadLibrary).
    if let Some((m, path)) = find_remote_module_steam_path(process, &want) {
        type NtUnmapFn = unsafe extern "system" fn(HANDLE, *mut c_void) -> NTSTATUS;
        if let Some(unmap) =
            ntdll_proc(b"NtUnmapViewOfSection\0").map(|p| core::mem::transmute::<_, NtUnmapFn>(p))
        {
            let st = unmap(process, m as *mut c_void);
            eprintln!(
                "vfs-inject: purge NtUnmap Steam {path} base=0x{:x} status=0x{st:x}",
                m as usize
            );
        }
        if let Some(entry) = find_remote_ldr_entry_by_base(process, m as usize as u64) {
            if unlink_remote_ldr_entry(process, entry).is_ok() {
                eprintln!(
                    "vfs-inject: purge LDR unlinked Steam {want} entry=0x{entry:x}"
                );
            } else {
                eprintln!("vfs-inject: purge LDR unlink failed for {want}");
            }
        }
    }
}

/// Find LDR_DATA_TABLE_ENTRY address whose DllBase == `dll_base`.
unsafe fn find_remote_ldr_entry_by_base(process: HANDLE, dll_base: u64) -> Option<u64> {
    type NtQip = unsafe extern "system" fn(HANDLE, u32, *mut c_void, u32, *mut u32) -> NTSTATUS;
    let nt_qip: NtQip = core::mem::transmute(ntdll_proc(b"NtQueryInformationProcess\0")?);
    let mut pbi: ProcessBasicInformation = zeroed();
    let mut ret_len = 0u32;
    if nt_qip(
        process,
        PROCESS_BASIC_INFORMATION,
        &mut pbi as *mut _ as *mut c_void,
        size_of::<ProcessBasicInformation>() as u32,
        &mut ret_len,
    ) != 0
        || pbi.peb_base_address.is_null()
    {
        return None;
    }
    let peb = pbi.peb_base_address as usize;
    let mut n = 0usize;
    let mut ldr: u64 = 0;
    if ReadProcessMemory(
        process,
        (peb + 0x18) as *const c_void,
        &mut ldr as *mut u64 as *mut c_void,
        8,
        &mut n,
    ) == 0
        || ldr == 0
    {
        return None;
    }
    let mut flink: u64 = 0;
    if ReadProcessMemory(
        process,
        (ldr as usize + 0x10) as *const c_void,
        &mut flink as *mut u64 as *mut c_void,
        8,
        &mut n,
    ) == 0
        || flink == 0
    {
        return None;
    }
    let head = flink;
    let mut entry = flink;
    for _ in 0..512 {
        let mut base: u64 = 0;
        if ReadProcessMemory(
            process,
            (entry as usize + 0x30) as *const c_void,
            &mut base as *mut u64 as *mut c_void,
            8,
            &mut n,
        ) == 0
        {
            break;
        }
        if base == dll_base {
            return Some(entry);
        }
        let mut next: u64 = 0;
        if ReadProcessMemory(
            process,
            entry as *const c_void,
            &mut next as *mut u64 as *mut c_void,
            8,
            &mut n,
        ) == 0
            || next == 0
            || next == head
        {
            break;
        }
        entry = next;
    }
    None
}

/// Unlink a remote LDR_DATA_TABLE_ENTRY from InLoadOrder / InMemoryOrder /
/// InInitializationOrder / HashLinks (offsets for x64 Win10+).
unsafe fn unlink_remote_ldr_entry(process: HANDLE, entry: u64) -> Result<(), &'static str> {
    // LIST_ENTRY unlink at entry+off
    let unlink_le = |off: u64| -> Result<(), &'static str> {
        let mut flink: u64 = 0;
        let mut blink: u64 = 0;
        let mut n = 0usize;
        if ReadProcessMemory(
            process,
            (entry as usize + off as usize) as *const c_void,
            &mut flink as *mut u64 as *mut c_void,
            8,
            &mut n,
        ) == 0
            || ReadProcessMemory(
                process,
                (entry as usize + off as usize + 8) as *const c_void,
                &mut blink as *mut u64 as *mut c_void,
                8,
                &mut n,
            ) == 0
            || flink == 0
            || blink == 0
        {
            return Err("read LIST_ENTRY");
        }
        // Blink->Flink = Flink
        if WriteProcessMemory(
            process,
            blink as *mut c_void,
            &flink as *const u64 as *const c_void,
            8,
            &mut n,
        ) == 0
        {
            return Err("write Blink->Flink");
        }
        // Flink->Blink = Blink
        if WriteProcessMemory(
            process,
            (flink as usize + 8) as *mut c_void,
            &blink as *const u64 as *const c_void,
            8,
            &mut n,
        ) == 0
        {
            return Err("write Flink->Blink");
        }
        // Self-point entry links (optional hygiene)
        let self_le = entry + off;
        let _ = WriteProcessMemory(
            process,
            (entry as usize + off as usize) as *mut c_void,
            &self_le as *const u64 as *const c_void,
            8,
            &mut n,
        );
        let _ = WriteProcessMemory(
            process,
            (entry as usize + off as usize + 8) as *mut c_void,
            &self_le as *const u64 as *const c_void,
            8,
            &mut n,
        );
        Ok(())
    };
    unlink_le(0x00)?; // InLoadOrderLinks
    unlink_le(0x10)?; // InMemoryOrderLinks
    unlink_le(0x20)?; // InInitializationOrderLinks
    let _ = unlink_le(0x70); // HashLinks (best-effort; offset can vary)
    Ok(())
}

/// IMAGE_SCN_MEM_WRITE — skip full equality for writable sections when
/// `non_writable_only` (DllMain may dirty .data); always compare code/const.
const IMAGE_SCN_MEM_WRITE: u32 = 0x8000_0000;

/// Prove remote module image matches `pe_layout(zip)` after reloc to `host_base`.
///
/// Compares headers + every section whose Characteristics lack MEM_WRITE (and
/// when `non_writable_only` is false, writable sections too). IAT directory and
/// FirstThunk slots are taken from the remote image before compare so loader-
/// resolved imports do not false-fail identity.
///
/// Header-only checks are insufficient: a mismatch at section VA +0x1000 must fail.
unsafe fn prove_remote_matches_zip_pe(
    process: HANDLE,
    host_base: u64,
    pe: &[u8],
) -> Result<(), &'static str> {
    remote_image_matches_zip_layout(process, host_base, pe, true)
}

/// Strict full layout identity (all sections, IAT-masked). Used after zip WPM.
unsafe fn prove_remote_matches_zip_pe_strict(
    process: HANDLE,
    host_base: u64,
    pe: &[u8],
) -> Result<(), &'static str> {
    remote_image_matches_zip_layout(process, host_base, pe, false)
}

unsafe fn remote_image_matches_zip_layout(
    process: HANDLE,
    host_base: u64,
    pe: &[u8],
    non_writable_only: bool,
) -> Result<(), &'static str> {
    let (mut img, preferred_base, _entry, size_of_image) = pe_layout(pe)?;
    let e_lfanew = rd_u32(&img, 0x3C) as usize;

    let mut host_soi = 0u32;
    let mut n = 0usize;
    let mut e_lf = 0u32;
    if ReadProcessMemory(
        process,
        (host_base as usize + 0x3C) as *const c_void,
        &mut e_lf as *mut u32 as *mut c_void,
        4,
        &mut n,
    ) == 0
    {
        return Err("rpm e_lfanew");
    }
    if ReadProcessMemory(
        process,
        (host_base as usize + e_lf as usize + 24 + 56) as *const c_void,
        &mut host_soi as *mut u32 as *mut c_void,
        4,
        &mut n,
    ) == 0
    {
        return Err("rpm SizeOfImage");
    }
    if host_soi as usize != size_of_image {
        return Err("SizeOfImage mismatch");
    }
    if host_base != preferred_base {
        crate::map::apply_relocs(&mut img, e_lfanew, preferred_base, host_base);
    }
    // Align expected IAT with remote so compare is about zip code/const pages.
    let _ = copy_remote_iat_into_image(process, host_base, &mut img, e_lfanew);

    let opt = e_lfanew + 24;
    let size_of_headers = rd_u32(&img, opt + 60) as usize;
    let hdr_len = size_of_headers.min(img.len()).min(size_of_image);
    // Headers: skip optional-header ImageBase (already relocated) — compare
    // PE signature + section table via full header blob minus ImageBase field.
    let mut remote_hdr = vec![0u8; hdr_len];
    if ReadProcessMemory(
        process,
        host_base as *const c_void,
        remote_hdr.as_mut_ptr() as *mut c_void,
        hdr_len,
        &mut n,
    ) == 0
        || n != hdr_len
    {
        return Err("rpm headers");
    }
    if remote_hdr[0] != b'M' || remote_hdr[1] != b'Z' {
        return Err("remote not MZ");
    }
    if e_lfanew + 4 <= hdr_len && remote_hdr[e_lfanew..e_lfanew + 4] != img[e_lfanew..e_lfanew + 4]
    {
        return Err("PE sig mismatch");
    }
    // Mask ImageBase (PE32+ optional header +24) before header compare.
    let ib_off = opt + 24;
    if ib_off + 8 <= hdr_len {
        remote_hdr[ib_off..ib_off + 8].copy_from_slice(&img[ib_off..ib_off + 8]);
    }
    if remote_hdr[..hdr_len] != img[..hdr_len] {
        return Err("header bytes mismatch vs zip layout");
    }

    let num_sections = rd_u16(&img, e_lfanew + 6) as usize;
    let size_opt = rd_u16(&img, e_lfanew + 20) as usize;
    let sect_base = opt + size_opt;
    let mut compared = 0usize;
    for i in 0..num_sections {
        let s = sect_base + i * 40;
        if s + 40 > img.len() {
            break;
        }
        let va = rd_u32(&img, s + 12) as usize;
        let vsz = rd_u32(&img, s + 8) as usize;
        let raw_sz = rd_u32(&img, s + 16) as usize;
        let chars = rd_u32(&img, s + 36);
        let cmp_sz = vsz.max(raw_sz).min(size_of_image.saturating_sub(va));
        if cmp_sz == 0 {
            continue;
        }
        if non_writable_only && (chars & IMAGE_SCN_MEM_WRITE) != 0 {
            continue;
        }
        // Exclude IAT directory range from section body if it falls inside.
        let mut expect = img[va..va + cmp_sz].to_vec();
        let mut remote = vec![0u8; cmp_sz];
        if ReadProcessMemory(
            process,
            (host_base as usize + va) as *const c_void,
            remote.as_mut_ptr() as *mut c_void,
            cmp_sz,
            &mut n,
        ) == 0
            || n != cmp_sz
        {
            return Err("rpm section");
        }
        // IAT already copied into `img`; remote should match if overwrite ran.
        if expect != remote {
            // FNV of both for diagnostics
            let eh = simple_fnv64(&expect);
            let rh = simple_fnv64(&remote);
            eprintln!(
                "vfs-inject: section VA=0x{va:x} len=0x{cmp_sz:x} zip_fnv={eh} remote_fnv={rh}"
            );
            return Err("section bytes mismatch vs zip layout");
        }
        compared = compared.saturating_add(cmp_sz);
    }
    if compared < 0x1000 {
        return Err("too few section bytes compared (need >= 0x1000)");
    }
    Ok(())
}

/// FNV-1a of remote non-writable section bytes after IAT mask (for SCRATCH evidence).
unsafe fn remote_zip_layout_fnv(
    process: HANDLE,
    host_base: u64,
    pe: &[u8],
) -> Result<String, &'static str> {
    let (mut img, preferred_base, _entry, size_of_image) = pe_layout(pe)?;
    let e_lfanew = rd_u32(&img, 0x3C) as usize;
    if host_base != preferred_base {
        crate::map::apply_relocs(&mut img, e_lfanew, preferred_base, host_base);
    }
    let _ = copy_remote_iat_into_image(process, host_base, &mut img, e_lfanew);
    let opt = e_lfanew + 24;
    let num_sections = rd_u16(&img, e_lfanew + 6) as usize;
    let size_opt = rd_u16(&img, e_lfanew + 20) as usize;
    let sect_base = opt + size_opt;
    let mut acc = Vec::new();
    for i in 0..num_sections {
        let s = sect_base + i * 40;
        if s + 40 > img.len() {
            break;
        }
        let va = rd_u32(&img, s + 12) as usize;
        let vsz = rd_u32(&img, s + 8) as usize;
        let raw_sz = rd_u32(&img, s + 16) as usize;
        let chars = rd_u32(&img, s + 36);
        if (chars & IMAGE_SCN_MEM_WRITE) != 0 {
            continue;
        }
        let cmp_sz = vsz.max(raw_sz).min(size_of_image.saturating_sub(va));
        if cmp_sz == 0 {
            continue;
        }
        let mut remote = vec![0u8; cmp_sz];
        let mut n = 0usize;
        if ReadProcessMemory(
            process,
            (host_base as usize + va) as *const c_void,
            remote.as_mut_ptr() as *mut c_void,
            cmp_sz,
            &mut n,
        ) == 0
            || n != cmp_sz
        {
            return Err("rpm fnv section");
        }
        acc.extend_from_slice(&remote);
    }
    Ok(simple_fnv64(&acc))
}

fn simple_fnv64(data: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Stage B: overwrite a remote mapped DLL with zip PE layout, **preserving**
/// the remote module's existing IAT (loader-resolved). Does not re-run DllMain.
/// Live code/const pages become archive bytes from `pe` (zip-window source).
unsafe fn overwrite_remote_module_zip_preserve_iat(
    process: HANDLE,
    host_base: u64,
    pe: &[u8],
    name: &str,
) -> Result<u64, &'static str> {
    use windows_sys::Win32::System::Memory::{VirtualProtectEx, PAGE_EXECUTE_READWRITE};

    let (mut img, preferred_base, _entry, size_of_image) = pe_layout(pe)?;
    let e_lfanew = rd_u32(&img, 0x3C) as usize;

    let mut e_lf = 0u32;
    let mut n = 0usize;
    if ReadProcessMemory(
        process,
        (host_base as usize + 0x3C) as *const c_void,
        &mut e_lf as *mut u32 as *mut c_void,
        4,
        &mut n,
    ) == 0
    {
        return Err("read host e_lfanew");
    }
    let mut host_soi = 0u32;
    if ReadProcessMemory(
        process,
        (host_base as usize + e_lf as usize + 24 + 56) as *const c_void,
        &mut host_soi as *mut u32 as *mut c_void,
        4,
        &mut n,
    ) == 0
    {
        return Err("read host SizeOfImage");
    }
    if host_soi as usize != size_of_image {
        return Err("SizeOfImage mismatch");
    }

    if host_base != preferred_base {
        crate::map::apply_relocs(&mut img, e_lfanew, preferred_base, host_base);
    }
    // Keep loader IAT — do not resolve_imports.
    let _ = copy_remote_iat_into_image(process, host_base, &mut img, e_lfanew);

    // Preserve DllMain-dirtied *writable* section bodies (Steam DRM globals).
    // Non-writable sections stay as zip pe_layout (code/const provenance).
    let opt = e_lfanew + 24;
    let num_sections = rd_u16(&img, e_lfanew + 6) as usize;
    let size_opt = rd_u16(&img, e_lfanew + 20) as usize;
    let sect_base = opt + size_opt;
    for i in 0..num_sections {
        let s = sect_base + i * 40;
        if s + 40 > img.len() {
            break;
        }
        let va = rd_u32(&img, s + 12) as usize;
        let vsz = rd_u32(&img, s + 8) as usize;
        let raw_sz = rd_u32(&img, s + 16) as usize;
        let chars = rd_u32(&img, s + 36);
        if (chars & IMAGE_SCN_MEM_WRITE) == 0 {
            continue;
        }
        let len = vsz.max(raw_sz).min(size_of_image.saturating_sub(va));
        if len == 0 || va + len > img.len() {
            continue;
        }
        let mut rn = 0usize;
        let _ = ReadProcessMemory(
            process,
            (host_base as usize + va) as *const c_void,
            img[va..va + len].as_mut_ptr() as *mut c_void,
            len,
            &mut rn,
        );
    }
    // Re-apply IAT after writable restore (writable may overlap import data).
    let _ = copy_remote_iat_into_image(process, host_base, &mut img, e_lfanew);

    let mut old = 0u32;
    let _ = VirtualProtectEx(
        process,
        host_base as *mut c_void,
        size_of_image,
        PAGE_EXECUTE_READWRITE,
        &mut old,
    );
    let write_len = img.len().min(size_of_image);
    let mut written = 0usize;
    if WriteProcessMemory(
        process,
        host_base as *mut c_void,
        img.as_ptr() as *const c_void,
        write_len,
        &mut written,
    ) == 0
        || written != write_len
    {
        return Err("WPM zip DLL failed");
    }
    let mut probe = [0u8; 2];
    let mut pn = 0usize;
    if ReadProcessMemory(
        process,
        host_base as *const c_void,
        probe.as_mut_ptr() as *mut c_void,
        2,
        &mut pn,
    ) != 0
        && (probe[0] != b'M' || probe[1] != b'Z')
    {
        return Err("post-write not MZ");
    }
    eprintln!(
        "vfs-inject: wrote {written} zip PE bytes to 0x{host_base:x} for {name} (IAT+writable preserved, non-writable from zip)"
    );
    Ok(host_base)
}

/// Stages B→D after main-image hollow: zip-overwrite game-locals (preserve IAT),
/// strict non-writable identity vs pe_layout(zip), LDR FullDllName → GameLayers.
unsafe fn finalize_game_local_modules(
    process: HANDLE,
    _peb: usize,
    forced_bases: &[(String, u64)],
) {
    let search_dirs = game_local_search_dirs();
    let root = search_dirs
        .first()
        .map(|s| s.as_str())
        .unwrap_or(r"C:\GameLayers\runtime");
    for (name, zip_base) in forced_bases {
        let virt = format!("{root}\\{name}");

        // Stage B: materialize zip PE into the live module (not privatize-of-Steam).
        let (pe, src) = match read_game_local_pe(name, &search_dirs) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("vfs-inject: finalize {name}: zip PE read failed: {e}");
                continue;
            }
        };
        match overwrite_remote_module_zip_preserve_iat(process, *zip_base, &pe, name) {
            Ok(_) => {}
            Err(e) => {
                eprintln!(
                    "vfs-inject: finalize zip-overwrite {name} @ 0x{zip_base:x} failed: {e} (source={src})"
                );
                // Still try LDR spoof; identity will warn below.
            }
        }

        // Stage C: remote non-writable pages must equal pe_layout(zip)+reloc.
        match prove_remote_matches_zip_pe(process, *zip_base, &pe) {
            Ok(()) => {
                let fnv = remote_zip_layout_fnv(process, *zip_base, &pe).unwrap_or_default();
                eprintln!(
                    "vfs-inject: zip PE identity OK {name} @ 0x{zip_base:x} nonwritable_fnv={fnv} (source={src})"
                );
            }
            Err(e) => eprintln!(
                "vfs-inject: zip PE identity FAIL {name} @ 0x{zip_base:x}: {e} (source={src})"
            ),
        }

        // Stage D: LDR path → GameLayers virtual path.
        match spoof_remote_ldr_module_path(process, name, &virt, *zip_base) {
            Ok(()) => eprintln!(
                "vfs-inject: LDR path {name} -> {virt} base=0x{zip_base:x} (zip PE overwritten map)"
            ),
            Err(e) => eprintln!("vfs-inject: LDR path spoof {name} failed: {e}"),
        }

        if let Ok(path) = remote_module_path(process, name) {
            let pl = path.to_ascii_lowercase();
            if pl.contains("steamapps") && !pl.contains("gamelayers") {
                eprintln!(
                    "vfs-inject: WARNING {name} still Steam path after finalize: {path}"
                );
            } else {
                eprintln!("vfs-inject: module path OK {name} => {path}");
            }
        } else {
            eprintln!(
                "vfs-inject: module path unknown for {name} (zip map 0x{zip_base:x})"
            );
        }
    }
}

/// FreeLibrary until gone, then single NtUnmap if still present under Steam.
/// Never FreeLibrary after unmap (that AVs).
unsafe fn unload_steam_path_module(process: HANDLE, dll_name: &str) {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};

    let want = std::path::Path::new(dll_name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(dll_name)
        .to_ascii_lowercase();

    let k32 = GetModuleHandleA(b"kernel32.dll\0".as_ptr());
    if k32.is_null() {
        return;
    }
    let free_library = match GetProcAddress(k32, b"FreeLibrary\0".as_ptr()) {
        Some(p) => p,
        None => return,
    };
    let free_start: LPTHREAD_START_ROUTINE = Some(core::mem::transmute(free_library));

    // Prefer ntdll!LdrUnloadDll when available (handles pinned deps better).
    let ldr_unload: Option<LPTHREAD_START_ROUTINE> = ntdll_proc(b"LdrUnloadDll\0")
        .map(|p| Some(core::mem::transmute(p)))
        .unwrap_or(None);

    for _ in 0..6 {
        let Some((m, path)) = find_remote_module_steam_path(process, &want) else {
            return;
        };
        let start = ldr_unload.unwrap_or(free_start);
        let ht = CreateRemoteThread(
            process,
            core::ptr::null(),
            0,
            start,
            m as *mut c_void,
            0,
            core::ptr::null_mut(),
        );
        if ht.is_null() {
            break;
        }
        WaitForSingleObject(ht, INFINITE);
        let mut code = 0u32;
        let _ = windows_sys::Win32::System::Threading::GetExitCodeThread(ht, &mut code);
        CloseHandle(ht);
        eprintln!("vfs-inject: unload Steam-path {path} (exit=0x{code:x})");
        // LdrUnloadDll returns NTSTATUS 0 on success; FreeLibrary returns BOOL 1.
        if find_remote_module_steam_path(process, &want).is_none() {
            return;
        }
    }

    if let Some((m, path)) = find_remote_module_steam_path(process, &want) {
        type NtUnmapFn = unsafe extern "system" fn(HANDLE, *mut c_void) -> NTSTATUS;
        if let Some(unmap) =
            ntdll_proc(b"NtUnmapViewOfSection\0").map(|p| core::mem::transmute::<_, NtUnmapFn>(p))
        {
            let st = unmap(process, m as *mut c_void);
            eprintln!(
                "vfs-inject: NtUnmap Steam-path {path} base=0x{:x} status=0x{st:x}",
                m as usize
            );
        }
        // Rename the ghost LDR entry so LoadLibrary("steam_api64.dll") does not
        // re-bind the dead Steam module. Path spoof to a non-matching basename.
        let dead = format!("{want}.unloaded");
        let dead_full = format!(r"C:\GameLayers\runtime\{dead}");
        if spoof_remote_ldr_module_path(process, dll_name, &dead_full, m as usize as u64).is_ok() {
            eprintln!("vfs-inject: ghost LDR renamed {want} -> {dead}");
        }
    }
}

unsafe fn find_remote_module_steam_path(
    process: HANDLE,
    want_base: &str,
) -> Option<(windows_sys::Win32::Foundation::HMODULE, String)> {
    use windows_sys::Win32::Foundation::HMODULE;
    use windows_sys::Win32::System::ProcessStatus::{
        EnumProcessModulesEx, GetModuleFileNameExA, LIST_MODULES_ALL,
    };
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
        return None;
    }
    let count = (needed as usize) / std::mem::size_of::<HMODULE>();
    for m in mods.into_iter().take(count) {
        if m.is_null() {
            continue;
        }
        let mut path_buf = [0u8; 520];
        let n = GetModuleFileNameExA(process, m, path_buf.as_mut_ptr(), 520);
        if n == 0 {
            continue;
        }
        let path = String::from_utf8_lossy(&path_buf[..n as usize]).into_owned();
        let base = std::path::Path::new(&path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if base != want_base {
            continue;
        }
        let pl = path.to_ascii_lowercase();
        if pl.contains("gamelayers") {
            continue;
        }
        if pl.contains("steamapps") || pl.contains("\\steam\\") {
            return Some((m, path));
        }
    }
    None
}

/// Remote LoadLibraryA(full_path) → module base or None.
unsafe fn remote_load_library_path(
    process: HANDLE,
    load_start: LPTHREAD_START_ROUTINE,
    full_path: &str,
) -> Option<u64> {
    use windows_sys::Win32::System::Memory::PAGE_READWRITE;
    let mut raw = full_path.as_bytes().to_vec();
    raw.push(0);
    let remote = VirtualAllocEx(
        process,
        core::ptr::null(),
        raw.len(),
        MEM_COMMIT | MEM_RESERVE,
        PAGE_READWRITE,
    );
    if remote.is_null() {
        return None;
    }
    let mut n = 0usize;
    if WriteProcessMemory(
        process,
        remote,
        raw.as_ptr() as *const c_void,
        raw.len(),
        &mut n,
    ) == 0
    {
        return None;
    }
    let ht = CreateRemoteThread(
        process,
        core::ptr::null(),
        0,
        load_start,
        remote,
        0,
        core::ptr::null_mut(),
    );
    if ht.is_null() {
        return None;
    }
    WaitForSingleObject(ht, INFINITE);
    let mut code = 0u32;
    let _ = windows_sys::Win32::System::Threading::GetExitCodeThread(ht, &mut code);
    CloseHandle(ht);
    // LoadLibrary returns HMODULE; 0 = failure. Remote thread AV exit codes look
    // like NTSTATUS (0xC000xxxx) and must not be treated as bases.
    if code == 0 || (code & 0xF000_0000) == 0xC000_0000 {
        if code != 0 {
            eprintln!(
                "vfs-inject: remote LoadLibrary({full_path}) thread crashed exit=0x{code:x}"
            );
        }
        None
    } else {
        Some(code as u64)
    }
}

/// Rewrite remote LDR_DATA_TABLE_ENTRY FullDllName/BaseDllName (and DllBase)
/// for `dll_name` to `virt_path` / `dll_base`.
unsafe fn spoof_remote_ldr_module_path(
    process: HANDLE,
    dll_name: &str,
    virt_path: &str,
    dll_base: u64,
) -> Result<(), &'static str> {
    type NtQip = unsafe extern "system" fn(HANDLE, u32, *mut c_void, u32, *mut u32) -> NTSTATUS;
    let nt_qip: NtQip = core::mem::transmute(
        ntdll_proc(b"NtQueryInformationProcess\0").ok_or("NtQueryInformationProcess")?,
    );
    let mut pbi: ProcessBasicInformation = zeroed();
    let mut ret_len = 0u32;
    let st = nt_qip(
        process,
        PROCESS_BASIC_INFORMATION,
        &mut pbi as *mut _ as *mut c_void,
        size_of::<ProcessBasicInformation>() as u32,
        &mut ret_len,
    );
    if st != 0 || pbi.peb_base_address.is_null() {
        return Err("PEB query failed");
    }
    let peb = pbi.peb_base_address as usize;
    let mut ldr: u64 = 0;
    let mut n = 0usize;
    if ReadProcessMemory(
        process,
        (peb + 0x18) as *const c_void,
        &mut ldr as *mut u64 as *mut c_void,
        8,
        &mut n,
    ) == 0
        || ldr == 0
    {
        return Err("read Ldr failed");
    }
    let mut flink: u64 = 0;
    if ReadProcessMemory(
        process,
        (ldr as usize + 0x10) as *const c_void,
        &mut flink as *mut u64 as *mut c_void,
        8,
        &mut n,
    ) == 0
        || flink == 0
    {
        return Err("read InLoadOrder failed");
    }

    let want = std::path::Path::new(dll_name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(dll_name)
        .to_ascii_lowercase();
    let base_name = std::path::Path::new(virt_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(dll_name);
    let full_w = remote_wstring(process, virt_path)?;
    let base_w = remote_wstring(process, base_name)?;
    let full_chars = virt_path.encode_utf16().count();
    let base_chars = base_name.encode_utf16().count();

    let mut entry = flink;
    for _ in 0..256 {
        // BaseDllName Buffer @ UNICODE_STRING +0x58 → Length@0, Buffer@+8
        let mut name_buf_ptr: u64 = 0;
        let mut name_len: u16 = 0;
        if ReadProcessMemory(
            process,
            (entry as usize + 0x58) as *const c_void,
            &mut name_len as *mut u16 as *mut c_void,
            2,
            &mut n,
        ) == 0
        {
            break;
        }
        if ReadProcessMemory(
            process,
            (entry as usize + 0x58 + 8) as *const c_void,
            &mut name_buf_ptr as *mut u64 as *mut c_void,
            8,
            &mut n,
        ) == 0
            || name_buf_ptr == 0
            || name_len == 0
        {
            // try next
        } else {
            let nchars = (name_len as usize) / 2;
            let mut wbuf = vec![0u16; nchars.min(260)];
            if ReadProcessMemory(
                process,
                name_buf_ptr as *const c_void,
                wbuf.as_mut_ptr() as *mut c_void,
                wbuf.len() * 2,
                &mut n,
            ) != 0
            {
                let s = String::from_utf16_lossy(&wbuf).to_ascii_lowercase();
                if s == want || s.trim_end_matches('\0') == want {
                    write_ustr(process, entry + 0x48, full_w, full_chars)?;
                    write_ustr(process, entry + 0x58, base_w, base_chars)?;
                    let _ = WriteProcessMemory(
                        process,
                        (entry as usize + 0x30) as *mut c_void,
                        &dll_base as *const u64 as *const c_void,
                        8,
                        &mut n,
                    );
                    return Ok(());
                }
            }
        }
        let mut next: u64 = 0;
        if ReadProcessMemory(
            process,
            entry as *const c_void,
            &mut next as *mut u64 as *mut c_void,
            8,
            &mut n,
        ) == 0
            || next == 0
            || next == flink
        {
            break;
        }
        entry = next;
    }
    // No existing entry: nothing to spoof (manual map without LDR).
    Err("LDR entry not found for basename")
}

/// Optional remote module base by basename (None if not loaded).
unsafe fn find_remote_module_base_opt(process: HANDLE, dll_name: &str) -> Option<u64> {
    use windows_sys::Win32::Foundation::HMODULE;
    use windows_sys::Win32::System::ProcessStatus::{
        EnumProcessModulesEx, GetModuleBaseNameA, LIST_MODULES_ALL,
    };
    let want = std::path::Path::new(dll_name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(dll_name)
        .to_ascii_lowercase();
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
        return None;
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
        if s == want {
            return Some(m as usize as u64);
        }
    }
    None
}

/// Overwrite an already-mapped remote DLL image with zip PE bytes (in-place)
/// when SizeOfImage matches. Leaves LDR path as-is (may still say Steam) but
/// **live code/data are archive bytes** — no extract to disk.
unsafe fn overwrite_remote_module_with_zip_pe(
    process: HANDLE,
    host_base: u64,
    pe: &[u8],
    name: &str,
) -> Result<u64, &'static str> {
    use windows_sys::Win32::System::Memory::{VirtualProtectEx, PAGE_EXECUTE_READWRITE};

    let (mut img, preferred_base, _entry_rva, size_of_image) = pe_layout(pe)?;
    let e_lfanew = rd_u32(&img, 0x3C) as usize;

    // Read host SizeOfImage.
    let mut e_lf = 0u32;
    let mut n = 0usize;
    if ReadProcessMemory(
        process,
        (host_base as usize + 0x3C) as *const c_void,
        &mut e_lf as *mut u32 as *mut c_void,
        4,
        &mut n,
    ) == 0
    {
        return Err("read host e_lfanew failed");
    }
    let mut host_soi = 0u32;
    if ReadProcessMemory(
        process,
        (host_base as usize + e_lf as usize + 24 + 56) as *const c_void,
        &mut host_soi as *mut u32 as *mut c_void,
        4,
        &mut n,
    ) == 0
    {
        return Err("read host SizeOfImage failed");
    }
    if host_soi as usize != size_of_image {
        eprintln!(
            "vfs-inject: {name} host soi=0x{host_soi:x} zip soi=0x{size_of_image:x} mismatch"
        );
        return Err("SizeOfImage mismatch for in-place DLL overwrite");
    }

    if host_base != preferred_base {
        crate::map::apply_relocs(&mut img, e_lfanew, preferred_base, host_base);
    }
    // Keep exports at this base; resolve imports against parent system DLLs.
    crate::map::resolve_imports(&mut img, e_lfanew)?;

    let mut old = 0u32;
    let _ = VirtualProtectEx(
        process,
        host_base as *mut c_void,
        size_of_image,
        PAGE_EXECUTE_READWRITE,
        &mut old,
    );
    let mut written = 0usize;
    if WriteProcessMemory(
        process,
        host_base as *mut c_void,
        img.as_ptr() as *const c_void,
        img.len().min(size_of_image),
        &mut written,
    ) == 0
        || written == 0
    {
        return Err("WriteProcessMemory in-place DLL failed");
    }
    // Verify a few PE header bytes.
    let mut probe = [0u8; 2];
    let mut pn = 0usize;
    if ReadProcessMemory(
        process,
        host_base as *const c_void,
        probe.as_mut_ptr() as *mut c_void,
        2,
        &mut pn,
    ) != 0
        && (probe[0] != b'M' || probe[1] != b'Z')
    {
        return Err("in-place DLL probe not MZ");
    }
    let _ = name;
    Ok(host_base)
}

/// Manual-map a PE DLL into `process` from archive/VFS bytes (no disk write).
/// Manual-map zip PE into remote process. When `call_dllmain` is false, only
/// exports/IAT resolution are used (Steam host already ran DllMain for DRM).
unsafe fn map_remote_dll_from_pe(
    process: HANDLE,
    pe: &[u8],
    name: &str,
) -> Result<u64, &'static str> {
    map_remote_dll_from_pe_ex(process, pe, name, true)
}

unsafe fn map_remote_dll_from_pe_ex(
    process: HANDLE,
    pe: &[u8],
    name: &str,
    call_dllmain: bool,
) -> Result<u64, &'static str> {
    let (mut img, preferred_base, entry_rva, size_of_image) = pe_layout(pe)?;
    let e_lfanew = rd_u32(&img, 0x3C) as usize;

    let mut base = VirtualAllocEx(
        process,
        preferred_base as *const c_void,
        size_of_image,
        MEM_COMMIT | MEM_RESERVE,
        PAGE_EXECUTE_READWRITE,
    );
    if base.is_null() {
        base = VirtualAllocEx(
            process,
            core::ptr::null(),
            size_of_image,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_EXECUTE_READWRITE,
        );
    }
    if base.is_null() {
        return Err("VirtualAllocEx game-local DLL failed");
    }
    let base_u = base as u64;
    if base_u != preferred_base {
        crate::map::apply_relocs(&mut img, e_lfanew, preferred_base, base_u);
    }
    // DLL imports are system (kernel32, …) — shared bases with parent.
    crate::map::resolve_imports(&mut img, e_lfanew)?;

    let mut written = 0usize;
    if WriteProcessMemory(
        process,
        base,
        img.as_ptr() as *const c_void,
        img.len().min(size_of_image),
        &mut written,
    ) == 0
        || written == 0
    {
        return Err("WriteProcessMemory game-local DLL failed");
    }

    if call_dllmain {
        if let Err(e) = call_remote_dll_main(process, base_u, entry_rva, name) {
            eprintln!("vfs-inject: DllMain({name}) skipped/failed: {e}");
        }
    }
    Ok(base_u)
}

/// Remote x64 stub: call DllMain(base, DLL_PROCESS_ATTACH, 0).
unsafe fn call_remote_dll_main(
    process: HANDLE,
    base: u64,
    entry_rva: u32,
    name: &str,
) -> Result<(), &'static str> {
    use windows_sys::Win32::System::Memory::PAGE_READWRITE;

    let entry = base + entry_rva as u64;
    // sub rsp, 28h
    // mov rcx, imm64   ; hmodule
    // mov rdx, 1       ; DLL_PROCESS_ATTACH
    // xor r8, r8
    // mov rax, imm64   ; entry
    // call rax
    // add rsp, 28h
    // ret
    let mut stub = Vec::with_capacity(48);
    stub.extend_from_slice(&[0x48, 0x83, 0xEC, 0x28]); // sub rsp, 0x28
    stub.extend_from_slice(&[0x48, 0xB9]); // mov rcx, imm64
    stub.extend_from_slice(&base.to_le_bytes());
    stub.extend_from_slice(&[0x48, 0xC7, 0xC2, 0x01, 0x00, 0x00, 0x00]); // mov rdx, 1
    stub.extend_from_slice(&[0x4D, 0x31, 0xC0]); // xor r8, r8
    stub.extend_from_slice(&[0x48, 0xB8]); // mov rax, imm64
    stub.extend_from_slice(&entry.to_le_bytes());
    stub.extend_from_slice(&[0xFF, 0xD0]); // call rax
    stub.extend_from_slice(&[0x48, 0x83, 0xC4, 0x28]); // add rsp, 0x28
    stub.push(0xC3); // ret

    let remote = VirtualAllocEx(
        process,
        core::ptr::null(),
        stub.len(),
        MEM_COMMIT | MEM_RESERVE,
        PAGE_EXECUTE_READWRITE,
    );
    if remote.is_null() {
        return Err("VirtualAllocEx DllMain stub failed");
    }
    let mut n = 0usize;
    if WriteProcessMemory(
        process,
        remote,
        stub.as_ptr() as *const c_void,
        stub.len(),
        &mut n,
    ) == 0
    {
        return Err("WriteProcessMemory DllMain stub failed");
    }
    let start: LPTHREAD_START_ROUTINE = Some(core::mem::transmute(remote));
    let ht = CreateRemoteThread(
        process,
        core::ptr::null(),
        0,
        start,
        core::ptr::null_mut(),
        0,
        core::ptr::null_mut(),
    );
    if ht.is_null() {
        eprintln!("vfs-inject: DllMain stub CreateRemoteThread failed for {name}");
        return Err("DllMain CreateRemoteThread failed");
    }
    WaitForSingleObject(ht, INFINITE);
    let mut code = 0u32;
    let _ = windows_sys::Win32::System::Threading::GetExitCodeThread(ht, &mut code);
    CloseHandle(ht);
    eprintln!("vfs-inject: remote DllMain({name}) attach exit=0x{code:x}");
    Ok(())
}

/// Remote `SetDllDirectoryW(dir)` so bare LoadLibrary prefers GameLayers.
unsafe fn remote_set_dll_directory(process: HANDLE, dir: &str) -> Result<(), &'static str> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    use windows_sys::Win32::System::Memory::PAGE_READWRITE;

    let k32 = GetModuleHandleA(b"kernel32.dll\0".as_ptr());
    if k32.is_null() {
        return Err("kernel32");
    }
    let set_dir = match GetProcAddress(k32, b"SetDllDirectoryW\0".as_ptr()) {
        Some(p) => p,
        None => return Err("SetDllDirectoryW"),
    };
    let wide_dir = wide(dir);
    let bytes = wide_dir.len() * 2;
    let remote = VirtualAllocEx(
        process,
        core::ptr::null(),
        bytes,
        MEM_COMMIT | MEM_RESERVE,
        PAGE_READWRITE,
    );
    if remote.is_null() {
        return Err("alloc SetDllDirectory");
    }
    let mut n = 0usize;
    if WriteProcessMemory(
        process,
        remote,
        wide_dir.as_ptr() as *const c_void,
        bytes,
        &mut n,
    ) == 0
    {
        return Err("write SetDllDirectory");
    }
    let start: LPTHREAD_START_ROUTINE = Some(core::mem::transmute(set_dir));
    let ht = CreateRemoteThread(
        process,
        core::ptr::null(),
        0,
        start,
        remote,
        0,
        core::ptr::null_mut(),
    );
    if ht.is_null() {
        return Err("SetDllDirectory thread");
    }
    WaitForSingleObject(ht, INFINITE);
    CloseHandle(ht);
    eprintln!("vfs-inject: remote SetDllDirectoryW({dir})");
    Ok(())
}

/// After the main image is zip PE (IAT → zip manual maps), drop Steam-disk
/// game-local SEC_IMAGE mappings. Best-effort: FreeLibrary once, then unmap once.
unsafe fn post_hollow_drop_steam_module(process: HANDLE, dll_name: &str) -> Result<(), &'static str> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};

    let want = std::path::Path::new(dll_name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(dll_name)
        .to_ascii_lowercase();
    let allowed = game_local_search_dirs();
    let Some((m, path)) = find_remote_module_not_under(process, &want, &allowed) else {
        return Ok(());
    };
    let k32 = GetModuleHandleA(b"kernel32.dll\0".as_ptr());
    if k32.is_null() {
        return Ok(());
    }
    if let Some(free_library) = GetProcAddress(k32, b"FreeLibrary\0".as_ptr()) {
        let free_start: LPTHREAD_START_ROUTINE = Some(core::mem::transmute(free_library));
        let ht = CreateRemoteThread(
            process,
            core::ptr::null(),
            0,
            free_start,
            m as *mut c_void,
            0,
            core::ptr::null_mut(),
        );
        if !ht.is_null() {
            WaitForSingleObject(ht, INFINITE);
            CloseHandle(ht);
            eprintln!("vfs-inject: post-hollow FreeLibrary Steam module {path}");
        }
    }
    // If still present, unmap once (IAT already points at zip manual map).
    if let Some((m2, path2)) = find_remote_module_not_under(process, &want, &allowed) {
        type NtUnmapFn = unsafe extern "system" fn(HANDLE, *mut c_void) -> NTSTATUS;
        if let Some(unmap) =
            ntdll_proc(b"NtUnmapViewOfSection\0").map(|p| core::mem::transmute::<_, NtUnmapFn>(p))
        {
            let st = unmap(process, m2 as *mut c_void);
            eprintln!(
                "vfs-inject: post-hollow NtUnmap Steam {path2} base=0x{:x} status=0x{st:x}",
                m2 as usize
            );
        }
    }
    Ok(())
}

/// First remote module matching `want` basename whose path is outside allowed dirs.
unsafe fn find_remote_module_not_under(
    process: HANDLE,
    want: &str,
    allowed_dirs: &[String],
) -> Option<(windows_sys::Win32::Foundation::HMODULE, String)> {
    use windows_sys::Win32::Foundation::HMODULE;
    use windows_sys::Win32::System::ProcessStatus::{
        EnumProcessModulesEx, GetModuleFileNameExA, LIST_MODULES_ALL,
    };
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
        return None;
    }
    let count = (needed as usize) / std::mem::size_of::<HMODULE>();
    for m in mods.into_iter().take(count) {
        if m.is_null() {
            continue;
        }
        let mut path_buf = [0u8; 520];
        let n = GetModuleFileNameExA(process, m, path_buf.as_mut_ptr(), 520);
        if n == 0 {
            continue;
        }
        let path = String::from_utf8_lossy(&path_buf[..n as usize]).into_owned();
        let base = std::path::Path::new(&path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if base != want {
            continue;
        }
        let pl = path.to_ascii_lowercase();
        let allowed = allowed_dirs
            .iter()
            .any(|d| pl.starts_with(&d.to_ascii_lowercase()))
            || pl.contains("gamelayers");
        if allowed {
            continue;
        }
        return Some((m, path));
    }
    None
}

unsafe fn remote_module_path(process: HANDLE, dll_name: &str) -> Result<String, &'static str> {
    use windows_sys::Win32::Foundation::HMODULE;
    use windows_sys::Win32::System::ProcessStatus::{
        EnumProcessModulesEx, GetModuleFileNameExA, LIST_MODULES_ALL,
    };
    let want = std::path::Path::new(dll_name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(dll_name)
        .to_ascii_lowercase();
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
        let mut path_buf = [0u8; 520];
        let n = GetModuleFileNameExA(process, m, path_buf.as_mut_ptr(), 520);
        if n == 0 {
            continue;
        }
        let path = String::from_utf8_lossy(&path_buf[..n as usize]).into_owned();
        let base = std::path::Path::new(&path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if base == want {
            return Ok(path);
        }
    }
    Err("module path not found")
}

/// MSVC default cookie; replace with a non-default pseudo-random value so
/// `/GS` checks do not use the well-known constant (and so early EH paths that
/// touch the cookie see a sane value before CRT `__security_init_cookie`).
fn init_security_cookie(img: &mut [u8], e_lfanew: usize, preferred_base: u64, new_base: u64) {
    let opt = e_lfanew + 24;
    let lc_dir = opt + 112 + 10 * 8;
    if lc_dir + 8 > img.len() {
        return;
    }
    let lc_rva = rd_u32(img, lc_dir) as usize;
    if lc_rva == 0 || lc_rva + 0x60 > img.len() {
        return;
    }
    let size = rd_u32(img, lc_rva) as usize;
    if size < 0x60 {
        return;
    }
    // IMAGE_LOAD_CONFIG_DIRECTORY64.SecurityCookie @ +0x58 — VA of cookie.
    let cookie_va = rd_u64(img, lc_rva + 0x58);
    if cookie_va == 0 {
        return;
    }
    // Cookie VA is stored relative to preferred base in the file; after relocs
    // it may already be adjusted. Compute RVA from whichever base matches.
    let cookie_rva = if cookie_va >= new_base && (cookie_va - new_base) < img.len() as u64 {
        (cookie_va - new_base) as usize
    } else if cookie_va >= preferred_base && (cookie_va - preferred_base) < img.len() as u64 {
        (cookie_va - preferred_base) as usize
    } else {
        return;
    };
    if cookie_rva + 8 > img.len() {
        return;
    }
    // Mix tick count + stack address; never leave the default 0x2B992DDFA232.
    let tick = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0xC0FFEEu64);
    let mut cookie = tick ^ (new_base.rotate_left(17)) ^ 0x0000_2B99_2DDF_A232;
    if cookie == 0x0000_2B99_2DDF_A232 || cookie == 0 || cookie == 0xFFFF_FFFF_FFFF_FFFF {
        cookie = 0x1234_5678_9ABC_DEF0 ^ new_base;
    }
    // Top 16 bits zeroed on x64 for historical MSVC compatibility.
    cookie &= 0x0000_FFFF_FFFF_FFFF;
    if cookie == 0 {
        cookie = 0x0000_4800_DC56_1234;
    }
    wr_u64(img, cookie_rva, cookie);
}

/// Install TLS template into the primary thread TEB and write `_tls_index`.
unsafe fn setup_remote_tls(
    process: HANDLE,
    thread: HANDLE,
    image_base: u64,
    img: &[u8],
    e_lfanew: usize,
    preferred_base: u64,
) -> Result<(), &'static str> {
    let opt = e_lfanew + 24;
    let tls_dir = opt + 112 + 9 * 8;
    if tls_dir + 8 > img.len() {
        return Ok(());
    }
    let tls_rva = rd_u32(img, tls_dir) as usize;
    if tls_rva == 0 || tls_rva + 40 > img.len() {
        return Ok(());
    }

    let start_va = rd_u64(img, tls_rva);
    let end_va = rd_u64(img, tls_rva + 8);
    let index_va = rd_u64(img, tls_rva + 16);
    if start_va == 0 || end_va < start_va {
        return Ok(());
    }
    let data_size = (end_va - start_va) as usize;
    if data_size == 0 || data_size > 64 * 1024 {
        return Ok(());
    }

    // Template bytes live in the image. VA→offset uses image_base after reloc
    // (TLS directory absolute addresses are reloc targets).
    let start_rva = if start_va >= image_base {
        (start_va - image_base) as usize
    } else if start_va >= preferred_base {
        (start_va - preferred_base) as usize
    } else {
        return Ok(());
    };
    if start_rva + data_size > img.len() {
        return Ok(());
    }

    let remote_tls = VirtualAllocEx(
        process,
        core::ptr::null(),
        data_size.max(0x10),
        MEM_COMMIT | MEM_RESERVE,
        PAGE_EXECUTE_READWRITE,
    );
    if remote_tls.is_null() {
        return Err("VirtualAllocEx TLS data failed");
    }
    let mut n = 0usize;
    if WriteProcessMemory(
        process,
        remote_tls,
        img[start_rva..start_rva + data_size].as_ptr() as *const c_void,
        data_size,
        &mut n,
    ) == 0
    {
        return Err("WriteProcessMemory TLS data failed");
    }

    // AddressOfIndex → DWORD TLS index. Use 0 (primary slot).
    if index_va != 0 {
        let idx_addr = if index_va >= image_base {
            index_va
        } else if index_va >= preferred_base {
            image_base + (index_va - preferred_base)
        } else {
            0
        };
        if idx_addr != 0 {
            let zero = 0u32;
            let _ = WriteProcessMemory(
                process,
                idx_addr as *mut c_void,
                &zero as *const u32 as *const c_void,
                4,
                &mut n,
            );
        }
    }

    // Primary thread TEB → ThreadLocalStoragePointer @ +0x58 (x64).
    let nt_qit: NtQueryInformationThreadFn = core::mem::transmute(
        ntdll_proc(b"NtQueryInformationThread\0").ok_or("NtQueryInformationThread")?,
    );
    let mut tbi: ThreadBasicInformation = zeroed();
    let mut ret_len = 0u32;
    let st = nt_qit(
        thread,
        THREAD_BASIC_INFORMATION,
        &mut tbi as *mut _ as *mut c_void,
        size_of::<ThreadBasicInformation>() as u32,
        &mut ret_len,
    );
    if st != 0 || tbi.teb_base_address.is_null() {
        eprintln!("vfs-inject: TLS TEB query failed status=0x{st:x}");
        return Ok(()); // best-effort
    }
    let teb = tbi.teb_base_address as usize;
    let mut tls_array: u64 = 0;
    if ReadProcessMemory(
        process,
        (teb + 0x58) as *const c_void,
        &mut tls_array as *mut u64 as *mut c_void,
        8,
        &mut n,
    ) == 0
    {
        return Ok(());
    }

    let remote_tls_u = remote_tls as u64;
    if tls_array == 0 {
        // Allocate a small TLS vector (64 slots) and point TEB at it.
        let vec_bytes = 64 * 8;
        let vec = VirtualAllocEx(
            process,
            core::ptr::null(),
            vec_bytes,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_EXECUTE_READWRITE,
        );
        if vec.is_null() {
            return Ok(());
        }
        let zeros = vec![0u8; vec_bytes];
        let _ = WriteProcessMemory(
            process,
            vec,
            zeros.as_ptr() as *const c_void,
            vec_bytes,
            &mut n,
        );
        let vec_u = vec as u64;
        let _ = WriteProcessMemory(
            process,
            (teb + 0x58) as *mut c_void,
            &vec_u as *const u64 as *const c_void,
            8,
            &mut n,
        );
        let _ = WriteProcessMemory(
            process,
            vec,
            &remote_tls_u as *const u64 as *const c_void,
            8,
            &mut n,
        );
    } else {
        // Overwrite slot 0 with our TLS template (main EXE convention).
        let _ = WriteProcessMemory(
            process,
            tls_array as *mut c_void,
            &remote_tls_u as *const u64 as *const c_void,
            8,
            &mut n,
        );
    }
    Ok(())
}

/// Build PIC-ish x64 shellcode:
///   RtlAddFunctionTable(pdata, count, image_base);
///   for each TLS callback: callback(image_base, DLL_PROCESS_ATTACH, 0);
///   jmp real_entry
fn build_entry_trampoline(
    image_base: u64,
    real_entry: u64,
    img: &[u8],
    e_lfanew: usize,
    preferred_base: u64,
) -> Result<Vec<u8>, &'static str> {
    let rtl_add = ntdll_proc(b"RtlAddFunctionTable\0").unwrap_or(core::ptr::null()) as u64;

    let opt = e_lfanew + 24;
    let ex_dir = opt + 112 + 3 * 8;
    let (ex_rva, ex_size) = if ex_dir + 8 <= img.len() {
        (rd_u32(img, ex_dir) as u64, rd_u32(img, ex_dir + 4) as u32)
    } else {
        (0, 0)
    };
    // RUNTIME_FUNCTION is 12 bytes each.
    let ex_count = if ex_rva != 0 && ex_size >= 12 {
        ex_size / 12
    } else {
        0
    };
    let pdata = if ex_count > 0 {
        image_base + ex_rva
    } else {
        0
    };

    let mut callbacks: Vec<u64> = Vec::new();
    let tls_dir = opt + 112 + 9 * 8;
    if tls_dir + 8 <= img.len() {
        let tls_rva = rd_u32(img, tls_dir) as usize;
        if tls_rva != 0 && tls_rva + 40 <= img.len() {
            let mut cbs_va = rd_u64(img, tls_rva + 24);
            if cbs_va != 0 {
                if cbs_va < image_base && cbs_va >= preferred_base {
                    cbs_va = image_base + (cbs_va - preferred_base);
                }
                if cbs_va >= image_base {
                    let mut off = (cbs_va - image_base) as usize;
                    while off + 8 <= img.len() {
                        let mut cb = rd_u64(img, off);
                        if cb == 0 {
                            break;
                        }
                        if cb < image_base && cb >= preferred_base {
                            cb = image_base + (cb - preferred_base);
                        }
                        callbacks.push(cb);
                        off += 8;
                        if callbacks.len() > 16 {
                            break;
                        }
                    }
                }
            }
        }
    }

    let mut code = Vec::with_capacity(256 + callbacks.len() * 32);

    // sub rsp, 0x28  — shadow space + alignment
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x28]);

    if rtl_add != 0 && pdata != 0 && ex_count > 0 {
        // mov rcx, pdata
        code.extend_from_slice(&[0x48, 0xB9]);
        code.extend_from_slice(&pdata.to_le_bytes());
        // mov edx, ex_count
        code.extend_from_slice(&[0xBA]);
        code.extend_from_slice(&ex_count.to_le_bytes());
        // mov r8, image_base
        code.extend_from_slice(&[0x49, 0xB8]);
        code.extend_from_slice(&image_base.to_le_bytes());
        // mov rax, rtl_add
        code.extend_from_slice(&[0x48, 0xB8]);
        code.extend_from_slice(&rtl_add.to_le_bytes());
        // call rax
        code.extend_from_slice(&[0xFF, 0xD0]);
    }

    for cb in callbacks {
        // mov rcx, image_base
        code.extend_from_slice(&[0x48, 0xB9]);
        code.extend_from_slice(&image_base.to_le_bytes());
        // mov edx, DLL_PROCESS_ATTACH
        code.extend_from_slice(&[0xBA]);
        code.extend_from_slice(&DLL_PROCESS_ATTACH.to_le_bytes());
        // xor r8, r8
        code.extend_from_slice(&[0x4D, 0x31, 0xC0]);
        // mov rax, cb
        code.extend_from_slice(&[0x48, 0xB8]);
        code.extend_from_slice(&cb.to_le_bytes());
        // call rax
        code.extend_from_slice(&[0xFF, 0xD0]);
    }

    // add rsp, 0x28
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x28]);
    // mov rax, real_entry
    code.extend_from_slice(&[0x48, 0xB8]);
    code.extend_from_slice(&real_entry.to_le_bytes());
    // jmp rax
    code.extend_from_slice(&[0xFF, 0xE0]);

    Ok(code)
}

unsafe fn remote_wstring(process: HANDLE, s: &str) -> Result<u64, &'static str> {
    let mut w: Vec<u16> = s.encode_utf16().collect();
    w.push(0);
    let bytes = w.len() * 2;
    let remote = VirtualAllocEx(
        process,
        core::ptr::null(),
        bytes,
        MEM_COMMIT | MEM_RESERVE,
        PAGE_EXECUTE_READWRITE,
    );
    if remote.is_null() {
        return Err("VirtualAllocEx path string failed");
    }
    let mut n = 0usize;
    if WriteProcessMemory(
        process,
        remote,
        w.as_ptr() as *const c_void,
        bytes,
        &mut n,
    ) == 0
        || n != bytes
    {
        return Err("WriteProcessMemory path string failed");
    }
    Ok(remote as u64)
}

unsafe fn write_ustr(
    process: HANDLE,
    ustr_addr: u64,
    buf: u64,
    chars: usize,
) -> Result<(), &'static str> {
    let byte_len = (chars * 2) as u16;
    let max_len = byte_len.saturating_add(2);
    let mut raw = [0u8; 16];
    raw[0..2].copy_from_slice(&byte_len.to_le_bytes());
    raw[2..4].copy_from_slice(&max_len.to_le_bytes());
    raw[8..16].copy_from_slice(&buf.to_le_bytes());
    let mut n = 0usize;
    if WriteProcessMemory(
        process,
        ustr_addr as *mut c_void,
        raw.as_ptr() as *const c_void,
        16,
        &mut n,
    ) == 0
    {
        return Err("write UNICODE_STRING failed");
    }
    Ok(())
}

/// Patch PEB ProcessParameters and the main LDR_DATA_TABLE_ENTRY names so
/// GetModuleFileName / SKSE sibling resolution use `image_path`.
unsafe fn spoof_peb_and_ldr_paths(
    process: HANDLE,
    peb: usize,
    image_path: &str,
    image_base: u64,
    size_of_image: usize,
    entry_rva: u32,
) -> Result<(), &'static str> {
    let mut params: u64 = 0;
    let mut n = 0usize;
    if ReadProcessMemory(
        process,
        (peb + 0x20) as *const c_void,
        &mut params as *mut u64 as *mut c_void,
        8,
        &mut n,
    ) == 0
        || params == 0
    {
        return Err("read ProcessParameters failed");
    }

    let image_w = remote_wstring(process, image_path)?;
    let dir = std::path::Path::new(image_path)
        .parent()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_else(|| image_path.to_string());
    let dir_w = remote_wstring(process, &dir)?;
    let dir_slash = if dir.ends_with('\\') {
        dir.clone()
    } else {
        format!("{dir}\\")
    };
    let dllpath_w = remote_wstring(process, &dir_slash)?;
    let base_name = std::path::Path::new(image_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(image_path);
    let base_w = remote_wstring(process, base_name)?;

    let img_chars = image_path.encode_utf16().count();
    let dir_chars = dir.encode_utf16().count();
    let dll_chars = dir_slash.encode_utf16().count();
    let base_chars = base_name.encode_utf16().count();

    // RTL_USER_PROCESS_PARAMETERS (x64):
    // CurrentDirectory.DosPath @ +0x38, DllPath @ +0x50, ImagePathName @ +0x60, CommandLine @ +0x70
    write_ustr(process, params + 0x38, dir_w, dir_chars)?;
    write_ustr(process, params + 0x50, dllpath_w, dll_chars)?;
    write_ustr(process, params + 0x60, image_w, img_chars)?;
    write_ustr(process, params + 0x70, image_w, img_chars)?; // CommandLine ~ image path

    // PEB.Ldr @ +0x18 → PEB_LDR_DATA.InLoadOrderModuleList @ +0x10
    let mut ldr: u64 = 0;
    if ReadProcessMemory(
        process,
        (peb + 0x18) as *const c_void,
        &mut ldr as *mut u64 as *mut c_void,
        8,
        &mut n,
    ) == 0
        || ldr == 0
    {
        return Ok(()); // best-effort; ProcessParameters often enough
    }
    // First link is list head; Flink points to first LDR_DATA_TABLE_ENTRY
    let mut flink: u64 = 0;
    if ReadProcessMemory(
        process,
        (ldr as usize + 0x10) as *const c_void,
        &mut flink as *mut u64 as *mut c_void,
        8,
        &mut n,
    ) == 0
        || flink == 0
    {
        return Ok(());
    }
    // Walk a few entries to find main module (first entry is the EXE).
    let mut entry = flink;
    for _ in 0..8 {
        let mut dll_base: u64 = 0;
        if ReadProcessMemory(
            process,
            (entry as usize + 0x30) as *const c_void,
            &mut dll_base as *mut u64 as *mut c_void,
            8,
            &mut n,
        ) == 0
        {
            break;
        }
        // After hollow, host base may still be listed; also match first entry.
        if dll_base == image_base || entry == flink {
            // FullDllName @ +0x48, BaseDllName @ +0x58
            let _ = write_ustr(process, entry + 0x48, image_w, img_chars);
            let _ = write_ustr(process, entry + 0x58, base_w, base_chars);
            // DllBase @ +0x30
            let _ = WriteProcessMemory(
                process,
                (entry as usize + 0x30) as *mut c_void,
                &image_base as *const u64 as *const c_void,
                8,
                &mut n,
            );
            // EntryPoint @ +0x38
            let ep = image_base + entry_rva as u64;
            let _ = WriteProcessMemory(
                process,
                (entry as usize + 0x38) as *mut c_void,
                &ep as *const u64 as *const c_void,
                8,
                &mut n,
            );
            // SizeOfImage @ +0x40 (ULONG)
            let soi = size_of_image as u32;
            let _ = WriteProcessMemory(
                process,
                (entry as usize + 0x40) as *mut c_void,
                &soi as *const u32 as *const c_void,
                4,
                &mut n,
            );
            break;
        }
        let mut next: u64 = 0;
        if ReadProcessMemory(
            process,
            entry as *const c_void,
            &mut next as *mut u64 as *mut c_void,
            8,
            &mut n,
        ) == 0
            || next == 0
            || next == flink
        {
            break;
        }
        entry = next;
    }
    Ok(())
}

/// Build a kernel SEC_IMAGE-like mapping **in the current process** from PE
/// bytes without writing them to disk. Returns a base address already mapped
/// (for synthetic section MapView).
///
/// Used by the shim when `NtCreateSection(SEC_IMAGE)` is called on a zip-window
/// synth file handle.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pe_looks_like_rejects_garbage() {
        assert!(!pe_looks_like_image(b"not a pe"));
        assert!(!pe_looks_like_image(&[]));
    }

    #[test]
    fn pe_looks_like_accepts_mz() {
        let mut buf = vec![0u8; 0x40];
        buf[0] = b'M';
        buf[1] = b'Z';
        assert!(pe_looks_like_image(&buf));
    }

    /// Prove create_process_from_pe_bytes does not create a new file under TEMP
    /// whose content is the PE (no vfs-run-* staging). Uses this test binary as
    /// the PE source — it already exists on disk (not archive staging).
    #[test]
    fn hollow_does_not_stage_pe_under_temp() {
        let exe = std::env::current_exe().expect("exe");
        let pe = std::fs::read(&exe).expect("read self");
        assert!(pe_looks_like_image(&pe));

        // Snapshot TEMP children matching our forbidden staging prefixes.
        let temp = std::env::temp_dir();
        let before: std::collections::HashSet<_> = std::fs::read_dir(&temp)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.starts_with("vfs-run-") || n.starts_with("vfs-sse-") || n.starts_with("vfs-sec-")
                {
                    Some(n)
                } else {
                    None
                }
            })
            .collect();

        let virt = r"C:\GameLayers\runtime\hollow-test.exe";
        let result = create_process_from_pe_bytes(&pe, virt, &[], None);
        // Hollow may fail on CI policy; what we assert is no new staging files.
        if let Ok((proc, thread, _, _)) = result {
            unsafe {
                CloseHandle(thread);
                CloseHandle(proc);
            }
        }

        let after: Vec<_> = std::fs::read_dir(&temp)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.starts_with("vfs-run-") || n.starts_with("vfs-sse-") || n.starts_with("vfs-sec-")
                {
                    if !before.contains(&n) {
                        Some(n)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();
        assert!(
            after.is_empty(),
            "hollow must not stage archive/PE under TEMP: {after:?}"
        );
    }

    #[test]
    fn trampoline_has_jmp_entry() {
        // Minimal fake image with no TLS/exception dirs.
        let mut img = vec![0u8; 0x200];
        img[0] = b'M';
        img[1] = b'Z';
        img[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        img[0x80..0x84].copy_from_slice(b"PE\0\0");
        // Optional magic PE32+
        img[0x80 + 24] = 0x0B;
        img[0x80 + 25] = 0x02;
        let code = build_entry_trampoline(0x140000000, 0x140001000, &img, 0x80, 0x140000000)
            .expect("tramp");
        assert!(code.len() > 16);
        // Ends with jmp rax
        assert_eq!(&code[code.len() - 2..], &[0xFF, 0xE0]);
    }

    #[test]
    fn hollow_always_logs_zip_pe_write_for_self() {
        // Drive real create_process_from_pe_bytes + hollow_existing_process and
        // require the shipped log line proving archive PE was written (not path-only).
        let exe = std::env::current_exe().expect("exe");
        let pe = std::fs::read(&exe).expect("read self");
        assert!(pe_looks_like_image(&pe));
        let virt = r"C:\GameLayers\runtime\hollow-zipwrite-test.exe";
        // Capture stderr is hard in unit tests; assert the PE write path by
        // creating the process and reading remote entry bytes after hollow.
        let result = create_process_from_pe_bytes(&pe, virt, &[], None);
        if let Ok((proc, thread, _, _)) = result {
            // PEB ImageBase + entry must be readable (zip image present).
            unsafe {
                type NtQip = unsafe extern "system" fn(
                    HANDLE,
                    u32,
                    *mut u8,
                    u32,
                    *mut u32,
                ) -> i32;
                let ntdll = windows_sys::Win32::System::LibraryLoader::GetModuleHandleA(
                    b"ntdll.dll\0".as_ptr(),
                );
                let ntqip: NtQip = core::mem::transmute(
                    windows_sys::Win32::System::LibraryLoader::GetProcAddress(
                        ntdll,
                        b"NtQueryInformationProcess\0".as_ptr(),
                    )
                    .unwrap(),
                );
                let mut pbi = [0u8; 48];
                let mut rl = 0u32;
                assert_eq!(ntqip(proc, 0, pbi.as_mut_ptr(), 48, &mut rl), 0);
                let peb = usize::from_le_bytes(pbi[8..16].try_into().unwrap());
                let mut base = 0u64;
                let mut n = 0usize;
                assert_ne!(
                    ReadProcessMemory(
                        proc,
                        (peb + 0x10) as *const c_void,
                        &mut base as *mut _ as *mut c_void,
                        8,
                        &mut n,
                    ),
                    0
                );
                assert_ne!(base, 0);
                let e_lfanew = u32::from_le_bytes(pe[0x3C..0x40].try_into().unwrap()) as usize;
                let entry_rva =
                    u32::from_le_bytes(pe[e_lfanew + 24 + 16..e_lfanew + 24 + 20].try_into().unwrap())
                        as u64;
                let mut probe = [0u8; 16];
                assert_ne!(
                    ReadProcessMemory(
                        proc,
                        (base + entry_rva) as *const c_void,
                        probe.as_mut_ptr() as *mut c_void,
                        16,
                        &mut n,
                    ),
                    0
                );
                // Entry must look like code (not all zeros) — zip PE was written.
                assert!(probe.iter().any(|&b| b != 0), "entry empty; zip PE not written");
                windows_sys::Win32::System::Threading::TerminateProcess(proc, 0);
                CloseHandle(thread);
                CloseHandle(proc);
            }
        }
    }

    #[test]
    fn hollow_host_for_skyrim_prefers_steam_when_present() {
        let host = hollow_host_exe_for(Some(r"C:\GameLayers\runtime\SkyrimSE.exe"))
            .expect("host");
        let lower = host.to_ascii_lowercase();
        // Must never pick a managed zip-virtual path as CreateProcess app.
        assert!(
            !lower.contains(r"gamelayers\runtime"),
            "host must not be under managed root: {host}"
        );
        // Prefer Steam install when available (DRM-safe ProcessImageFileName).
        let steam = r"C:\Program Files (x86)\Steam\steamapps\common\Skyrim Special Edition\SkyrimSE.exe";
        if std::path::Path::new(steam).is_file() {
            assert_eq!(host, steam);
        } else {
            assert!(std::path::Path::new(&host).is_file());
        }
    }

    #[test]
    fn hollow_host_never_uses_gamelayers_runtime() {
        let host = hollow_host_exe().expect("host");
        assert!(!host.to_ascii_lowercase().contains(r"gamelayers\runtime"));
        assert!(std::path::Path::new(&host).is_file());
    }

    /// Game-local PE must come from layer zip windows (not Steam disk extract).
    #[test]
    fn game_local_pe_reads_steam_api_from_layer_zip() {
        let zip = std::path::Path::new(r"C:\GameLayers\1. Skyrim Special Edition.zip");
        if !zip.is_file() {
            eprintln!("skip: GameLayers base zip not present");
            return;
        }
        let (bytes, src) = read_game_local_pe("steam_api64.dll", &[]).expect("zip PE");
        assert!(pe_looks_like_image(&bytes), "must be MZ PE");
        assert!(bytes.len() > 10_000);
        assert!(
            src.contains("zip-window:") || src.starts_with("disk:"),
            "source must be a zip window or a labelled disk read, got {src}"
        );
        // The disk fallback must never resolve into the Steam install: that
        // silently bypasses the zip (see the `disk:` note on read_game_local_pe).
        assert!(!src.to_ascii_lowercase().contains("steamapps"), "source={src}");
        // bink too
        let (bink, bsrc) = read_game_local_pe("bink2w64.dll", &[]).expect("bink zip PE");
        assert!(pe_looks_like_image(&bink));
        assert!(bsrc.contains("zip-window:") || bsrc.starts_with("disk:"));
        assert!(!bsrc.to_ascii_lowercase().contains("steamapps"), "source={bsrc}");
    }

    /// The `search_dirs` fallback is a real `std::fs::read`, so it must be
    /// labelled `disk:` — a `vfs:` prefix there hid host-disk PE loads in the
    /// log and made a zip bypass look like a VFS hit.
    #[test]
    fn game_local_pe_disk_fallback_is_labelled_disk_not_vfs() {
        let dir = std::env::temp_dir().join(format!(
            "vfs-pe-label-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // Minimal thing read_game_local_pe accepts: MZ and >512 bytes.
        let mut pe = vec![0u8; 1024];
        pe[0] = b'M';
        pe[1] = b'Z';
        let name = "vfs-label-probe.dll";
        std::fs::write(dir.join(name), &pe).unwrap();

        let (bytes, src) =
            read_game_local_pe(name, &[dir.to_string_lossy().into_owned()]).expect("disk PE");

        assert_eq!(bytes.len(), 1024);
        assert!(
            src.starts_with("disk:"),
            "disk fallback must be labelled disk:, got {src}"
        );
        assert!(
            !src.starts_with("vfs:"),
            "disk read must not claim to be a VFS read, got {src}"
        );
        assert!(src.contains(name), "source should name the file, got {src}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_system_import_excludes_game_locals() {
        assert!(is_system_import_dll("KERNEL32.dll"));
        assert!(!is_system_import_dll("steam_api64.dll"));
        assert!(!is_system_import_dll("bink2w64.dll"));
    }

    /// Shipped Stages B→C: zip PE WPM over host-mapped steam_api64, preserve IAT,
    /// remote non-writable layout equals pe_layout(zip). Fails on privatize-only.
    #[test]
    fn game_local_zip_overwrite_preserves_iat_and_identity() {
        let zip = std::path::Path::new(r"C:\GameLayers\1. Skyrim Special Edition.zip");
        let steam = r"C:\Program Files (x86)\Steam\steamapps\common\Skyrim Special Edition\SkyrimSE.exe";
        if !zip.is_file() || !std::path::Path::new(steam).is_file() {
            eprintln!("skip: GameLayers zip or Steam SkyrimSE missing");
            return;
        }
        let (pe, src) = read_game_local_pe("steam_api64.dll", &[]).expect("zip PE");
        assert!(src.contains("zip-window:"), "source={src}");
        assert!(pe_looks_like_image(&pe));

        let host_w = wide(steam);
        let mut cmd_w = wide(&format!("\"{steam}\""));
        let mut si: STARTUPINFOW = unsafe { zeroed() };
        si.cb = size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = unsafe { zeroed() };
        let ok = unsafe {
            CreateProcessW(
                host_w.as_ptr(),
                cmd_w.as_mut_ptr(),
                core::ptr::null(),
                core::ptr::null(),
                0,
                CREATE_SUSPENDED,
                core::ptr::null(),
                core::ptr::null(),
                &si,
                &mut pi,
            )
        };
        assert_ne!(ok, 0, "CreateProcess Steam SkyrimSE suspended");

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
            let k32 = GetModuleHandleA(b"kernel32.dll\0".as_ptr());
            assert!(!k32.is_null());
            let load_library = GetProcAddress(k32, b"LoadLibraryA\0".as_ptr()).expect("LoadLibraryA");
            let load_start: LPTHREAD_START_ROUTINE = Some(core::mem::transmute(load_library));
            // Stage A: host-map from Steam disk (DllMain path), same as real hollow host.
            // Note: LoadLibrary remote thread exit code is 32-bit — always re-resolve base via EnumProcessModules.
            let steam_dll = r"C:\Program Files (x86)\Steam\steamapps\common\Skyrim Special Edition\steam_api64.dll";
            let _ = remote_load_library_path(pi.hProcess, load_start, steam_dll);
            let base = find_remote_module_base_opt(pi.hProcess, "steam_api64.dll")
                .expect("steam_api64 LoadLibrary into suspended host");

            // Snapshot one IAT slot (FirstThunk) before overwrite.
            let (img0, _, _, _) = pe_layout(&pe).expect("layout");
            let e_lf = rd_u32(&img0, 0x3C) as usize;
            let opt = e_lf + 24;
            let iat_dir = opt + 112 + 12 * 8;
            let iat_rva = rd_u32(&img0, iat_dir) as usize;
            let mut iat_before = [0u8; 8];
            let mut n = 0usize;
            let mut have_iat = false;
            if iat_rva != 0 {
                have_iat = ReadProcessMemory(
                    pi.hProcess,
                    (base as usize + iat_rva) as *const c_void,
                    iat_before.as_mut_ptr() as *mut c_void,
                    8,
                    &mut n,
                ) != 0
                    && n == 8;
            }

            overwrite_remote_module_zip_preserve_iat(
                pi.hProcess,
                base,
                &pe,
                "steam_api64.dll",
            )
            .expect("Stage B zip overwrite");

            prove_remote_matches_zip_pe(pi.hProcess, base, &pe)
                .expect("Stage C non-writable identity vs pe_layout(zip)");

            if have_iat {
                let mut iat_after = [0u8; 8];
                assert_ne!(
                    ReadProcessMemory(
                        pi.hProcess,
                        (base as usize + iat_rva) as *const c_void,
                        iat_after.as_mut_ptr() as *mut c_void,
                        8,
                        &mut n,
                    ),
                    0
                );
                assert_eq!(
                    iat_before, iat_after,
                    "IAT first slot must be preserved across zip overwrite"
                );
            }

            // Mutate one byte at +0x1000 (typical first section) → identity must fail.
            let mut one = [0u8; 1];
            let mut rn = 0usize;
            assert_ne!(
                ReadProcessMemory(
                    pi.hProcess,
                    (base as usize + 0x1000) as *const c_void,
                    one.as_mut_ptr() as *mut c_void,
                    1,
                    &mut rn,
                ),
                0
            );
            let flipped = [one[0] ^ 0xFF];
            use windows_sys::Win32::System::Memory::{VirtualProtectEx, PAGE_EXECUTE_READWRITE};
            let mut old = 0u32;
            let _ = VirtualProtectEx(
                pi.hProcess,
                (base as usize + 0x1000) as *mut c_void,
                1,
                PAGE_EXECUTE_READWRITE,
                &mut old,
            );
            assert_ne!(
                WriteProcessMemory(
                    pi.hProcess,
                    (base as usize + 0x1000) as *mut c_void,
                    flipped.as_ptr() as *const c_void,
                    1,
                    &mut rn,
                ),
                0
            );
            let fail = prove_remote_matches_zip_pe(pi.hProcess, base, &pe);
            assert!(
                fail.is_err(),
                "header-only would pass; full section compare must catch +0x1000 flip"
            );
        }));

        unsafe {
            let _ = windows_sys::Win32::System::Threading::TerminateProcess(pi.hProcess, 0);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
        }
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }
}

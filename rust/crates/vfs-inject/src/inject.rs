//! Win32 injection FFI: LoadLibrary (post-init full shim) and reflective-map +
//! RIP-redirect (pre-init early payload).
#![allow(unsafe_code)]

use core::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::time::Instant;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::Debug::{
    FlushInstructionCache, GetThreadContext, SetThreadContext, WriteProcessMemory, CONTEXT,
};

const CONTEXT_FULL: u32 = 0x0010_000B;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Memory::{
    VirtualAllocEx, MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE, PAGE_READWRITE,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, CreateRemoteThread, GetExitCodeProcess, ResumeThread,
    WaitForSingleObject, CREATE_SUSPENDED, INFINITE, LPTHREAD_START_ROUTINE, PROCESS_INFORMATION,
    STARTUPINFOW,
};

use crate::artifacts::resolve_payload_for_run;
use crate::map::{apply_relocs, build_image, export_rva};
use crate::payload_cfg::{PayloadConfig, RedirectEntry, MAX_REDIRECTS};
use crate::static_imports::{load_preinit_from_config_file, StaticImport};
use crate::stub::build_stub;
use crate::{InjectError, PreinitConfig, PreinitRedirect, RunConfig};


/// Build the early redirect table: config-file static imports first, then any
/// explicit `extra` rows (caller overrides). Caps at [`MAX_REDIRECTS`].
pub fn merge_preinit_redirects(config_path: &str, extra: &[PreinitRedirect]) -> Vec<PreinitRedirect> {
    let mut out = load_preinit_from_config_file(config_path, MAX_REDIRECTS);
    for e in extra {
        if let Some(i) = out.iter().position(|x| x.suffix.eq_ignore_ascii_case(&e.suffix)) {
            out[i] = PreinitRedirect {
                suffix: e.suffix.clone(),
                backing_nt: e.backing_nt.clone(),
                backing_size: e.backing_size,
            };
        } else if out.len() < MAX_REDIRECTS {
            out.push(PreinitRedirect {
                suffix: e.suffix.clone(),
                backing_nt: e.backing_nt.clone(),
                backing_size: e.backing_size,
            });
        }
    }
    out
}

/// Parse static-import rows from a config file (VFS1 section).
pub fn load_static_imports_from_config(path: &str) -> Vec<StaticImport> {
    crate::static_imports::load_static_imports_from_path(path).unwrap_or_default()
}

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

fn ntdll_proc(name: &[u8]) -> Result<usize, InjectError> {
    // SAFETY: ntdll is always mapped in the injector; addresses are valid in the
    // target too (session-shared base).
    unsafe {
        let h = GetModuleHandleW(wide("ntdll.dll").as_ptr());
        if h.is_null() {
            return Err(InjectError::Ntdll);
        }
        match GetProcAddress(h, name.as_ptr()) {
            Some(p) => Ok(p as usize),
            None => Err(InjectError::Ntdll),
        }
    }
}

unsafe fn vae(process: HANDLE, size: usize, exec: bool) -> Result<u64, InjectError> {
    let prot = if exec {
        PAGE_EXECUTE_READWRITE
    } else {
        PAGE_READWRITE
    };
    let p = VirtualAllocEx(
        process,
        core::ptr::null(),
        size,
        MEM_COMMIT | MEM_RESERVE,
        prot,
    );
    if p.is_null() {
        return Err(InjectError::Alloc);
    }
    Ok(p as u64)
}

unsafe fn wpm(process: HANDLE, addr: u64, data: &[u8]) -> Result<(), InjectError> {
    let mut n = 0usize;
    let ok = WriteProcessMemory(
        process,
        addr as *const c_void,
        data.as_ptr() as *const c_void,
        data.len(),
        &mut n,
    );
    if ok == 0 || n != data.len() {
        return Err(InjectError::Write);
    }
    Ok(())
}

/// Grow the *primary* suspended thread's stack to `stack_reserve` bytes.
///
/// Keeps CreateProcess image = Steam library path (DRM) while avoiding the
/// stock 1 MiB SkyrimSE stack that overflows under VFS hooks (`0xC00000FD`).
///
/// Unlike a bare RSP pivot onto an empty region (which broke Steam DRM /
/// RtlUserThreadStart), this:
/// 1. reads the live TEB StackBase + current RSP,
/// 2. copies `[RSP, StackBase)` onto the high end of a new reservation,
/// 3. adjusts RSP by the same delta, then updates TEB stack fields.
unsafe fn expand_primary_stack(
    process: HANDLE,
    primary: HANDLE,
    stack_reserve: usize,
) -> Result<(), InjectError> {
    type NtQueryInformationThreadFn = unsafe extern "system" fn(
        HANDLE,
        u32,
        *mut c_void,
        u32,
        *mut u32,
    ) -> i32;

    let ntdll = GetModuleHandleW(wide("ntdll.dll").as_ptr());
    if ntdll.is_null() {
        return Err(InjectError::Ntdll);
    }
    let nt_qit: NtQueryInformationThreadFn =
        match GetProcAddress(ntdll, b"NtQueryInformationThread\0".as_ptr()) {
            Some(p) => core::mem::transmute(p),
            None => return Err(InjectError::Ntdll),
        };

    // THREADINFOCLASS ThreadBasicInformation = 0
    // struct { ExitStatus, TebBaseAddress, ClientId, Affinity, Priority, BasePriority }
    #[repr(C)]
    struct ThreadBasicInformation {
        exit_status: i32,
        teb_base: u64,
        client_id: [u64; 2],
        affinity: u64,
        priority: i32,
        base_priority: i32,
    }
    let mut tbi: ThreadBasicInformation = zeroed();
    let mut ret_len = 0u32;
    let st = nt_qit(
        primary,
        0,
        &mut tbi as *mut _ as *mut c_void,
        size_of::<ThreadBasicInformation>() as u32,
        &mut ret_len,
    );
    if st != 0 || tbi.teb_base == 0 {
        eprintln!("vfs-inject: NtQueryInformationThread TEB failed status={st:x}");
        return Err(InjectError::CreateProcess);
    }

    let teb = tbi.teb_base as *mut c_void;
    // TEB x64: StackBase=0x08, StackLimit=0x10, DeallocationStack=0x1478
    let mut old_base = 0u64;
    let mut old_limit = 0u64;
    let mut old_dealloc = 0u64;
    let mut read_n = 0usize;
    for (off, dst) in [
        (0x08u64, &mut old_base),
        (0x10u64, &mut old_limit),
        (0x1478u64, &mut old_dealloc),
    ] {
        let ok = windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory(
            process,
            (teb as u64 + off) as *const c_void,
            dst as *mut u64 as *mut c_void,
            8,
            &mut read_n,
        );
        if ok == 0 || read_n != 8 || *dst == 0 {
            eprintln!("vfs-inject: TEB stack field read failed off={off:#x}");
            return Err(InjectError::CreateProcess);
        }
    }

    let mut ctx_buf = vec![0u8; size_of::<CONTEXT>() + 16];
    let ctx_addr = (ctx_buf.as_mut_ptr() as usize + 15) & !15;
    let ctx = ctx_addr as *mut CONTEXT;
    core::ptr::write_bytes(ctx as *mut u8, 0, size_of::<CONTEXT>());
    (*ctx).ContextFlags = CONTEXT_FULL;
    if GetThreadContext(primary, ctx) == 0 {
        return Err(InjectError::CreateProcess);
    }
    let old_rsp = (*ctx).Rsp;
    if old_rsp == 0 || old_rsp >= old_base || old_rsp < old_limit {
        eprintln!(
            "vfs-inject: primary RSP={old_rsp:#x} outside stack [{old_limit:#x},{old_base:#x})"
        );
        return Err(InjectError::CreateProcess);
    }
    let used = (old_base - old_rsp) as usize;
    // Leave headroom: used frames + 256 KiB slack must fit in the new reserve.
    if used + 256 * 1024 > stack_reserve {
        eprintln!("vfs-inject: live stack used={used:#x} exceeds expand target");
        return Err(InjectError::Alloc);
    }

    let stack = VirtualAllocEx(
        process,
        core::ptr::null(),
        stack_reserve,
        MEM_COMMIT | MEM_RESERVE,
        PAGE_READWRITE,
    );
    if stack.is_null() {
        return Err(InjectError::Alloc);
    }
    let new_base = stack as u64 + stack_reserve as u64; // high address (grows down)
    // Guard-ish low page as StackLimit (committed but not used for frames).
    let new_limit = stack as u64 + 0x1000;
    let new_rsp = new_base - used as u64;

    // Copy live frames [old_rsp, old_base) → [new_rsp, new_base).
    let mut live = vec![0u8; used];
    let ok = windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory(
        process,
        old_rsp as *const c_void,
        live.as_mut_ptr() as *mut c_void,
        used,
        &mut read_n,
    );
    if ok == 0 || read_n != used {
        eprintln!("vfs-inject: read live stack frames failed used={used:#x}");
        return Err(InjectError::CreateProcess);
    }
    let mut written = 0usize;
    let ok = WriteProcessMemory(
        process,
        new_rsp as *mut c_void,
        live.as_ptr() as *const c_void,
        used,
        &mut written,
    );
    if ok == 0 || written != used {
        eprintln!("vfs-inject: write live stack frames failed");
        return Err(InjectError::Write);
    }

    for (off, val) in [
        (0x08u64, new_base),
        (0x10u64, new_limit),
        (0x1478u64, stack as u64),
    ] {
        let ok = WriteProcessMemory(
            process,
            (teb as u64 + off) as *mut c_void,
            &val as *const u64 as *const c_void,
            8,
            &mut written,
        );
        if ok == 0 || written != 8 {
            eprintln!("vfs-inject: TEB stack field write failed off={off:#x}");
            return Err(InjectError::Write);
        }
    }

    (*ctx).Rsp = new_rsp;
    if SetThreadContext(primary, ctx) == 0 {
        return Err(InjectError::CreateProcess);
    }
    eprintln!(
        "vfs-inject: expanded primary stack to {stack_reserve:#x} \
         (TEB={:#x} RSP {old_rsp:#x}→{new_rsp:#x} used={used:#x}; \
         old dealloc={old_dealloc:#x})",
        tbi.teb_base
    );
    Ok(())
}

/// Inject `dll_path` into a process via `LoadLibraryW` on a remote thread.
/// Used for the post-init full shim (`vfs-shim-dll`). Wakes the loader — do not
/// use this alone when the target's own static imports must be virtualized.
pub fn inject_dll(process: HANDLE, dll_path: &str) -> Result<(), InjectError> {
    // SAFETY: standard remote LoadLibrary injection; `process` is a live process
    // handle with the needed rights (from CreateProcessW).
    unsafe {
        let dll_w = wide(dll_path);
        let bytes = dll_w.len() * 2;
        let remote = VirtualAllocEx(
            process,
            core::ptr::null(),
            bytes,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        );
        if remote.is_null() {
            return Err(InjectError::Alloc);
        }
        let mut written = 0usize;
        let ok = WriteProcessMemory(
            process,
            remote,
            dll_w.as_ptr() as *const c_void,
            bytes,
            &mut written,
        );
        if ok == 0 || written != bytes {
            return Err(InjectError::Write);
        }
        let k32 = GetModuleHandleW(wide("kernel32.dll").as_ptr());
        if k32.is_null() {
            return Err(InjectError::RemoteThread);
        }
        let load_library = match GetProcAddress(k32, b"LoadLibraryW\0".as_ptr()) {
            Some(p) => p,
            None => return Err(InjectError::RemoteThread),
        };
        let start: LPTHREAD_START_ROUTINE = Some(core::mem::transmute(load_library));
        let hthread =
            CreateRemoteThread(process, core::ptr::null(), 0, start, remote, 0, core::ptr::null_mut());
        if hthread.is_null() || hthread == INVALID_HANDLE_VALUE {
            let err = windows_sys::Win32::Foundation::GetLastError();
            eprintln!("vfs-inject: CreateRemoteThread(LoadLibrary) failed last_error={err}");
            return Err(InjectError::RemoteThread);
        }
        WaitForSingleObject(hthread, INFINITE);
        let mut exit: u32 = 0;
        let got = windows_sys::Win32::System::Threading::GetExitCodeThread(hthread, &mut exit);
        CloseHandle(hthread);
        // LoadLibraryW returns HMODULE. On x64 the thread exit code is only the
        // low 32 bits — a successful load above 4GB can look like 0. Treat 0 as
        // soft-warn only; hollow/bootstrap timeouts catch true failures.
        if got == 0 || exit == 0 {
            eprintln!(
                "vfs-inject: remote LoadLibraryW exit={exit:#x} for {dll_path} \
                 (may still be OK on x64 if HMODULE high bits set)"
            );
        }
        Ok(())
    }
}

/// Reflectively map the zero-import payload into a suspended process, write
/// Config + PIC stub, and redirect the primary thread's RIP at the stub.
/// Does **not** resume the thread — caller does that after any extra setup.
///
/// Returns remote counters + Config addresses (for dual-layer handoff).
///
/// When `with_release_gate` is true, the preinit stub spins after
/// `shim_install` until the injector writes a non-zero u32 at `release_flag`
/// (used so LoadLibrary of the full shim can run before the primary continues
/// into `RtlUserThreadStart` / loader init).
pub fn arm_preinit_payload(
    process: HANDLE,
    thread: HANDLE,
    payload_dll_path: &str,
    redirects: &[PreinitRedirect],
) -> Result<PreinitArm, InjectError> {
    arm_preinit_payload_ex(process, thread, payload_dll_path, redirects, false)
}

/// Like [`arm_preinit_payload`], with optional post-install spin gate.
pub fn arm_preinit_payload_ex(
    process: HANDLE,
    thread: HANDLE,
    payload_dll_path: &str,
    redirects: &[PreinitRedirect],
    with_release_gate: bool,
) -> Result<PreinitArm, InjectError> {
    if redirects.len() > MAX_REDIRECTS {
        return Err(InjectError::Config);
    }

    let raw = std::fs::read(payload_dll_path).map_err(|_| InjectError::PayloadRead)?;
    let (mut img, image_base, e_lfanew) = build_image(&raw).map_err(|_| InjectError::PeParse)?;

    // SAFETY: process/thread from CreateProcessW(CREATE_SUSPENDED); all remote
    // allocations are in that process.
    unsafe {
        let remote_base = vae(process, img.len(), true)?;
        apply_relocs(&mut img, e_lfanew, image_base, remote_base);
        wpm(process, remote_base, &img)?;
        FlushInstructionCache(process, remote_base as *const c_void, img.len());

        let install_rva = export_rva(&img, e_lfanew, b"shim_install").map_err(|_| InjectError::PeParse)?;
        let remote_install = remote_base + install_rva as u64;

        let tramp_base = vae(process, 0x1000, true)?;
        let counters = vae(process, 0x1000, false)?;

        // Materialize UTF-16 strings for each redirect into remote memory.
        let mut entries = [RedirectEntry::default(); MAX_REDIRECTS];
        for (i, r) in redirects.iter().enumerate() {
            let suf_w: Vec<u16> = r.suffix.encode_utf16().collect();
            let bak_w = wide(&r.backing_nt);
            let bak_wlen = (bak_w.len() - 1) as u32; // exclude NUL
            let suf_bytes: Vec<u8> = suf_w.iter().flat_map(|c| c.to_le_bytes()).collect();
            let bak_bytes: Vec<u8> = bak_w.iter().flat_map(|c| c.to_le_bytes()).collect();
            let remote_suf = vae(process, suf_bytes.len().max(2), false)?;
            let remote_bak = vae(process, bak_bytes.len(), false)?;
            wpm(process, remote_suf, &suf_bytes)?;
            wpm(process, remote_bak, &bak_bytes)?;
            entries[i] = RedirectEntry {
                suffix_ptr: remote_suf as usize,
                suffix_wlen: suf_w.len() as u32,
                backing_ptr: remote_bak as usize,
                backing_wlen: bak_wlen,
                backing_size: r.backing_size,
            };
        }

        let cfg = PayloadConfig {
            nt_protect: ntdll_proc(b"NtProtectVirtualMemory\0")?,
            open_target: ntdll_proc(b"NtOpenFile\0")?,
            open_tramp: tramp_base as usize,
            qattr_target: ntdll_proc(b"NtQueryAttributesFile\0")?,
            qattr_tramp: (tramp_base + 0x40) as usize,
            qfull_target: ntdll_proc(b"NtQueryFullAttributesFile\0")?,
            qfull_tramp: (tramp_base + 0x80) as usize,
            create_target: ntdll_proc(b"NtCreateFile\0")?,
            create_tramp: (tramp_base + 0xC0) as usize,
            install_mask: 0xF,
            redirect_count: redirects.len() as u32,
            redirects: entries,
            counters: counters as usize,
            secondary_open: 0,
            secondary_create: 0,
            secondary_qattr: 0,
            secondary_qfull: 0,
        };
        let cfg_bytes = cfg.as_bytes();
        let remote_config = vae(process, cfg_bytes.len(), false)?;
        wpm(process, remote_config, cfg_bytes)?;

        // CONTEXT must be 16-byte aligned or Get/SetThreadContext fails (998).
        #[repr(C, align(16))]
        struct AlignedContext(CONTEXT);
        const CONTEXT_CONTROL_AMD64: u32 = 0x0010_0001;
        let mut actx = AlignedContext(zeroed());
        actx.0.ContextFlags = CONTEXT_CONTROL_AMD64;
        if GetThreadContext(thread, &mut actx.0) == 0 {
            return Err(InjectError::ThreadContext);
        }
        let orig_rip = actx.0.Rip;

        // Dual-layer spin gate: counters+0x20 (u32). 0 = hold after install.
        let release_flag = counters + 0x20;
        let stub = build_stub(
            remote_config,
            remote_install,
            orig_rip,
            counters,
            if with_release_gate { release_flag } else { 0 },
        );
        let remote_stub = vae(process, stub.len().max(16), true)?;
        wpm(process, remote_stub, &stub)?;
        FlushInstructionCache(process, remote_stub as *const c_void, stub.len());

        actx.0.ContextFlags = CONTEXT_CONTROL_AMD64;
        actx.0.Rip = remote_stub;
        if SetThreadContext(thread, &actx.0) == 0 {
            return Err(InjectError::ThreadContext);
        }

        Ok(PreinitArm {
            counters,
            cfg_remote: remote_config,
            release_flag: if with_release_gate { release_flag } else { 0 },
        })
    }
}

/// Result of arming the early payload on a suspended process.
pub struct PreinitArm {
    pub counters: u64,
    pub cfg_remote: u64,
    /// When non-zero, address of u32 the preinit stub spins on until set.
    pub release_flag: u64,
}

/// Launch the target with dual-layer injection:
/// 1. Pre-init early payload (RIP-redirect) installs hooks then **spins**  
/// 2. Injector LoadLibrary full shim (remote thread) — loader init with early
///    hooks live; full shim `install_late` publishes secondary  
/// 3. Injector releases the spin gate → primary continues to RtlUserThreadStart  
///
/// Requires `cfg.payload_path` pointing at `vfs_payload.dll`.
///
/// Early redirect table = static imports from the config file (if any) plus
/// `cfg.preinit_redirects` (explicit overrides / extras). Cap = payload
/// `MAX_REDIRECTS` (4).
pub fn run_target_with_shim(cfg: RunConfig) -> Result<i32, InjectError> {
    // Resolve payload and best-effort co-locate next to the full shim so child
    // CPIW inject can find vfs_payload.dll beside the loaded shim DLL.
    let payload_path = resolve_payload_for_run(&cfg.payload_path, &cfg.dll_path)
        .ok_or(InjectError::PayloadRead)?;

    std::env::set_var("VFS_SHIM_CONFIG", &cfg.config_path);
    std::env::set_var("VFS_SHIM_READY", &cfg.ready_path);
    // Advertise payload path for children that resolve via env.
    std::env::set_var("VFS_PAYLOAD_PATH", &payload_path);
    // `VFS_VIRTUAL_IMAGE` marked a hollowed image so the shim could spoof
    // `GetModuleFileName`. With the staged launch the process really *is* the
    // image it reports, so nothing sets it — cleared here in case an ancestor
    // left one behind.
    std::env::remove_var("VFS_VIRTUAL_IMAGE");
    let cfg_file = format!("{}.payload_cfg", cfg.ready_path);
    std::env::set_var("VFS_DUAL_LAYER", "1");
    std::env::set_var("VFS_PAYLOAD_CFG_FILE", &cfg_file);
    let _ = std::fs::remove_file(&cfg_file);
    // The managed root, so the child's fuse client matches the session root.
    if let Some(ref d) = cfg.current_dir {
        std::env::set_var("VFS_VIRTUAL_DIR", d);
    }
    let _ = std::fs::remove_file(&cfg.ready_path);

    let redirects = merge_preinit_redirects(&cfg.config_path, &cfg.preinit_redirects);

    // SAFETY: CreateProcessW + dual-layer arm + resume.
    unsafe {
        let mut pi: PROCESS_INFORMATION = zeroed();
        // One launch path: `CreateProcess` the staged image and dual-layer
        // inject. Staging puts the EXE and its import closure on disk, so the
        // loader resolves everything itself and there is nothing left for a
        // hollow to do — see `vfs-director::stage`.
        let mut cmdline = format!("\"{}\"", cfg.target_exe);
        for a in &cfg.args {
            cmdline.push_str(&format!(" \"{a}\""));
        }
        let app_w = wide(&cfg.target_exe);
        let mut cmd_w = wide(&cmdline);
        let cwd_w = cfg.current_dir.as_ref().map(|s| wide(s));
        let mut si: STARTUPINFOW = zeroed();
        si.cb = size_of::<STARTUPINFOW>() as u32;
        let ok = CreateProcessW(
            app_w.as_ptr(),
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
            return Err(InjectError::CreateProcess);
        }

        // The shim adds frames to every intercepted call, and the stock 1 MiB
        // primary stack overflows under that (`0xC00000FD`). This used to be
        // done on the hollow path only; it is not hollow-specific, so it moved
        // here when that path went away. Best-effort — a failure resumes with
        // the stock stack rather than refusing to launch.
        if let Err(e) = expand_primary_stack(pi.hProcess, pi.hThread, 16 * 1024 * 1024) {
            eprintln!(
                "vfs-inject: expand_primary_stack failed ({e:?}) — resuming with stock 1MiB stack"
            );
        }

        let arm = match arm_preinit_payload_ex(
            pi.hProcess,
            pi.hThread,
            &payload_path,
            &redirects,
            true, // spin gate
        ) {
            Ok(a) => a,
            Err(e) => {
                let _ = ResumeThread(pi.hThread);
                CloseHandle(pi.hThread);
                CloseHandle(pi.hProcess);
                return Err(e);
            }
        };

        // Publish cfg address for install_late (bootstrap reads this file).
        let _ = std::fs::write(&cfg_file, format!("{:x}", arm.cfg_remote));

        ResumeThread(pi.hThread);

        // Wait until early install sentinel is set (stub ran shim_install).
        let deadline = Instant::now() + cfg.ready_timeout;
        loop {
            let mut word = [0u8; 4];
            let mut n = 0usize;
            let ok = windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory(
                pi.hProcess,
                (arm.counters + 0x1C) as *const c_void,
                word.as_mut_ptr() as *mut c_void,
                4,
                &mut n,
            );
            if ok != 0 && n == 4 && u32::from_le_bytes(word) == 0xC0DE {
                break;
            }
            if Instant::now() >= deadline {
                CloseHandle(pi.hThread);
                CloseHandle(pi.hProcess);
                return Err(InjectError::Timeout);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        // Full shim LoadLibrary on a remote thread. Process init runs here with
        // early hooks already live. DllMain spawns bootstrap → install_late.
        if let Err(e) = inject_dll(pi.hProcess, &cfg.dll_path) {
            // Release spin so the process can die cleanly.
            let one = 1u32.to_le_bytes();
            let _ = wpm(pi.hProcess, arm.release_flag, &one);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            return Err(e);
        }

        // Wait for full shim ready marker.
        let deadline = Instant::now() + cfg.ready_timeout;
        while !std::path::Path::new(&cfg.ready_path).exists() {
            if Instant::now() >= deadline {
                let one = 1u32.to_le_bytes();
                let _ = wpm(pi.hProcess, arm.release_flag, &one);
                CloseHandle(pi.hThread);
                CloseHandle(pi.hProcess);
                return Err(InjectError::Timeout);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Release primary thread into RtlUserThreadStart / rest of init + main.
        let one = 1u32.to_le_bytes();
        if wpm(pi.hProcess, arm.release_flag, &one).is_err() {
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            return Err(InjectError::Write);
        }

        if cfg.detach {
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            return Ok(0);
        }

        if WaitForSingleObject(pi.hProcess, INFINITE) != 0 {
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            return Err(InjectError::Wait);
        }
        let mut code: u32 = 0;
        let got = GetExitCodeProcess(pi.hProcess, &mut code);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
        if got == 0 {
            return Err(InjectError::ExitCode);
        }
        Ok(code as i32)
    }
}

/// Launch the target suspended, arm the early payload via reflective-map +
/// RIP-redirect (no LoadLibrary), resume, and return the exit code.
///
/// Hooks are live before `LdrpInitializeProcess`, so the EXE's own static
/// imports can be virtualized via the payload redirect table.
pub fn run_target_with_preinit(cfg: PreinitConfig) -> Result<i32, InjectError> {
    let mut cmdline = format!("\"{}\"", cfg.target_exe);
    for a in &cfg.args {
        cmdline.push_str(&format!(" \"{a}\""));
    }
    let app_w = wide(&cfg.target_exe);
    let mut cmd_w = wide(&cmdline);
    let cwd_w = cfg
        .current_dir
        .as_ref()
        .map(|s| wide(s));

    // SAFETY: CreateProcessW + preinit arm + resume; handles closed on every path.
    unsafe {
        let mut si: STARTUPINFOW = zeroed();
        si.cb = size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = zeroed();
        let ok = CreateProcessW(
            app_w.as_ptr(),
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
            return Err(InjectError::CreateProcess);
        }

        if let Err(e) = arm_preinit_payload(
            pi.hProcess,
            pi.hThread,
            &cfg.payload_path,
            &cfg.redirects,
        ) {
            let _ = ResumeThread(pi.hThread);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            return Err(e);
        }

        ResumeThread(pi.hThread);
        if WaitForSingleObject(pi.hProcess, INFINITE) != 0 {
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            return Err(InjectError::Wait);
        }
        let mut code: u32 = 0;
        let got = GetExitCodeProcess(pi.hProcess, &mut code);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
        if got == 0 {
            return Err(InjectError::ExitCode);
        }
        Ok(code as i32)
    }
}

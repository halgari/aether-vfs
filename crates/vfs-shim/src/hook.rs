//! The NtCreateFile detour. ALL `unsafe` in the crate lives here.
#![allow(unsafe_code)]

use core::ffi::c_void;
use std::sync::OnceLock;

use retour::RawDetour;
use vfs_redirect::Decision;
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};

use crate::engine::Engine;
use crate::ntdef::{NtCreateFileFn, ObjectAttributes, UnicodeString, STATUS_UNSUCCESSFUL};

/// Errors installing the hook.
#[derive(Debug)]
pub enum InstallError {
    AlreadyInstalled,
    NtdllMissing,
    ProcMissing,
    Detour,
}

static ENGINE: OnceLock<Engine> = OnceLock::new();
// Set once, before the detour is enabled; only read from the hook thereafter.
static mut TRAMPOLINE: Option<NtCreateFileFn> = None;

/// Keeps the detour alive; dropping it disables the hook.
pub struct HookGuard {
    _detour: RawDetour,
}

/// Install the NtCreateFile detour backed by `engine`. Idempotent-guarded: a
/// second call returns `AlreadyInstalled`.
pub fn install(engine: Engine) -> Result<HookGuard, InstallError> {
    ENGINE.set(engine).map_err(|_| InstallError::AlreadyInstalled)?;

    // SAFETY: standard ntdll lookup + detour install. `hook` matches the
    // NtCreateFile ABI (`ntdef::NtCreateFileFn`). Trampoline is stored before
    // the detour is enabled, so the hook always observes `Some`.
    unsafe {
        let ntdll = GetModuleHandleA(b"ntdll.dll\0".as_ptr());
        if ntdll.is_null() {
            return Err(InstallError::NtdllMissing);
        }
        let proc = GetProcAddress(ntdll, b"NtCreateFile\0".as_ptr())
            .ok_or(InstallError::ProcMissing)?;
        let target = proc as *const ();
        let detour =
            RawDetour::new(target, hook as *const ()).map_err(|_| InstallError::Detour)?;
        TRAMPOLINE = Some(core::mem::transmute::<*const (), NtCreateFileFn>(
            detour.trampoline() as *const (),
        ));
        detour.enable().map_err(|_| InstallError::Detour)?;
        Ok(HookGuard { _detour: detour })
    }
}

/// The detour. Must never panic (a panic across `extern "system"` aborts) and
/// must do no hookable I/O.
unsafe extern "system" fn hook(
    file_handle: *mut HANDLE,
    access: u32,
    oa: *const ObjectAttributes,
    iosb: *mut c_void,
    alloc: *const i64,
    attrs: u32,
    share: u32,
    disp: u32,
    opts: u32,
    ea: *const c_void,
    ealen: u32,
) -> NTSTATUS {
    // Invariant: TRAMPOLINE is Some once the detour is enabled.
    let tramp = match TRAMPOLINE {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };

    if let Some(engine) = ENGINE.get() {
        if !oa.is_null() {
            let oa_ref = &*oa;
            // MVP: only fully-qualified opens (no RootDirectory-relative).
            if oa_ref.root_directory.is_null() && !oa_ref.object_name.is_null() {
                let us = &*oa_ref.object_name;
                if !us.buffer.is_null() {
                    let units = core::slice::from_raw_parts(us.buffer, us.length as usize / 2);
                    let path = String::from_utf16_lossy(units);
                    if let Decision::Redirect { target_nt } = engine.decide(&path) {
                        // Buffers live across the synchronous trampoline call.
                        let mut wbuf: Vec<u16> = target_nt.encode_utf16().collect();
                        let byte_len = (wbuf.len() * 2) as u16;
                        let new_us = UnicodeString {
                            length: byte_len,
                            maximum_length: byte_len,
                            buffer: wbuf.as_mut_ptr(),
                        };
                        let new_oa = ObjectAttributes {
                            length: oa_ref.length,
                            root_directory: core::ptr::null_mut(),
                            object_name: &new_us,
                            attributes: oa_ref.attributes,
                            security_descriptor: oa_ref.security_descriptor,
                            security_qos: oa_ref.security_qos,
                        };
                        let status = tramp(
                            file_handle, access, &new_oa, iosb, alloc, attrs, share, disp, opts,
                            ea, ealen,
                        );
                        drop(wbuf);
                        return status;
                    }
                }
            }
        }
    }

    tramp(file_handle, access, oa, iosb, alloc, attrs, share, disp, opts, ea, ealen)
}

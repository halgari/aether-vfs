//! The ntdll detours. ALL `unsafe` in the crate lives here.
#![allow(unsafe_code)]

use core::ffi::c_void;
use std::sync::OnceLock;

use retour::RawDetour;
use vfs_redirect::{AttrDecision, Decision};
use windows_sys::Win32::Foundation::{HANDLE, HMODULE, NTSTATUS};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};

use crate::engine::Engine;
use crate::ntdef::{
    FileBasicInformation, FileNetworkOpenInformation, NtCreateFileFn, NtQueryAttributesFileFn,
    NtQueryFullAttributesFileFn, ObjectAttributes, UnicodeString, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, STATUS_OBJECT_NAME_NOT_FOUND, STATUS_SUCCESS, STATUS_UNSUCCESSFUL,
};

/// Errors installing the hooks.
#[derive(Debug)]
pub enum InstallError {
    AlreadyInstalled,
    NtdllMissing,
    ProcMissing,
    Detour,
}

static ENGINE: OnceLock<Engine> = OnceLock::new();
// Each set once, before any detour is enabled; only read from the hooks after.
static mut TRAMP_CREATE: Option<NtCreateFileFn> = None;
static mut TRAMP_QATTR: Option<NtQueryAttributesFileFn> = None;
static mut TRAMP_QFULL: Option<NtQueryFullAttributesFileFn> = None;

/// Keeps the detours alive; dropping it disables the hooks.
pub struct HookGuard {
    _detours: Vec<RawDetour>,
}

/// Resolve `name` in ntdll and build (not yet enabled) a detour to `hookfn`.
unsafe fn make_detour(
    ntdll: HMODULE,
    name: &[u8],
    hookfn: *const (),
) -> Result<RawDetour, InstallError> {
    let proc = GetProcAddress(ntdll, name.as_ptr()).ok_or(InstallError::ProcMissing)?;
    RawDetour::new(proc as *const (), hookfn).map_err(|_| InstallError::Detour)
}

/// Install all read-path detours backed by `engine`. Idempotent-guarded.
pub fn install(engine: Engine) -> Result<HookGuard, InstallError> {
    ENGINE.set(engine).map_err(|_| InstallError::AlreadyInstalled)?;

    // SAFETY: standard ntdll lookup + detour install; each hook matches its
    // function's ABI; each trampoline is stored before any detour is enabled.
    unsafe {
        let ntdll = GetModuleHandleA(b"ntdll.dll\0".as_ptr());
        if ntdll.is_null() {
            return Err(InstallError::NtdllMissing);
        }

        let d_create = make_detour(ntdll, b"NtCreateFile\0", create_hook as *const ())?;
        TRAMP_CREATE = Some(core::mem::transmute::<*const (), NtCreateFileFn>(
            d_create.trampoline() as *const (),
        ));
        let d_qattr = make_detour(ntdll, b"NtQueryAttributesFile\0", qattr_hook as *const ())?;
        TRAMP_QATTR = Some(core::mem::transmute::<*const (), NtQueryAttributesFileFn>(
            d_qattr.trampoline() as *const (),
        ));
        let d_qfull =
            make_detour(ntdll, b"NtQueryFullAttributesFile\0", qfull_hook as *const ())?;
        TRAMP_QFULL = Some(core::mem::transmute::<*const (), NtQueryFullAttributesFileFn>(
            d_qfull.trampoline() as *const (),
        ));

        d_create.enable().map_err(|_| InstallError::Detour)?;
        d_qattr.enable().map_err(|_| InstallError::Detour)?;
        d_qfull.enable().map_err(|_| InstallError::Detour)?;

        Ok(HookGuard { _detours: vec![d_create, d_qattr, d_qfull] })
    }
}

/// Decode a fully-qualified ObjectName. `None` when ineligible (null/relative OA
/// or empty name).
unsafe fn path_of(oa: *const ObjectAttributes) -> Option<String> {
    if oa.is_null() {
        return None;
    }
    let oa_ref = &*oa;
    if !oa_ref.root_directory.is_null() || oa_ref.object_name.is_null() {
        return None;
    }
    let us = &*oa_ref.object_name;
    if us.buffer.is_null() {
        return None;
    }
    let units = core::slice::from_raw_parts(us.buffer, us.length as usize / 2);
    Some(String::from_utf16_lossy(units))
}

/// Decode + ask the engine what to do with an open.
unsafe fn decision_for(oa: *const ObjectAttributes) -> Option<Decision> {
    let engine = ENGINE.get()?;
    let path = path_of(oa)?;
    Some(engine.decide(&path))
}

unsafe extern "system" fn create_hook(
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
    let tramp = match TRAMP_CREATE {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    match decision_for(oa) {
        Some(Decision::Redirect { target_nt }) => {
            let mut wbuf: Vec<u16> = target_nt.encode_utf16().collect();
            let byte_len = (wbuf.len() * 2) as u16;
            let new_us = UnicodeString {
                length: byte_len,
                maximum_length: byte_len,
                buffer: wbuf.as_mut_ptr(),
            };
            let oa_ref = &*oa;
            let new_oa = ObjectAttributes {
                length: oa_ref.length,
                root_directory: core::ptr::null_mut(),
                object_name: &new_us,
                attributes: oa_ref.attributes,
                security_descriptor: oa_ref.security_descriptor,
                security_qos: oa_ref.security_qos,
            };
            let status = tramp(
                file_handle, access, &new_oa, iosb, alloc, attrs, share, disp, opts, ea, ealen,
            );
            drop(wbuf);
            status
        }
        Some(Decision::Deny) => STATUS_OBJECT_NAME_NOT_FOUND,
        Some(Decision::PassThrough) | None => {
            tramp(file_handle, access, oa, iosb, alloc, attrs, share, disp, opts, ea, ealen)
        }
    }
}

unsafe extern "system" fn qattr_hook(
    oa: *const ObjectAttributes,
    info: *mut FileBasicInformation,
) -> NTSTATUS {
    let tramp = match TRAMP_QATTR {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if let Some(engine) = ENGINE.get() {
        if let Some(path) = path_of(oa) {
            match engine.query_attributes(&path) {
                AttrDecision::Attributes { is_dir, .. } => {
                    if !info.is_null() {
                        (*info).creation_time = 0;
                        (*info).last_access_time = 0;
                        (*info).last_write_time = 0;
                        (*info).change_time = 0;
                        (*info).file_attributes =
                            if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
                    }
                    return STATUS_SUCCESS;
                }
                AttrDecision::Deny => return STATUS_OBJECT_NAME_NOT_FOUND,
                AttrDecision::PassThrough => {}
            }
        }
    }
    tramp(oa, info)
}

unsafe extern "system" fn qfull_hook(
    oa: *const ObjectAttributes,
    info: *mut FileNetworkOpenInformation,
) -> NTSTATUS {
    let tramp = match TRAMP_QFULL {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if let Some(engine) = ENGINE.get() {
        if let Some(path) = path_of(oa) {
            match engine.query_attributes(&path) {
                AttrDecision::Attributes { is_dir, size, .. } => {
                    if !info.is_null() {
                        (*info).creation_time = 0;
                        (*info).last_access_time = 0;
                        (*info).last_write_time = 0;
                        (*info).change_time = 0;
                        (*info).allocation_size = size as i64;
                        (*info).end_of_file = size as i64;
                        (*info).file_attributes =
                            if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
                    }
                    return STATUS_SUCCESS;
                }
                AttrDecision::Deny => return STATUS_OBJECT_NAME_NOT_FOUND,
                AttrDecision::PassThrough => {}
            }
        }
    }
    tramp(oa, info)
}

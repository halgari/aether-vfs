//! Telling a suspended target where it is: rewriting its command line and
//! current directory before any of its code runs.
//!
//! A target whose root location is purely virtual cannot be *started* there —
//! `CreateProcess` needs a real image and a real working directory — so it is
//! started from its root's backing directory and then, still suspended, has
//! its `RTL_USER_PROCESS_PARAMETERS` rewritten: `CommandLine` names the image
//! by its virtual path, and `CurrentDirectory.DosPath` is the virtual
//! directory. Both happen before kernelbase initialises in the target, so
//! `GetCommandLineW` and `GetCurrentDirectoryW` report them, and relative
//! paths resolve against the virtual directory. The current-directory
//! *handle* stays the real backing directory's, which the shim recognises as
//! its root's second spelling.
//!
//! `ImagePathName` is left alone: the loader takes the application directory
//! from it while it maps the static imports, which happens before any hook
//! exists and so must find real files. The shim renames it once the loader
//! is done (see the shim's bootstrap).
//!
//! The target has the injector's own bitness (the 32-bit injector starts
//! 32-bit programs), so its structures are laid out at this build's pointer
//! width.

use core::ffi::c_void;
use std::mem::size_of;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Memory::{VirtualQueryEx, MEMORY_BASIC_INFORMATION};

/// `PEB.ProcessParameters`: after four bytes and three pointers.
const PEB_PROCESS_PARAMETERS: usize = 4 * size_of::<usize>();
/// `RTL_USER_PROCESS_PARAMETERS` fields.
const PP_FLAGS: usize = 0x08;
#[cfg(target_arch = "x86_64")]
const PP_CURDIR_DOS_PATH: usize = 0x38;
#[cfg(target_arch = "x86")]
const PP_CURDIR_DOS_PATH: usize = 0x24;
#[cfg(target_arch = "x86_64")]
const PP_COMMAND_LINE: usize = 0x70;
#[cfg(target_arch = "x86")]
const PP_COMMAND_LINE: usize = 0x40;
/// `RTL_USER_PROC_PARAMS_NORMALIZED`: the string buffers hold addresses, not
/// offsets from the block.
const PARAMS_NORMALIZED: u32 = 1;
/// The current directory's buffer is `MAX_PATH` characters: the runtime copies
/// a new directory into it in place (`RtlSetCurrentDirectory_U`), checking
/// against `MaximumLength`.
const CURDIR_BUFFER_BYTES: usize = 260 * 2;

/// `UNICODE_STRING`.
#[repr(C)]
#[derive(Default)]
struct RemoteUnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: usize,
}

/// `PROCESS_BASIC_INFORMATION`.
#[repr(C)]
#[derive(Default)]
struct BasicInformation {
    exit_status: i32,
    peb_base_address: usize,
    affinity_mask: usize,
    base_priority: i32,
    unique_process_id: usize,
    inherited_from_unique_process_id: usize,
}

type NtQueryInformationProcessFn =
    unsafe extern "system" fn(HANDLE, u32, *mut c_void, u32, *mut u32) -> i32;

/// Rewrite the suspended `process`'s command line to `command_line` and its
/// current directory to `cwd` (a directory; a trailing `\` is added), each
/// only when given.
///
/// # Safety
/// `process` must be a suspended process this process created, with
/// `PROCESS_VM_READ | PROCESS_VM_WRITE | PROCESS_VM_OPERATION |
/// PROCESS_QUERY_INFORMATION` (a `CreateProcessW` handle has all of them).
#[allow(unsafe_code)]
pub unsafe fn rewrite(
    process: HANDLE,
    command_line: Option<&str>,
    cwd: Option<&str>,
) -> Result<(), String> {
    if command_line.is_none() && cwd.is_none() {
        return Ok(());
    }
    let params = process_parameters(process)?;
    let flags: u32 = read(process, params + PP_FLAGS)?;
    if flags & PARAMS_NORMALIZED == 0 {
        return Err("the target's process parameters are not normalized".into());
    }
    if let Some(cmd) = command_line {
        let units: Vec<u16> = cmd.encode_utf16().collect();
        put_string(
            process,
            params,
            params + PP_COMMAND_LINE,
            &units,
            units.len() * 2 + 2,
        )?;
    }
    if let Some(dir) = cwd {
        let mut dir = dir.trim_end_matches(['\\', '/']).to_string();
        dir.push('\\');
        let units: Vec<u16> = dir.encode_utf16().collect();
        if units.len() * 2 + 2 > CURDIR_BUFFER_BYTES {
            return Err(format!(
                "the virtual working directory {dir:?} is longer than MAX_PATH"
            ));
        }
        put_string(
            process,
            params,
            params + PP_CURDIR_DOS_PATH,
            &units,
            CURDIR_BUFFER_BYTES,
        )?;
    }
    Ok(())
}

/// The base address of `process`'s PEB.
///
/// # Safety
/// `process` is a live process handle with `PROCESS_QUERY_INFORMATION`.
#[allow(unsafe_code)]
pub(crate) unsafe fn peb_base(process: HANDLE) -> Result<u64, String> {
    let ntdll = GetModuleHandleW(wide("ntdll.dll").as_ptr());
    let query = GetProcAddress(ntdll, c"NtQueryInformationProcess".as_ptr().cast())
        .ok_or("ntdll has no NtQueryInformationProcess")?;
    let query: NtQueryInformationProcessFn = core::mem::transmute(query);
    let mut info = BasicInformation::default();
    let st = query(
        process,
        0, // ProcessBasicInformation
        (&mut info as *mut BasicInformation).cast(),
        size_of::<BasicInformation>() as u32,
        core::ptr::null_mut(),
    );
    if st < 0 || info.peb_base_address == 0 {
        return Err(format!("NtQueryInformationProcess: {st:#x}"));
    }
    Ok(info.peb_base_address as u64)
}

/// The address of `process`'s `RTL_USER_PROCESS_PARAMETERS`.
#[allow(unsafe_code)]
unsafe fn process_parameters(process: HANDLE) -> Result<usize, String> {
    let peb = peb_base(process)? as usize;
    let params: usize = read(process, peb + PEB_PROCESS_PARAMETERS)?;
    if params == 0 {
        return Err("the target has no process parameters yet".into());
    }
    Ok(params)
}

/// Make the `UNICODE_STRING` at `at` (a field of the parameter block at
/// `params`) say `units`, with room for at least `capacity` bytes.
///
/// The new text stays **inside the parameter block**: process start refuses
/// a block whose strings point elsewhere (a command line in its own
/// allocation fails kernelbase's initialisation with
/// `STATUS_DLL_INIT_FAILED`; a working directory there faults). So it goes in
/// the string's own buffer when that is big enough, and otherwise after the
/// block's last byte, in the slack of the block's own allocation, with the
/// block's `Length` grown to cover it.
#[allow(unsafe_code)]
unsafe fn put_string(
    process: HANDLE,
    params: usize,
    at: usize,
    units: &[u16],
    capacity: usize,
) -> Result<(), String> {
    let need = units.len() * 2 + 2;
    let capacity = capacity.max(need);
    let mut text = units.to_vec();
    text.push(0);
    let old: RemoteUnicodeString = read(process, at)?;
    if old.buffer != 0 && old.maximum_length as usize >= capacity {
        write_bytes(process, old.buffer, text.as_ptr().cast(), need)?;
        let len = (units.len() * 2) as u16;
        return write_bytes(process, at, (&len as *const u16).cast(), 2);
    }
    if capacity > u16::MAX as usize {
        return Err("a process parameter longer than a UNICODE_STRING holds".into());
    }
    // The block: MaximumLength (u32 @ 0), Length (u32 @ 4).
    let max_len: u32 = read(process, params)?;
    let used: u32 = read(process, params + 4)?;
    let place = (params + used as usize + 7) & !7;
    let end = place + capacity;
    let mut mbi: MEMORY_BASIC_INFORMATION = core::mem::zeroed();
    let got = VirtualQueryEx(
        process,
        params as *const c_void,
        &mut mbi,
        size_of::<MEMORY_BASIC_INFORMATION>(),
    );
    let region_end = mbi.BaseAddress as usize + mbi.RegionSize;
    if got == 0 || end > region_end {
        return Err(format!(
            "no room in the target's parameter block for a {capacity}-byte string"
        ));
    }
    write_bytes(process, place, text.as_ptr().cast(), need)?;
    let s = RemoteUnicodeString {
        length: (units.len() * 2) as u16,
        maximum_length: capacity as u16,
        buffer: place,
    };
    // Only the string itself: a `CURDIR`'s handle follows it.
    write_bytes(
        process,
        at,
        (&s as *const RemoteUnicodeString).cast(),
        size_of::<RemoteUnicodeString>(),
    )?;
    let used = (end - params) as u32;
    write_bytes(process, params + 4, (&used as *const u32).cast(), 4)?;
    let max_len = max_len.max(used);
    write_bytes(process, params, (&max_len as *const u32).cast(), 4)
}

#[allow(unsafe_code)]
unsafe fn read<T: Default>(process: HANDLE, at: usize) -> Result<T, String> {
    let mut v = T::default();
    let mut got = 0usize;
    let ok = ReadProcessMemory(
        process,
        at as *const c_void,
        (&mut v as *mut T).cast(),
        size_of::<T>(),
        &mut got,
    );
    if ok == 0 || got != size_of::<T>() {
        return Err(format!("ReadProcessMemory at {at:#x}"));
    }
    Ok(v)
}

#[allow(unsafe_code)]
unsafe fn write_bytes(
    process: HANDLE,
    at: usize,
    src: *const c_void,
    len: usize,
) -> Result<(), String> {
    let mut put = 0usize;
    let ok = WriteProcessMemory(process, at as *const c_void, src, len, &mut put);
    if ok == 0 || put != len {
        return Err(format!("WriteProcessMemory at {at:#x}"));
    }
    Ok(())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

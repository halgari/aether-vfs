//! An exception log: one record per fatal-class exception raised anywhere in the
//! process, for the failures a debugger cannot see.
//!
//! A thread hidden from debuggers (`ThreadHideFromDebugger`, which DRM wrappers
//! such as SteamStub set on the main thread) raises exceptions the debugger is
//! never told about, and when one goes unhandled the process ends with the
//! exception's code and no trace: Skyrim SE closed under the shim ended with
//! `0xC0000005` while a debugger attached to it saw no exception and no call to
//! any exit routine. A vectored handler runs for every thread, hidden or not.
//!
//! Off unless [`vfs_env::SHIM_EXCEPTION_LOG`] names a file. The file is opened
//! at install, before the detours go live; a record is formatted on the stack
//! (the faulting thread may hold the heap lock) and written with one
//! `WriteFile`. The handler only observes: it always continues the search. It
//! is registered *last* in the vectored chain, so faults the shim's own
//! handler services (lazy sections) never reach it.
#![allow(unsafe_code)]

use core::ffi::c_void;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::cell::Cell;

use windows_sys::Win32::Foundation::{HMODULE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, WriteFile, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_ALWAYS,
};
use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, CONTEXT, EXCEPTION_POINTERS,
};
#[cfg(target_arch = "x86_64")]
use windows_sys::Win32::System::Diagnostics::Debug::{
    RtlLookupFunctionEntry, RtlVirtualUnwind, UNW_FLAG_NHANDLER,
};
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;

const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
/// A C++ `throw`: frequent, and handled by the program's own `catch`.
const CXX_EXCEPTION: u32 = 0xE06D_7363;
/// Records per process, so a fault loop cannot fill the disk.
const MAX_RECORDS: u32 = 64;
const MAX_FRAMES: usize = 32;

static FILE: AtomicIsize = AtomicIsize::new(0);
static RECORDS: AtomicU32 = AtomicU32::new(0);

thread_local! {
    /// Set while this thread is inside the handler: a fault while recording
    /// (an unwind through a corrupt stack) is not recorded again.
    static IN_HANDLER: Cell<bool> = const { Cell::new(false) };
}

/// Opens the log and registers the handler, when the variable is set. Before
/// the detours: the open is real I/O.
pub fn install() {
    let Some(path) = vfs_env::text(vfs_env::SHIM_EXCEPTION_LOG) else {
        return;
    };
    let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    // SAFETY: a NUL-terminated path; the handle is kept for the process's life.
    let h = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_APPEND_DATA,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            core::ptr::null(),
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            core::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        return;
    }
    FILE.store(h as isize, Ordering::Release);
    // SAFETY: `handler` matches PVECTORED_EXCEPTION_HANDLER; 0 = call it last.
    unsafe { AddVectoredExceptionHandler(0, Some(handler)) };
}

/// A fixed-size line buffer: formatting allocates nothing.
struct Buf {
    bytes: [u8; 4096],
    len: usize,
}

impl core::fmt::Write for Buf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let n = s.len().min(self.bytes.len() - self.len);
        self.bytes[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

/// `module.dll+0x1234` for an address, or the bare address.
unsafe fn describe(out: &mut Buf, addr: u64) {
    let mut module: HMODULE = core::ptr::null_mut();
    let found = GetModuleHandleExW(
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        addr as usize as *const u16,
        &mut module,
    ) != 0;
    if !found {
        let _ = write!(out, "0x{addr:016x}");
        return;
    }
    let mut name = [0u16; 260];
    let n = GetModuleFileNameW(module, name.as_mut_ptr(), name.len() as u32) as usize;
    let file = &name[..n];
    let start = file
        .iter()
        .rposition(|&c| c == u16::from(b'\\') || c == u16::from(b'/'))
        .map_or(0, |i| i + 1);
    for &c in &file[start..] {
        let _ = out.write_char(char::from_u32(u32::from(c)).unwrap_or('?'));
    }
    let _ = write!(out, "+0x{:x}", addr - module as u64);
}

unsafe extern "system" fn handler(info: *mut EXCEPTION_POINTERS) -> i32 {
    crate::hook::contain_panic(
        "ExceptionLog",
        || {
            if IN_HANDLER.with(|c| c.replace(true)) {
                return EXCEPTION_CONTINUE_SEARCH;
            }
            // SAFETY: the pointers the system passed to a vectored handler.
            unsafe { record(info) };
            IN_HANDLER.with(|c| c.set(false));
            EXCEPTION_CONTINUE_SEARCH
        },
        || EXCEPTION_CONTINUE_SEARCH,
    )
}

unsafe fn record(info: *mut EXCEPTION_POINTERS) {
    if info.is_null() || (*info).ExceptionRecord.is_null() || (*info).ContextRecord.is_null() {
        return;
    }
    let rec = &*(*info).ExceptionRecord;
    let code = rec.ExceptionCode as u32;
    // Errors only (severity 3), and not C++ throws.
    if code & 0xC000_0000 != 0xC000_0000 || code == CXX_EXCEPTION {
        return;
    }
    if RECORDS.fetch_add(1, Ordering::Relaxed) >= MAX_RECORDS {
        return;
    }
    let h = FILE.load(Ordering::Acquire);
    if h == 0 {
        return;
    }
    let mut out = Buf {
        bytes: [0; 4096],
        len: 0,
    };
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let _ = write!(
        out,
        "{ms} pid={} tid={} code=0x{code:08x} flags=0x{:x} at ",
        std::process::id(),
        GetCurrentThreadId(),
        rec.ExceptionFlags
    );
    describe(&mut out, rec.ExceptionAddress as u64);
    if code == 0xC000_0005 && rec.NumberParameters >= 2 {
        let what = match rec.ExceptionInformation[0] {
            0 => "read",
            1 => "write",
            8 => "execute",
            _ => "access",
        };
        let _ = write!(out, " ({what} 0x{:x})", rec.ExceptionInformation[1]);
    }
    let _ = out.write_str("\n");
    write_stack(&mut out, *(*info).ContextRecord);
    let mut written = 0u32;
    // The shim's own write: never through its hooks.
    let _io = crate::hook::ShimIoGuard::enter();
    WriteFile(
        h as *mut c_void,
        out.bytes.as_ptr(),
        out.len as u32,
        &mut written,
        core::ptr::null_mut(),
    );
}

/// A 32-bit process: x86 has no unwind tables to walk, and guessing frames from
/// EBP inside an exception handler could fault again, so the faulting
/// instruction alone.
#[cfg(target_arch = "x86")]
unsafe fn write_stack(out: &mut Buf, ctx: CONTEXT) {
    let _ = out.write_str("    ");
    describe(out, u64::from(ctx.Eip));
    let _ = out.write_str("\n    (the stack isn't unwound in a 32-bit process)\n");
}

/// The faulting stack, unwound from the exception's context with the x64 unwind tables.
#[cfg(target_arch = "x86_64")]
unsafe fn write_stack(out: &mut Buf, mut ctx: CONTEXT) {
    for _ in 0..MAX_FRAMES {
        let pc = ctx.Rip;
        if pc == 0 {
            break;
        }
        let _ = out.write_str("    ");
        describe(out, pc);
        let _ = out.write_str("\n");
        let mut image_base = 0u64;
        let entry = RtlLookupFunctionEntry(pc, &mut image_base, core::ptr::null_mut());
        if entry.is_null() {
            // A leaf: the return address is on top of the stack.
            if ctx.Rsp == 0 || !ctx.Rsp.is_multiple_of(8) {
                break;
            }
            ctx.Rip = *(ctx.Rsp as *const u64);
            ctx.Rsp += 8;
        } else {
            let mut data: *mut c_void = core::ptr::null_mut();
            let mut frame = 0u64;
            RtlVirtualUnwind(
                UNW_FLAG_NHANDLER,
                image_base,
                pc,
                entry,
                &mut ctx,
                &mut data,
                &mut frame,
                core::ptr::null_mut(),
            );
        }
    }
}

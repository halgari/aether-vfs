//! Child-process propagation: inject the shim into force-suspended children,
//! readiness events, self-DLL path discovery.
#![allow(unsafe_code)]

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcessId, IsWow64Process2, SetEvent, WaitForMultipleObjects,
    WaitForSingleObject,
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The readiness event name for a given process id.
fn ready_event_name(pid: u32) -> Vec<u16> {
    wide(&format!(r"Local\vfs_shim_ready_{pid}"))
}

/// The event a child's shim sets when its bootstrap failed.
fn failed_event_name(pid: u32) -> Vec<u16> {
    wide(&format!(r"Local\vfs_shim_failed_{pid}"))
}

/// Why injecting into a child failed. Every one of these ends with the child
/// killed and its `CreateProcess` call failing (see `hook::process`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildInjectError {
    /// The child is a 32-bit (WOW64) image; the shim and payload are 64-bit.
    Child32Bit,
    /// This shim's own DLL path is unknown, so there is nothing to inject.
    NoShimDll,
    /// The child exited before it reported ready.
    ChildExited,
    /// The remote `LoadLibrary` of the shim could not be started or did not finish.
    InjectDll,
    /// The child's shim bootstrap failed and said so.
    BootstrapFailed,
    /// The child's shim never reported ready within the ready timeout.
    ReadyTimeout,
}

impl ChildInjectError {
    /// A short stable spelling, the key of the refused-child counter.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Child32Bit => "child-32bit",
            Self::NoShimDll => "no-shim-dll",
            Self::ChildExited => "child-exited",
            Self::InjectDll => "inject-dll",
            Self::BootstrapFailed => "bootstrap-failed",
            Self::ReadyTimeout => "ready-timeout",
        }
    }
}

/// How long a spawning process waits for a child's shim: the same ready timeout
/// the top-level launch used. `run_target_with_shim` publishes it in
/// `VFS_READY_TIMEOUT_SECS`, which the child inherits; without it, the shared
/// default.
pub(crate) fn child_ready_timeout_ms() -> u32 {
    u32::try_from(vfs_env::ready_timeout_secs().saturating_mul(1000)).unwrap_or(u32::MAX - 1)
}

/// Inject into a force-suspended child, the way the launcher does: give its
/// primary thread the shim's stack, then `LoadLibrary` the shim on a remote
/// thread. That thread runs the child's process initialisation and then the
/// shim's `DllMain`, which bootstraps before it returns. The child's primary
/// thread is never resumed here, so a caller that asked for a suspended child
/// gets it still suspended, now virtualised.
///
/// **Fails closed.** On any `Err` the caller must kill the child: there is no
/// fallback that runs it without the shim.
pub(crate) fn inject_child(
    process: HANDLE,
    thread: HANDLE,
    pid: u32,
    full_shim_dll: Option<&str>,
    timeout_ms: u32,
) -> Result<(), ChildInjectError> {
    let full_shim_dll = full_shim_dll.ok_or(ChildInjectError::NoShimDll)?;
    if is_32bit(process) {
        return Err(ChildInjectError::Child32Bit);
    }
    // SAFETY: `process`/`thread` come from our own `CreateProcessInternalW`
    // hook, which forced CREATE_SUSPENDED, so the child has not run and we hold
    // the rights these calls need.
    unsafe {
        // Best-effort, as for the launch: a failure leaves the stock stack.
        let _ = vfs_inject::expand_primary_stack(process, thread, vfs_inject::PRIMARY_STACK_BYTES);
        if vfs_inject::inject_dll(process, full_shim_dll, timeout_ms).is_err() {
            return Err(if has_exited(process) {
                ChildInjectError::ChildExited
            } else {
                ChildInjectError::InjectDll
            });
        }
    }
    // The shim signalled one way or the other before its `LoadLibrary` returned.
    match wait_ready(process, pid, 0) {
        ReadyWait::Ready => Ok(()),
        ReadyWait::BootstrapFailed => Err(ChildInjectError::BootstrapFailed),
        ReadyWait::Exited => Err(ChildInjectError::ChildExited),
        ReadyWait::TimedOut => Err(ChildInjectError::ReadyTimeout),
    }
}

/// Whether `process` is a 32-bit image running under WOW64. A query that
/// fails reads as "no": the arm step then decides.
fn is_32bit(process: HANDLE) -> bool {
    let (mut machine, mut native) = (0u16, 0u16);
    // SAFETY: a query of a process handle we own, into two locals.
    let ok = unsafe { IsWow64Process2(process, &mut machine, &mut native) };
    // `IMAGE_FILE_MACHINE_UNKNOWN` (0): not a WOW64 process.
    ok != 0 && machine != 0
}

fn has_exited(process: HANDLE) -> bool {
    // SAFETY: a zero-timeout poll of a process handle we own.
    unsafe { WaitForSingleObject(process, 0) == 0 }
}

/// The absolute path of the DLL this code lives in.
pub(crate) fn self_dll_path() -> Option<String> {
    // SAFETY: resolve our module by an address inside it, then read its path.
    unsafe {
        let mut hmod = core::ptr::null_mut();
        let ok = GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            self_dll_path as *const u16,
            &mut hmod,
        );
        if ok == 0 || hmod.is_null() {
            return None;
        }
        let mut buf = vec![0u16; 32768];
        let n = GetModuleFileNameW(hmod, buf.as_mut_ptr(), buf.len() as u32);
        if n == 0 || n as usize >= buf.len() {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..n as usize]))
    }
}

/// Signal that the current process's shim has installed its hooks.
pub(crate) fn signal_ready() {
    // SAFETY: named-event create + set; the leaked handle is process-lifetime.
    unsafe {
        let name = ready_event_name(GetCurrentProcessId());
        let ev = CreateEventW(core::ptr::null(), 1, 0, name.as_ptr());
        if !ev.is_null() {
            SetEvent(ev);
        }
    }
}

/// Called once this process's shim has finished writing its own ready state:
/// drops `VFS_SHIM_READY`, the ready file the top-level launch used to talk to
/// *this* process, so descendants do not inherit it. A child that did would
/// write its own "ready" or failure into the top-level ready file and its boot
/// log. Children signal their spawner by named event, and refusals are logged
/// through `VFS_CHILD_REFUSED_LOG`, which is left set.
pub fn finish_ready_handshake() {
    std::env::remove_var(vfs_env::SHIM_READY);
}

/// Signal that the current process's shim could not bootstrap, so a spawning
/// parent stops waiting for a ready signal that will not come and kills us.
/// The counterpart of [`signal_ready`].
pub fn signal_bootstrap_failed() {
    // SAFETY: named-event create + set; the leaked handle is process-lifetime.
    unsafe {
        let name = failed_event_name(GetCurrentProcessId());
        let ev = CreateEventW(core::ptr::null(), 1, 0, name.as_ptr());
        if !ev.is_null() {
            SetEvent(ev);
        }
    }
}

/// How a wait for a child's shim ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadyWait {
    Ready,
    BootstrapFailed,
    Exited,
    TimedOut,
}

/// Wait up to `timeout_ms` for `pid`'s shim to signal readiness or failure,
/// or for the child to exit, whichever comes first.
pub(crate) fn wait_ready(process: HANDLE, pid: u32, timeout_ms: u32) -> ReadyWait {
    // SAFETY: named-event create + timed wait; handles closed before return.
    unsafe {
        let ready = CreateEventW(core::ptr::null(), 1, 0, ready_event_name(pid).as_ptr());
        let failed = CreateEventW(core::ptr::null(), 1, 0, failed_event_name(pid).as_ptr());
        if ready.is_null() || failed.is_null() {
            for h in [ready, failed] {
                if !h.is_null() {
                    CloseHandle(h);
                }
            }
            return ReadyWait::TimedOut;
        }
        // Index order is priority order when several are signalled at once.
        let handles = [ready, failed, process];
        let r = WaitForMultipleObjects(3, handles.as_ptr(), 0, timeout_ms);
        CloseHandle(ready);
        CloseHandle(failed);
        match r {
            0 => ReadyWait::Ready,
            1 => ReadyWait::BootstrapFailed,
            2 => ReadyWait::Exited,
            _ => ReadyWait::TimedOut,
        }
    }
}

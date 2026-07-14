mod datapage;
mod stub;

use core::ffi::c_void;
use std::env;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Diagnostics::Debug::{
    FlushInstructionCache, ReadProcessMemory, WriteProcessMemory,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
use windows_sys::Win32::System::Memory::{
    VirtualAllocEx, MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE, PAGE_READWRITE,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, ResumeThread, WaitForSingleObject, CREATE_SUSPENDED,
    INFINITE, PROCESS_INFORMATION, STARTUPINFOW,
};

type NtSetInformationProcessFn = unsafe extern "system" fn(
    process_handle: HANDLE,
    process_information_class: u32,
    process_information: *mut c_void,
    process_information_length: u32,
) -> i32;

#[repr(C)]
struct ProcessInstrumentationCallbackInfo {
    version: u32,
    reserved: u32,
    callback: *mut c_void,
}

const PROCESS_INSTRUMENTATION_CALLBACK: u32 = 40;

/// Resolves `ntdll!NtSetInformationProcess`. Mirrors the
/// GetModuleHandleA/GetProcAddress pattern proven in
/// crates/vfs-shim/src/hook.rs for NtCreateFile-family exports.
unsafe fn resolve_nt_set_information_process() -> NtSetInformationProcessFn {
    let ntdll = GetModuleHandleA(b"ntdll.dll\0".as_ptr());
    assert!(!ntdll.is_null(), "GetModuleHandleA(ntdll.dll) failed");
    let proc = GetProcAddress(ntdll, b"NtSetInformationProcess\0".as_ptr())
        .expect("GetProcAddress(NtSetInformationProcess) failed");
    core::mem::transmute::<_, NtSetInformationProcessFn>(proc)
}

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// `target-exe.exe` lives next to this binary (same workspace target dir).
fn target_exe_path() -> String {
    let mut path = env::current_exe().expect("current_exe");
    path.set_file_name("target-exe.exe");
    path.to_string_lossy().into_owned()
}

fn main() {
    let target = target_exe_path();
    let app_w = wide(&target);
    let mut cmd_w = wide(&format!("\"{target}\""));

    // SAFETY: standard CreateProcessW + suspend/resume/wait; every handle
    // opened here is closed before the function returns.
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
            core::ptr::null(),
            &si,
            &mut pi,
        );
        assert!(ok != 0, "CreateProcessW failed: {}", std::io::Error::last_os_error());
        println!("target created suspended, pid={}", pi.dwProcessId);

        // --- Task 3: write the stub + data page into the suspended target ---
        let mut local_stub = stub::stub_bytes();
        let stub_len = local_stub.len();
        println!("extracted stub: {stub_len} bytes");

        let data_remote = VirtualAllocEx(
            pi.hProcess,
            core::ptr::null(),
            datapage::DATA_PAGE_SIZE,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        );
        assert!(!data_remote.is_null(), "VirtualAllocEx(data page) failed");

        stub::patch_data_page_address(&mut local_stub, data_remote as u64);

        let stub_remote = VirtualAllocEx(
            pi.hProcess,
            core::ptr::null(),
            stub_len,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_EXECUTE_READWRITE,
        );
        assert!(!stub_remote.is_null(), "VirtualAllocEx(stub page) failed");

        let mut written = 0usize;
        let ok = WriteProcessMemory(
            pi.hProcess,
            stub_remote,
            local_stub.as_ptr() as *const c_void,
            stub_len,
            &mut written,
        );
        assert!(ok != 0 && written == stub_len, "WriteProcessMemory(stub) failed");

        // Required after writing fresh code into an executable page: the CPU
        // isn't guaranteed to see it without an explicit cache flush.
        FlushInstructionCache(pi.hProcess, stub_remote, stub_len);

        // Verify: read the stub back and diff against the local (patched) copy.
        let mut readback = vec![0u8; stub_len];
        let mut read_n = 0usize;
        let ok = ReadProcessMemory(
            pi.hProcess,
            stub_remote,
            readback.as_mut_ptr() as *mut c_void,
            stub_len,
            &mut read_n,
        );
        assert!(ok != 0 && read_n == stub_len, "ReadProcessMemory(stub verify) failed");
        assert_eq!(readback, local_stub, "stub bytes in target do not match what was written");
        println!(
            "stub written and verified at remote addr {:#x}, data page at {:#x}",
            stub_remote as usize, data_remote as usize
        );

        let nt_set_information_process = resolve_nt_set_information_process();
        let mut info = ProcessInstrumentationCallbackInfo {
            version: 1,
            reserved: 0,
            callback: stub_remote,
        };
        let status = nt_set_information_process(
            pi.hProcess,
            PROCESS_INSTRUMENTATION_CALLBACK,
            &mut info as *mut _ as *mut c_void,
            size_of::<ProcessInstrumentationCallbackInfo>() as u32,
        );
        println!("NtSetInformationProcess status = {status:#x}");

        let resumed = ResumeThread(pi.hThread);
        assert!(resumed != u32::MAX, "ResumeThread failed: {}", std::io::Error::last_os_error());

        // target-exe deliberately sleeps ~1s before exiting; read the data
        // page well within that window, while its address space still
        // exists. A process's memory is reclaimed once it fully exits, so
        // reading only after WaitForSingleObject(INFINITE) is too late.
        std::thread::sleep(std::time::Duration::from_millis(300));

        let mut data_buf = vec![0u8; datapage::DATA_PAGE_SIZE];
        let mut read_n = 0usize;
        let ok = ReadProcessMemory(
            pi.hProcess,
            data_remote,
            data_buf.as_mut_ptr() as *mut c_void,
            data_buf.len(),
            &mut read_n,
        );
        assert!(ok != 0 && read_n == data_buf.len(), "ReadProcessMemory(data page) failed");
        let decoded = datapage::decode(&data_buf);
        println!(
            "fire_count={} first_r10={:#x} thunk_value={:#x}",
            decoded.fire_count, decoded.first_r10, decoded.thunk_value
        );
        println!("VERDICT: {}", decoded.verdict_str());
        if decoded.fire_count == 0 {
            println!(
                "NOTE: callback never fired. Check the NtSetInformationProcess status \
                 above first (nonzero = install rejected) before concluding anything \
                 about timing."
            );
        }

        let wait = WaitForSingleObject(pi.hProcess, INFINITE);
        assert_eq!(wait, 0, "WaitForSingleObject unexpected result: {wait}");

        let mut exit_code: u32 = 0;
        let got = GetExitCodeProcess(pi.hProcess, &mut exit_code);
        assert!(got != 0, "GetExitCodeProcess failed");
        println!("target exited with code {exit_code}");

        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    }
}

mod datapage;
mod stub;

use core::ffi::c_void;
use std::env;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Diagnostics::Debug::{
    FlushInstructionCache, ReadProcessMemory, WriteProcessMemory,
};
use windows_sys::Win32::System::Memory::{
    VirtualAllocEx, MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE, PAGE_READWRITE,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, ResumeThread, WaitForSingleObject, CREATE_SUSPENDED,
    INFINITE, PROCESS_INFORMATION, STARTUPINFOW,
};

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

        let resumed = ResumeThread(pi.hThread);
        assert!(resumed != u32::MAX, "ResumeThread failed: {}", std::io::Error::last_os_error());

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

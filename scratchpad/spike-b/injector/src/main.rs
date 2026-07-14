use std::env;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::CloseHandle;
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

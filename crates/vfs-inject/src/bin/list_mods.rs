use std::mem::{size_of, zeroed};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HMODULE};
use windows_sys::Win32::System::ProcessStatus::{EnumProcessModules, GetModuleBaseNameA, GetModuleInformation, MODULEINFO};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, TerminateProcess, CREATE_SUSPENDED, PROCESS_INFORMATION, STARTUPINFOW,
};

fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() }

fn main() {
    let host = std::env::args().nth(1).unwrap_or_else(|| std::env::current_exe().unwrap().to_string_lossy().into_owned());
    unsafe {
        let mut si: STARTUPINFOW = zeroed();
        si.cb = size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = zeroed();
        let app = wide(&host);
        let mut cmd = wide(&format!("\"{host}\""));
        let ok = CreateProcessW(app.as_ptr(), cmd.as_mut_ptr(), std::ptr::null(), std::ptr::null(), 0, CREATE_SUSPENDED, std::ptr::null(), std::ptr::null(), &si, &mut pi);
        assert!(ok!=0);
        eprintln!("suspended pid={}", pi.dwProcessId);
        let mut mods = [0usize; 256];
        let mut needed = 0u32;
        // EnumProcessModules needs PROCESS_QUERY_INFORMATION | PROCESS_VM_READ — CreateProcess gives us full access
        let r = EnumProcessModules(pi.hProcess, mods.as_mut_ptr() as *mut HMODULE, (mods.len()*8) as u32, &mut needed);
        eprintln!("EnumProcessModules r={r} needed={needed}");
        let count = (needed as usize)/8;
        for i in 0..count.min(64) {
            let mut name = [0u8; 256];
            GetModuleBaseNameA(pi.hProcess, mods[i] as HMODULE, name.as_mut_ptr(), 256);
            let end = name.iter().position(|&c| c==0).unwrap_or(255);
            let nm = String::from_utf8_lossy(&name[..end]);
            let mut mi: MODULEINFO = zeroed();
            GetModuleInformation(pi.hProcess, mods[i] as HMODULE, &mut mi, size_of::<MODULEINFO>() as u32);
            eprintln!("  {nm} base={:p} size=0x{:x}", mi.lpBaseOfDll, mi.SizeOfImage);
        }
        TerminateProcess(pi.hProcess, 0);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    }
}

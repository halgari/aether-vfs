use std::mem::{size_of, zeroed};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, ResumeThread, WaitForSingleObject, CREATE_SUSPENDED,
    PROCESS_INFORMATION, STARTUPINFOW,
};

fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() }

fn main() {
    let exe = std::env::args().nth(1).expect("exe");
    let pe = std::fs::read(&exe).unwrap();
    let e = u32::from_le_bytes(pe[0x3C..0x40].try_into().unwrap()) as usize;
    let opt = e + 24;
    let entry_rva = u32::from_le_bytes(pe[opt+16..opt+20].try_into().unwrap()) as usize;
    let soimage = u32::from_le_bytes(pe[opt+56..opt+60].try_into().unwrap()) as usize;
    let soh = u32::from_le_bytes(pe[opt+60..opt+64].try_into().unwrap()) as usize;
    let nsec = u16::from_le_bytes(pe[e+6..e+8].try_into().unwrap()) as usize;
    let so = u16::from_le_bytes(pe[e+20..e+22].try_into().unwrap()) as usize;
    let sb = opt + so;
    let mut img = vec![0u8; soimage];
    img[..soh].copy_from_slice(&pe[..soh]);
    for i in 0..nsec {
        let s = sb + i*40;
        let va = u32::from_le_bytes(pe[s+12..s+16].try_into().unwrap()) as usize;
        let rs = u32::from_le_bytes(pe[s+16..s+20].try_into().unwrap()) as usize;
        let rp = u32::from_le_bytes(pe[s+20..s+24].try_into().unwrap()) as usize;
        if rs>0 { img[va..va+rs].copy_from_slice(&pe[rp..rp+rs]); }
    }
    eprintln!("local entry bytes: {:02x?}", &img[entry_rva..entry_rva+16]);

    unsafe {
        let mut si: STARTUPINFOW = zeroed();
        si.cb = size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = zeroed();
        let mut cmd = wide(&format!("\"{exe}\""));
        let app = wide(&exe);
        let ok = CreateProcessW(app.as_ptr(), cmd.as_mut_ptr(), std::ptr::null(), std::ptr::null(), 0, CREATE_SUSPENDED, std::ptr::null(), std::ptr::null(), &si, &mut pi);
        assert!(ok != 0, "CreateProcess failed");
        type NtQip = unsafe extern "system" fn(HANDLE, u32, *mut u8, u32, *mut u32) -> i32;
        let ntdll = windows_sys::Win32::System::LibraryLoader::GetModuleHandleA(b"ntdll.dll\0".as_ptr());
        let ntqip: NtQip = std::mem::transmute(windows_sys::Win32::System::LibraryLoader::GetProcAddress(ntdll, b"NtQueryInformationProcess\0".as_ptr()).unwrap());
        let mut pbi = [0u8; 48];
        let mut rl = 0u32;
        let st = ntqip(pi.hProcess, 0, pbi.as_mut_ptr(), 48, &mut rl);
        assert!(st == 0, "NtQIP {st}");
        let peb = usize::from_le_bytes(pbi[8..16].try_into().unwrap());
        let mut base = 0u64;
        let mut n = 0usize;
        ReadProcessMemory(pi.hProcess, (peb+0x10) as *const _, &mut base as *mut _ as *mut _, 8, &mut n);
        eprintln!("real process imagebase=0x{base:x} peb=0x{peb:x}");
        let mut remote_entry = [0u8; 16];
        ReadProcessMemory(pi.hProcess, (base as usize + entry_rva) as *const _, remote_entry.as_mut_ptr() as *mut _, 16, &mut n);
        eprintln!("remote entry bytes: {:02x?}", remote_entry);
        eprintln!("match={}", remote_entry.as_slice() == &img[entry_rva..entry_rva+16]);
        ResumeThread(pi.hThread);
        WaitForSingleObject(pi.hProcess, 5000);
        let mut code = 0u32;
        GetExitCodeProcess(pi.hProcess, &mut code);
        eprintln!("real run exit=0x{code:x} i32={}", code as i32);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    }
}

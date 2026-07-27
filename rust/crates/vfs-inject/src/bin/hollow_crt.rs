//! Hollow image then start via CreateRemoteThread(entry) instead of ResumeThread.
use std::mem::{size_of, zeroed};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory};
use windows_sys::Win32::System::Memory::{VirtualAllocEx, MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, CreateRemoteThread, GetExitCodeProcess, WaitForSingleObject, CREATE_SUSPENDED,
    PROCESS_INFORMATION, STARTUPINFOW, LPTHREAD_START_ROUTINE,
};

fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() }

fn main() {
    let pe_path = std::env::args().nth(1).unwrap();
    let pe = std::fs::read(&pe_path).unwrap();
    let host = std::env::current_exe().unwrap();
    let host_s = host.to_string_lossy().into_owned();
    eprintln!("host={host_s}");

    // Use library hollow
    let virt = pe_path.clone();
    match vfs_inject::create_process_from_pe_bytes(&pe, &virt, &[], None) {
        Ok((proc, thread, pid, tid)) => {
            eprintln!("hollowed pid={pid} tid={tid} — starting via CreateRemoteThread not ResumeThread");
            // Read PEB image base
            unsafe {
                type NtQip = unsafe extern "system" fn(HANDLE, u32, *mut u8, u32, *mut u32) -> i32;
                let ntdll = windows_sys::Win32::System::LibraryLoader::GetModuleHandleA(b"ntdll.dll\0".as_ptr());
                let ntqip: NtQip = std::mem::transmute(
                    windows_sys::Win32::System::LibraryLoader::GetProcAddress(ntdll, b"NtQueryInformationProcess\0".as_ptr()).unwrap()
                );
                let mut pbi = [0u8; 48];
                let mut rl = 0u32;
                ntqip(proc, 0, pbi.as_mut_ptr(), 48, &mut rl);
                let peb = usize::from_le_bytes(pbi[8..16].try_into().unwrap());
                let mut base = 0u64;
                let mut n = 0usize;
                ReadProcessMemory(proc, (peb+0x10) as *const _, &mut base as *mut _ as *mut _, 8, &mut n);
                let e = u32::from_le_bytes(pe[0x3C..0x40].try_into().unwrap()) as usize;
                let entry_rva = u32::from_le_bytes(pe[e+24+16..e+24+20].try_into().unwrap()) as u64;
                let entry = base + entry_rva;
                eprintln!("remote base=0x{base:x} entry=0x{entry:x}");

                // CreateRemoteThread at entry (ignore original primary thread)
                let start: LPTHREAD_START_ROUTINE = Some(std::mem::transmute(entry as usize));
                let ht = CreateRemoteThread(proc, std::ptr::null(), 0, start, std::ptr::null(), 0, std::ptr::null_mut());
                if ht.is_null() {
                    let err = windows_sys::Win32::Foundation::GetLastError();
                    eprintln!("CreateRemoteThread failed err={err}");
                } else {
                    WaitForSingleObject(ht, 15000);
                    CloseHandle(ht);
                }
                let mut code = 0u32;
                GetExitCodeProcess(proc, &mut code);
                eprintln!("exit=0x{code:x} i32={}", code as i32);
                // kill primary
                windows_sys::Win32::System::Threading::TerminateProcess(proc, code);
                CloseHandle(thread);
                CloseHandle(proc);
            }
        }
        Err(e) => eprintln!("hollow failed: {e}"),
    }
}

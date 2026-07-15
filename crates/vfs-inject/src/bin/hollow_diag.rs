fn main() {
    let pe_path = std::env::args().nth(1).expect("pe");
    let pe = std::fs::read(&pe_path).expect("read");
    eprintln!("read {} ({} bytes)", pe_path, pe.len());
    if std::env::args().nth(2).as_deref() == Some("resolve") {
        eprintln!("resolve mode removed");
        return;
    }
    let virt = std::env::args().nth(2).unwrap_or_else(|| pe_path.clone());
    let cwd = std::env::args().nth(3);
    match vfs_inject::create_process_from_pe_bytes(&pe, &virt, &[], cwd.as_deref()) {
        Ok((proc, thread, pid, tid)) => {
            eprintln!("hollowed pid={pid} tid={tid}");
            unsafe {
                // rcx mode: primary must be resumed. thread mode: already running.
                let mode = std::env::var("VFS_HOLLOW_START").unwrap_or_else(|_| "rcx".into());
                if mode != "thread" {
                    windows_sys::Win32::System::Threading::ResumeThread(thread);
                }
                let wait = windows_sys::Win32::System::Threading::WaitForSingleObject(proc, 15_000);
                let mut code = 0u32;
                windows_sys::Win32::System::Threading::GetExitCodeProcess(proc, &mut code);
                eprintln!("wait={wait} exit_code=0x{code:x} (i32={})", code as i32);
                windows_sys::Win32::Foundation::CloseHandle(thread);
                windows_sys::Win32::Foundation::CloseHandle(proc);
            }
        }
        Err(e) => {
            eprintln!("hollow failed: {e}");
            std::process::exit(1);
        }
    }
}

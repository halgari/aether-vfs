//! Stands in for Proton's Steam helper in `vfs-inject`'s tests.
//!
//! Usage: `vfs-fake-steam-helper <mode> [key]`
//! - `publish <key>`: write its own pid to `HKCU\<key>\pid`, then wait 30 s;
//! - `sleep`: publish nothing and wait 30 s (a helper stuck in start-up);
//! - `exit`: exit at once.
//!
//! Windows only; elsewhere it exits 2.

#[cfg(not(windows))]
fn main() {
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("publish") => {
            let Some(key) = args.get(2) else {
                std::process::exit(2);
            };
            publish(key);
            std::thread::sleep(std::time::Duration::from_secs(30));
        }
        Some("sleep") => std::thread::sleep(std::time::Duration::from_secs(30)),
        Some("exit") => {}
        _ => std::process::exit(2),
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn publish(key: &str) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Registry::{RegSetKeyValueW, HKEY_CURRENT_USER, REG_DWORD};
    let wide = |s: &str| -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let (key, value) = (wide(key), wide("pid"));
    let pid = std::process::id();
    // SAFETY: NUL-terminated strings and four bytes of data, all outliving
    // the call.
    unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            REG_DWORD,
            (&pid as *const u32).cast(),
            4,
        );
    }
}

//! The profile-API (INI) fixture; see `windows.rs`. Windows only: it calls
//! `GetPrivateProfileStringW` and friends directly.

#[cfg(windows)]
mod windows;

#[cfg(windows)]
fn main() {
    windows::main()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("vfs-fixture-prefs is a Windows program; build it with bin/build-windows");
    std::process::exit(11);
}

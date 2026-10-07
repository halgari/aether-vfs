//! The escape fixture; see `windows.rs`. Windows only: the vectors are NT and
//! Win32 path spellings.

#[cfg(windows)]
mod windows;

#[cfg(windows)]
fn main() {
    windows::main()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("vfs-fixture-escape is a Windows program; build it with bin/build-windows");
    std::process::exit(11);
}

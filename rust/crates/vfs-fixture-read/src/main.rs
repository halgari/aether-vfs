//! Injection read target: opens a (virtual) file via the normal Win32 path
//! (std::fs::read → CreateFileW → NtCreateFile, so the injected shim's hooks
//! intercept it), and asserts its length/content. Exit 0 iff it matches.
use std::process::exit;

fn main() {
    let path = std::env::var("VFS_FIXTURE_PATH").unwrap_or_else(|_| {
        eprintln!("VFS_FIXTURE_PATH unset"); exit(2);
    });
    let expect_len: usize = std::env::var("VFS_FIXTURE_EXPECT")
        .ok().and_then(|s| s.parse().ok())
        .unwrap_or_else(|| { eprintln!("VFS_FIXTURE_EXPECT unset/bad"); exit(2); });
    let fill: Option<u8> = std::env::var("VFS_FIXTURE_FILL").ok()
        .and_then(|s| s.parse().ok());

    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) => { eprintln!("FIXTURE FAIL: read {path}: {e}"); exit(1); }
    };
    if data.len() != expect_len {
        eprintln!("FIXTURE FAIL: len {} != {expect_len}", data.len()); exit(1);
    }
    if let Some(b) = fill {
        if data.iter().any(|&x| x != b) {
            eprintln!("FIXTURE FAIL: content byte != {b}"); exit(1);
        }
    }
    println!("FIXTURE OK: {} bytes", data.len());
    exit(0);
}

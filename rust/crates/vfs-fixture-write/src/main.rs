//! Injection write target: creates a (virtual) file via the normal Win32 path
//! (std::fs::write → NtCreateFile write-disposition + NtWriteFile, hooked by the
//! injected shim), reads it back, and exits 0 iff the round-trip matches.
use std::process::exit;

fn main() {
    let path = std::env::var("VFS_FIXTURE_PATH").unwrap_or_else(|_| {
        eprintln!("VFS_FIXTURE_PATH unset"); exit(2);
    });
    let data = std::env::var("VFS_FIXTURE_DATA").unwrap_or_else(|_| "written-bytes".into());
    if let Err(e) = std::fs::write(&path, data.as_bytes()) {
        eprintln!("WRITE FIXTURE FAIL: write {path}: {e}"); exit(1);
    }
    match std::fs::read(&path) {
        Ok(back) if back == data.as_bytes() => {
            println!("WRITE FIXTURE OK: {} bytes round-tripped", back.len()); exit(0);
        }
        Ok(back) => { eprintln!("WRITE FIXTURE FAIL: read-back {} != written {}", back.len(), data.len()); exit(1); }
        Err(e) => { eprintln!("WRITE FIXTURE FAIL: read-back {path}: {e}"); exit(1); }
    }
}

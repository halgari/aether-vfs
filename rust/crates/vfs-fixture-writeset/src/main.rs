//! Injection write-set target. Exercises the full write set — mkdir, truncate
//! (`File::set_len`), delete, rename — through the normal Win32 filesystem path
//! (hooked by the injected shim → routed over the ring to the JVM overlay), and
//! verifies each effect in-process via the read/attr hooks. Exits 0 iff every
//! step succeeds; a distinct non-zero code names the failing step.
//!
//! Env: `VFS_FIXTURE_DIR` — the virtual directory the ops run under
//! (required).
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::exit;

fn fail(code: i32, msg: String) -> ! {
    eprintln!("WRITESET FIXTURE FAIL [{code}]: {msg}");
    exit(code);
}

fn main() {
    let Ok(dir) = std::env::var("VFS_FIXTURE_DIR") else {
        eprintln!("VFS_FIXTURE_DIR unset");
        exit(2);
    };
    let dir = Path::new(&dir);

    // 1. mkdir → a fresh virtual directory that reads back as a directory.
    let made = dir.join("madedir");
    if let Err(e) = fs::create_dir(&made) {
        fail(10, format!("create_dir {}: {e}", made.display()));
    }
    match fs::metadata(&made) {
        Ok(m) if m.is_dir() => println!("mkdir OK: {}", made.display()),
        Ok(_) => fail(10, format!("{} exists but is not a directory", made.display())),
        Err(e) => fail(10, format!("metadata {} after mkdir: {e}", made.display())),
    }
    // Idempotency: creating an existing dir must report AlreadyExists (the
    // standard create-and-ignore idiom), NOT a generic failure.
    match fs::create_dir(&made) {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            println!("mkdir-existing OK: reported AlreadyExists");
        }
        Err(e) => fail(10, format!("re-create {} gave {:?}, want AlreadyExists", made.display(), e.kind())),
        Ok(()) => fail(10, format!("re-create {} unexpectedly succeeded", made.display())),
    }

    // 2. truncate → write 5 bytes, set_len(2) on the SAME open write handle,
    //    close, reopen for read: it must be exactly 2 bytes.
    let trunc = dir.join("trunc.bin");
    {
        let mut f = match fs::File::create(&trunc) {
            Ok(f) => f,
            Err(e) => fail(11, format!("create {}: {e}", trunc.display())),
        };
        if let Err(e) = f.write_all(b"12345") {
            fail(11, format!("write {}: {e}", trunc.display()));
        }
        if let Err(e) = f.set_len(2) {
            fail(11, format!("set_len {}: {e}", trunc.display()));
        }
    } // drop → close → overlay flush
    match fs::read(&trunc) {
        Ok(b) if b.len() == 2 => println!("truncate OK: {} -> {} bytes", trunc.display(), b.len()),
        Ok(b) => fail(11, format!("{} is {} bytes, expected 2", trunc.display(), b.len())),
        Err(e) => fail(11, format!("read-back {}: {e}", trunc.display())),
    }

    // 3. delete → create a file, remove it; a follow-up stat must miss.
    let del = dir.join("del.txt");
    if let Err(e) = fs::write(&del, b"doomed") {
        fail(12, format!("write {}: {e}", del.display()));
    }
    if let Err(e) = fs::remove_file(&del) {
        fail(12, format!("remove_file {}: {e}", del.display()));
    }
    match fs::metadata(&del) {
        Err(_) => println!("delete OK: {} is gone", del.display()),
        Ok(_) => fail(12, format!("{} still present after delete", del.display())),
    }

    // 4. rename → create a.txt, rename to b.txt; b must exist and a must not.
    let ren_a = dir.join("ren_a.txt");
    let ren_b = dir.join("ren_b.txt");
    if let Err(e) = fs::write(&ren_a, b"movable") {
        fail(13, format!("write {}: {e}", ren_a.display()));
    }
    if let Err(e) = fs::rename(&ren_a, &ren_b) {
        fail(13, format!("rename {} -> {}: {e}", ren_a.display(), ren_b.display()));
    }
    if fs::metadata(&ren_b).is_err() {
        fail(13, format!("{} missing after rename", ren_b.display()));
    }
    if fs::metadata(&ren_a).is_ok() {
        fail(13, format!("{} still present after rename", ren_a.display()));
    }
    println!("rename OK: {} -> {}", ren_a.display(), ren_b.display());

    println!("WRITESET FIXTURE OK: mkdir/truncate/delete/rename all passed");
    exit(0);
}

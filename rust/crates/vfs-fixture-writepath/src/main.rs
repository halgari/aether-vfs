//! End-to-end write-path fixture. Under its (virtual) working directory —
//! `Session::launch` sets the child's cwd to the session root, so plain
//! relative `std::fs` paths route through the injected shim — it creates
//! `write-probe.txt`, writes "hello", reopens for append, appends "world",
//! reads the whole file back and checks it is exactly "helloworld", then
//! renames it and exercises create+delete of a second file.
//!
//! Every failure exits a distinct non-zero code so the host can tell which
//! step failed without guessing: 2 create, 3 write, 4 append-open, 5
//! append-write, 6 readback mismatch, 7 rename, 8 second-file create/write,
//! 9 delete, 10 deleted file still exists.
use std::fs::OpenOptions;
use std::io::Write;
use std::process::exit;

const PATH: &str = "write-probe.txt";
const RENAMED: &str = "renamed-probe.txt";
const DELETE_PATH: &str = "delete-probe.txt";

fn main() {
    // 1. Create + write "hello".
    let mut f = match OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(PATH)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("WRITEPATH FIXTURE FAIL [2]: create {PATH}: {e}");
            exit(2);
        }
    };
    if let Err(e) = f.write_all(b"hello") {
        eprintln!("WRITEPATH FIXTURE FAIL [3]: write {PATH}: {e}");
        exit(3);
    }
    drop(f);

    // 2. Reopen for append, write "world".
    let mut f = match OpenOptions::new().append(true).open(PATH) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("WRITEPATH FIXTURE FAIL [4]: append-open {PATH}: {e}");
            exit(4);
        }
    };
    if let Err(e) = f.write_all(b"world") {
        eprintln!("WRITEPATH FIXTURE FAIL [5]: append-write {PATH}: {e}");
        exit(5);
    }
    drop(f);

    // 3. Readback: the whole file must be exactly "helloworld".
    match std::fs::read(PATH) {
        Ok(data) if data == b"helloworld" => {
            println!("WRITEPATH FIXTURE OK: {} bytes round-tripped", data.len());
        }
        Ok(data) => {
            eprintln!(
                "WRITEPATH FIXTURE FAIL [6]: readback {:?} != \"helloworld\"",
                String::from_utf8_lossy(&data)
            );
            exit(6);
        }
        Err(e) => {
            eprintln!("WRITEPATH FIXTURE FAIL [6]: readback {PATH}: {e}");
            exit(6);
        }
    }

    // 4. Rename write-probe.txt -> renamed-probe.txt, then confirm the new
    // name reads back the same bytes and the old name is gone.
    if let Err(e) = std::fs::rename(PATH, RENAMED) {
        eprintln!("WRITEPATH FIXTURE FAIL [7]: rename {PATH} -> {RENAMED}: {e}");
        exit(7);
    }
    match std::fs::read(RENAMED) {
        Ok(data) if data == b"helloworld" => {}
        Ok(data) => {
            eprintln!(
                "WRITEPATH FIXTURE FAIL [7]: {RENAMED} readback {:?} != \"helloworld\"",
                String::from_utf8_lossy(&data)
            );
            exit(7);
        }
        Err(e) => {
            eprintln!("WRITEPATH FIXTURE FAIL [7]: readback {RENAMED}: {e}");
            exit(7);
        }
    }
    if std::fs::metadata(PATH).is_ok() {
        eprintln!("WRITEPATH FIXTURE FAIL [7]: {PATH} still exists after rename");
        exit(7);
    }

    // 5. Create + write a second file, to be deleted next.
    match OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(DELETE_PATH)
        .and_then(|mut f| f.write_all(b"delete-me"))
    {
        Ok(()) => {}
        Err(e) => {
            eprintln!("WRITEPATH FIXTURE FAIL [8]: create/write {DELETE_PATH}: {e}");
            exit(8);
        }
    }

    // 6. Delete it.
    if let Err(e) = std::fs::remove_file(DELETE_PATH) {
        eprintln!("WRITEPATH FIXTURE FAIL [9]: delete {DELETE_PATH}: {e}");
        exit(9);
    }

    // 7. Confirm it is actually gone (a silently-swallowed delete would leave
    // this metadata probe succeeding).
    if std::fs::metadata(DELETE_PATH).is_ok() {
        eprintln!("WRITEPATH FIXTURE FAIL [10]: {DELETE_PATH} still exists after delete");
        exit(10);
    }

    println!("WRITEPATH FIXTURE OK: rename + delete round-tripped");
    exit(0);
}

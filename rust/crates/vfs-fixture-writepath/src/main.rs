//! End-to-end write-path fixture. Under its (virtual) working directory —
//! `Session::launch` sets the child's cwd to the session root, so plain
//! relative `std::fs` paths route through the injected shim — it creates
//! `write-probe.txt`, writes "hello", reopens for append, appends "world",
//! then reads the whole file back and checks it is exactly "helloworld".
//!
//! Every failure exits a distinct non-zero code so the host can tell which
//! step failed without guessing: 2 create, 3 write, 4 append-open, 5
//! append-write, 6 readback mismatch.
//!
//! Scope note: rename and delete are intentionally NOT exercised here. Those
//! need shim-side routing that a later task adds; a fixture that can fail for
//! two unrelated reasons diagnoses neither.
use std::fs::OpenOptions;
use std::io::Write;
use std::process::exit;

const PATH: &str = "write-probe.txt";

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
            exit(0);
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
}

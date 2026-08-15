//! End-to-end write-path fixture. Under its (virtual) working directory —
//! `Session::launch` sets the child's cwd to the session root, so plain
//! relative `std::fs` paths route through the injected shim — it creates
//! `write-probe.txt`, writes "hello", reopens for append, appends "world",
//! reads the whole file back and checks it is exactly "helloworld", then
//! renames it and exercises create+delete of a second file. It then proves
//! two behavioural fixes that are only reachable once writes cross the ring
//! instead of falling through to a real file: a same-handle write-then-seek-
//! then-read (the synthetic size must track a write within one open handle,
//! not only across a close+reopen), and `CREATE_NEW` exclusivity against an
//! already-existing path (must fail, not silently "succeed" via the
//! shim-local overlay bypass).
//!
//! With `VFS_FIXTURE_COW_PATH` set it first edits that path **in place** —
//! the copy-on-write case, see [`COW_PATH_ENV`] — which is the only step here
//! that needs the session to have a write layer. Unset, the step is skipped
//! and this fixture is exactly what it was before gate 4 Task 6b.
//!
//! Every failure exits a distinct non-zero code so the host can tell which
//! step failed without guessing: 2 create, 3 write, 4 append-open, 5
//! append-write, 6 readback mismatch, 7 rename, 8 second-file create/write,
//! 9 delete, 10 deleted file still exists, 11 same-handle create, 12
//! same-handle write, 13 same-handle seek, 14 same-handle readback mismatch,
//! 15 exclusive-create of a fresh path, 16 exclusive-create against an
//! existing path did not fail as expected, 17 in-place open refused, 18
//! in-place content missing or short (a blank destination, not a seeded
//! copy-up), 19 in-place write, 20 in-place readback mismatch.
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::process::exit;
use std::time::Duration;

/// Set `VFS_FIXTURE_COW_PATH` to a path the session serves from a **read-only**
/// source and this fixture edits it in place first: `OPEN_EXISTING` +
/// read/write, no create and no truncate — `fopen(…, "r+b")`, what a mod tool
/// or an INI writer does. Nothing writable holds that path, so only the
/// director's copy-up can answer it. Unset (every scenario that predates gate
/// 4 Task 6b) the step is skipped entirely and this fixture behaves exactly as
/// before. Exit codes 17-20.
const COW_PATH_ENV: &str = "VFS_FIXTURE_COW_PATH";
/// Offset the in-place edit writes at, chosen so both ends of the original
/// content stay untouched: a truncating or blank-file implementation cannot
/// produce the expected result, and a seeded-then-clobbered one cannot either.
const COW_OFFSET: u64 = 9;
const COW_EDIT: &[u8] = b"EDITED";

const PATH: &str = "write-probe.txt";
const RENAMED: &str = "renamed-probe.txt";
const DELETE_PATH: &str = "delete-probe.txt";
const SAME_HANDLE_PATH: &str = "same-handle-probe.txt";
const EXCL_PATH: &str = "excl-probe.txt";

fn main() {
    // 0. Copy-on-write, if the host asked for it: edit read-only content in
    //    place. Runs first so nothing this fixture wrote can be mistaken for
    //    the copied-up file.
    if let Ok(cow_path) = std::env::var(COW_PATH_ENV) {
        in_place_edit(&cow_path);
    }

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

    // 8. Same-handle write, then seek back and read, with no close/reopen in
    // between. A fresh reopen always gets an accurate size straight from the
    // director, so this is the only shape that can see the synthetic size
    // going stale after a write: without the fix, the handle's cached size
    // stays 0 (its value at open), so the seek-then-read below sees EOF at
    // offset 0 and reads back nothing instead of "hello".
    let mut f = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(SAME_HANDLE_PATH)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("WRITEPATH FIXTURE FAIL [11]: create {SAME_HANDLE_PATH}: {e}");
            exit(11);
        }
    };
    if let Err(e) = f.write_all(b"hello") {
        eprintln!("WRITEPATH FIXTURE FAIL [12]: write {SAME_HANDLE_PATH}: {e}");
        exit(12);
    }
    if let Err(e) = f.seek(SeekFrom::Start(0)) {
        eprintln!("WRITEPATH FIXTURE FAIL [13]: seek {SAME_HANDLE_PATH}: {e}");
        exit(13);
    }
    let mut buf = Vec::new();
    match f.read_to_end(&mut buf) {
        Ok(_) if buf == b"hello" => {}
        Ok(_) => {
            eprintln!(
                "WRITEPATH FIXTURE FAIL [14]: same-handle readback {:?} != \"hello\"",
                String::from_utf8_lossy(&buf)
            );
            exit(14);
        }
        Err(e) => {
            eprintln!("WRITEPATH FIXTURE FAIL [14]: same-handle readback {SAME_HANDLE_PATH}: {e}");
            exit(14);
        }
    }
    drop(f);
    println!("WRITEPATH FIXTURE OK: same-handle write-then-read round-tripped");

    // 9. CREATE_NEW (OPEN_EXCL) exclusivity. The first create-new against a
    // fresh path must succeed; a second create-new against the now-existing
    // path must fail. Before the fix, the director's write-open error for an
    // existing path had no distinct status, so the shim treated it like any
    // other director refusal and fell through to the shim-local overlay —
    // which created the file there and reported success, so an exclusive
    // create against an existing file silently "succeeded".
    match OpenOptions::new().write(true).create_new(true).open(EXCL_PATH) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(b"first") {
                eprintln!("WRITEPATH FIXTURE FAIL [15]: write {EXCL_PATH}: {e}");
                exit(15);
            }
        }
        Err(e) => {
            eprintln!("WRITEPATH FIXTURE FAIL [15]: exclusive-create {EXCL_PATH}: {e}");
            exit(15);
        }
    }
    match OpenOptions::new().write(true).create_new(true).open(EXCL_PATH) {
        Ok(_) => {
            eprintln!(
                "WRITEPATH FIXTURE FAIL [16]: exclusive-create against an existing {EXCL_PATH} succeeded"
            );
            exit(16);
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            eprintln!(
                "WRITEPATH FIXTURE FAIL [16]: exclusive-create against an existing {EXCL_PATH} \
                 failed with the wrong error kind: {e:?}"
            );
            exit(16);
        }
    }
    println!("WRITEPATH FIXTURE OK: CREATE_NEW exclusivity held");

    // The shim's hook-stats report is a periodic sample, not an exit dump —
    // nothing flushes it when a process exits, so any open recorded after the
    // reporter's last tick is invisible to a reader of the report file. A real
    // game session runs for minutes and this never matters, but this fixture's
    // last routed opens (the CREATE_NEW pair above) land microseconds before
    // exit, well inside one tick's window. Outlive at least one full tick so
    // the reporter has a chance to observe the fixture's final state before
    // the process disappears. Derived from the same interval the harness
    // configures (`VFS_SHIM_STATS_INTERVAL_MS`) rather than a fixed number, so
    // this stays correct if that interval ever changes.
    let interval_ms: u64 = std::env::var("VFS_SHIM_STATS_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(250);
    std::thread::sleep(Duration::from_millis(interval_ms.saturating_mul(2)));

    exit(0);
}

/// Copy-on-write over read-only content, driven the way an application drives
/// it: open an existing file for read+write with **no** create and no
/// truncate disposition, edit bytes in the middle, and read the whole file
/// back through the same handle.
///
/// The point is what the assertions here cannot be satisfied by. If the
/// director refuses the open (no write layer, or one composed as a sibling
/// rather than an overlay upper) this exits 17. If the write layer creates a
/// blank destination instead of seeding it from the read-only source, the
/// original bytes are missing and this exits 18 or 20. If copy-up truncates,
/// the tail is gone and this exits 20. Only a real seeded copy-up passes.
fn in_place_edit(path: &str) {
    let mut f = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "WRITEPATH FIXTURE FAIL [17]: in-place open {path}: {e} — a read-write open of \
                 read-only content must be served by copy-up"
            );
            exit(17);
        }
    };
    let mut original = Vec::new();
    if let Err(e) = f.read_to_end(&mut original) {
        eprintln!("WRITEPATH FIXTURE FAIL [18]: read {path}: {e}");
        exit(18);
    }
    if original.len() < COW_OFFSET as usize + COW_EDIT.len() {
        eprintln!(
            "WRITEPATH FIXTURE FAIL [18]: {path} is {} bytes; the handle must open onto the \
             copied-up content, so anything shorter than {} means the write layer created a \
             blank file instead of seeding from the source",
            original.len(),
            COW_OFFSET as usize + COW_EDIT.len()
        );
        exit(18);
    }

    if f.seek(SeekFrom::Start(COW_OFFSET)).is_err() || f.write_all(COW_EDIT).is_err() {
        eprintln!("WRITEPATH FIXTURE FAIL [19]: in-place write {path}");
        exit(19);
    }

    let mut expected = original.clone();
    expected[COW_OFFSET as usize..COW_OFFSET as usize + COW_EDIT.len()]
        .copy_from_slice(COW_EDIT);
    if let Err(e) = f.seek(SeekFrom::Start(0)) {
        eprintln!("WRITEPATH FIXTURE FAIL [20]: seek {path}: {e}");
        exit(20);
    }
    let mut back = Vec::new();
    match f.read_to_end(&mut back) {
        Ok(_) if back == expected => {}
        Ok(_) => {
            eprintln!(
                "WRITEPATH FIXTURE FAIL [20]: {path} readback {:?} != {:?} — the edit must land \
                 with every untouched byte preserved",
                String::from_utf8_lossy(&back),
                String::from_utf8_lossy(&expected)
            );
            exit(20);
        }
        Err(e) => {
            eprintln!("WRITEPATH FIXTURE FAIL [20]: readback {path}: {e}");
            exit(20);
        }
    }
    drop(f);
    println!(
        "WRITEPATH FIXTURE OK: in-place edit of {path} copied up ({} bytes)",
        expected.len()
    );
}

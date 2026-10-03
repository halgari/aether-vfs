//! The read-cache phase: many small reads of a file give the bytes one big
//! read gave, by every route a program reads — an explicit offset, the
//! handle's own position, seeks, several threads on one handle and on their
//! own — and a file rewritten through another handle reads back fresh.
//!
//! Driven by:
//!
//! - `VFS_FIXTURE_CACHE_PATH`: a file the director serves immutable, at least
//!   a few blocks (MiB) long. Read once in one call (large: never cached),
//!   then in small pieces, which the shim's read cache serves.
//! - `VFS_FIXTURE_CACHE_RW_PATH` and `VFS_FIXTURE_CACHE_RW_DATA`: a file
//!   first read in small pieces (cached, if it is immutable), then rewritten
//!   with `RW_DATA` through another handle, then read again in small pieces —
//!   which must give `RW_DATA`, not what was cached.
//!
//! Exits 1 with a `FIXTURE FAIL: cache:` line on the first difference.

use std::io::{Read, Seek, SeekFrom};
use std::os::windows::fs::FileExt;
use std::process::exit;

fn fail(msg: String) -> ! {
    eprintln!("FIXTURE FAIL: cache: {msg}");
    exit(1);
}

fn xorshift(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

/// Read `len` bytes at `off` with an explicit offset and check them against
/// `whole`: the count is `len` cut at end of file, and 0 at or past it.
fn check_at(f: &std::fs::File, whole: &[u8], off: u64, len: usize, what: &str) {
    let mut buf = vec![0xCDu8; len];
    let n = match f.seek_read(&mut buf, off) {
        Ok(n) => n,
        Err(e) => fail(format!("{what}: read {len} at {off}: {e}")),
    };
    let want = len.min(whole.len().saturating_sub(off as usize));
    if n != want {
        fail(format!(
            "{what}: read {len} at {off} gave {n} bytes, not {want}"
        ));
    }
    if n > 0 && buf[..n] != whole[off as usize..off as usize + n] {
        fail(format!("{what}: read {len} at {off}: wrong bytes"));
    }
}

/// Read all of `path` in small pieces through the handle's own position.
fn read_small(path: &str, piece: usize) -> Vec<u8> {
    let mut f = std::fs::File::open(path).unwrap_or_else(|e| fail(format!("open {path}: {e}")));
    let mut out = Vec::new();
    let mut buf = vec![0u8; piece];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) => fail(format!("read {path} in {piece}-byte pieces: {e}")),
        }
    }
    out
}

pub fn run() {
    let Ok(path) = std::env::var("VFS_FIXTURE_CACHE_PATH") else {
        return;
    };
    let whole = match crate::read_in_one_call(&path) {
        Ok(d) => d,
        Err(e) => fail(format!("one read of {path}: {e}")),
    };
    let size = whole.len() as u64;
    if size < (2 << 20) {
        fail(format!(
            "{path} is {size} bytes: too small to cross a block"
        ));
    }
    let mut small_reads = 0u64;

    // Explicit offsets: every size either side of the cache's threshold, at
    // block boundaries, at and past end of file, and at random.
    let f = std::fs::File::open(&path).unwrap_or_else(|e| fail(format!("open {path}: {e}")));
    let block = 1u64 << 20;
    let lens = [
        1usize, 2, 7, 100, 4095, 4096, 4097, 65535, 65536, 65537, 200_000,
    ];
    let mut offs = vec![
        0,
        1,
        block - 1,
        block - 3,
        block,
        2 * block - 4096,
        size - 1,
        size - 4096,
        size,
        size + 1,
        size + 10 * block,
    ];
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    for _ in 0..2000 {
        offs.push(xorshift(&mut x) % size);
    }
    for &off in &offs {
        for &len in &lens {
            check_at(&f, &whole, off, len, "explicit offset");
            small_reads += 1;
        }
    }

    // The handle's own position: sequential reads of a byte, then of odd
    // sizes, then seeks, must walk the file exactly as they would on disk.
    let mut g = std::fs::File::open(&path).unwrap_or_else(|e| fail(format!("open {path}: {e}")));
    let mut seq = Vec::with_capacity(whole.len());
    let mut one = [0u8; 1];
    for _ in 0..70_000 {
        match g.read(&mut one) {
            Ok(1) => seq.push(one[0]),
            other => fail(format!("1-byte read at {}: {other:?}", seq.len())),
        }
    }
    small_reads += 70_000;
    let mut buf = vec![0u8; 3001];
    loop {
        small_reads += 1;
        match g.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => seq.extend_from_slice(&buf[..n]),
            Err(e) => fail(format!("3001-byte read at {}: {e}", seq.len())),
        }
    }
    if seq != whole {
        fail(format!(
            "sequential small reads gave {} bytes that differ from one big read of {}",
            seq.len(),
            whole.len()
        ));
    }
    match g.read(&mut buf) {
        Ok(0) => {}
        other => fail(format!("a read at end of file gave {other:?}, not 0")),
    }
    for &(to, len) in &[
        (block - 2, 5usize),
        (17, 1),
        (size - 3, 10),
        (2 * block + 5, 4096),
    ] {
        g.seek(SeekFrom::Start(to)).unwrap();
        let mut b = vec![0u8; len];
        let n = g
            .read(&mut b)
            .unwrap_or_else(|e| fail(format!("read after seek to {to}: {e}")));
        let want = len.min((size - to) as usize);
        if n != want || b[..n] != whole[to as usize..to as usize + n] {
            fail(format!(
                "read {len} after seek to {to}: {n} bytes, wrong or short"
            ));
        }
        let pos = g.stream_position().unwrap();
        if pos != to + n as u64 {
            fail(format!("position after reading {n} at {to} is {pos}"));
        }
    }

    // Several threads, on one shared handle and on handles of their own.
    std::thread::scope(|s| {
        for t in 0..6u64 {
            let (f, whole, path) = (&f, &whole, &path);
            s.spawn(move || {
                let own = std::fs::File::open(path)
                    .unwrap_or_else(|e| fail(format!("thread {t}: open: {e}")));
                let mut x = 0x9E37_79B9_7F4A_7C15u64 ^ (t + 1);
                for i in 0..3000 {
                    let r = xorshift(&mut x);
                    let off = r % size;
                    let len = 1 + (r >> 40) as usize % 8192;
                    let h = if i % 2 == 0 { f } else { &own };
                    check_at(h, whole, off, len, "threaded");
                }
            });
        }
    });
    small_reads += 6 * 3000;

    // A file rewritten through another handle reads back fresh.
    if let Ok(rw) = std::env::var("VFS_FIXTURE_CACHE_RW_PATH") {
        let data = std::env::var("VFS_FIXTURE_CACHE_RW_DATA").unwrap_or_else(|_| "fresh".into());
        let before = read_small(&rw, 3);
        let held = std::fs::File::open(&rw).unwrap_or_else(|e| fail(format!("open {rw}: {e}")));
        let mut one = [0u8; 1];
        let _ = held.seek_read(&mut one, 0);
        if let Err(e) = std::fs::write(&rw, data.as_bytes()) {
            fail(format!("rewrite {rw}: {e}"));
        }
        for piece in [1usize, 3, 4096] {
            let after = read_small(&rw, piece);
            if after != data.as_bytes() {
                fail(format!(
                    "{rw} read in {piece}-byte pieces after a rewrite gave {:?}, not {data:?} \
                     (it held {:?} before)",
                    String::from_utf8_lossy(&after),
                    String::from_utf8_lossy(&before)
                ));
            }
        }
        drop(held);
        println!("FIXTURE CACHE RW OK: {rw} read back fresh");
    }

    println!("FIXTURE CACHE OK: {small_reads} small reads of {path} ({size} bytes) agreed");
    if let Some(ms) = std::env::var("VFS_FIXTURE_LINGER_MS")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        // Outlive a stats-report tick, so the host can read what the shim did.
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

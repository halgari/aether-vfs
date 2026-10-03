//! Injection read target: opens a (virtual) file via the normal Win32 path
//! (std::fs::read → CreateFileW → NtCreateFile, so the injected shim's hooks
//! intercept it), and asserts its length/content. Exit 0 iff it matches.
//! If `VFS_FIXTURE_WRITE_PATH` is set, after a successful read it also writes
//! `VFS_FIXTURE_WRITE_DATA` (default `written`) there, exiting 1 on error.
//!
//! If `VFS_FIXTURE_SLOW_PATH` is set, it then reads that file — each read one
//! `ReadFile` for the whole of it — on `VFS_FIXTURE_SLOW_THREADS` (default 1)
//! threads, while `VFS_FIXTURE_THREADS` (default 4) others each read
//! `VFS_FIXTURE_PATH` `VFS_FIXTURE_ROUNDS` (default 50) times, and exits 1
//! unless every one of those reads was right **and finished while the slow
//! reads were all still in flight**. The host makes the slow file slow; this
//! asserts that one thread's file operation does not make the others wait.
//!
//! Two more paths take the timing out of that, when the host serves them:
//! `VFS_FIXTURE_SLOW_STARTED` is waited for (until it exists) before the fast
//! threads start, in place of a fixed head start, and
//! `VFS_FIXTURE_SLOW_RELEASE` is looked up once they have all finished, which
//! is the host's cue to let the slow reads go.
//!
//! `VFS_FIXTURE_NAMES` and its companions run the names phase first: see
//! `names.rs`. Windows only — it calls Win32 directly.
//!
//! `VFS_FIXTURE_CACHE_PATH` and its companions run the read-cache phase after
//! the first read: see `cache.rs`. `VFS_FIXTURE_LINGER_MS` keeps the process
//! alive that long after it, so a shim stats report covers the run.
use std::process::exit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How long the fast threads give the slow read to get in flight, when the
/// host does not say when it is (`VFS_FIXTURE_SLOW_STARTED`).
const SLOW_HEAD_START: Duration = Duration::from_millis(300);
/// How long to wait for the host to say so before giving up.
const SLOW_START_PATIENCE: Duration = Duration::from_secs(60);

/// Read all of `path` with a single read call for its whole length, so the
/// shim sees one large `NtReadFile` (and pipelines it) rather than whatever
/// sizes `std::fs::read` chooses.
fn read_in_one_call(path: &str) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len() as usize;
    let mut buf = vec![0u8; len];
    let mut got = 0;
    while got < len {
        match f.read(&mut buf[got..])? {
            0 => break,
            n => got += n,
        }
    }
    buf.truncate(got);
    Ok(buf)
}

#[cfg(windows)]
mod cache;
#[cfg(windows)]
mod names;

fn env_num(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// See the module docs. Returns only if the phase passed.
fn concurrent_phase(path: &str, slow_path: &str, expect_len: usize, fill: Option<u8>) {
    let threads = env_num("VFS_FIXTURE_THREADS", 4);
    let rounds = env_num("VFS_FIXTURE_ROUNDS", 50);
    let slow_threads = env_num("VFS_FIXTURE_SLOW_THREADS", 1);
    let slow_done = AtomicBool::new(false);
    let started = Instant::now();
    let (fast_ms, slow_was_done) = std::thread::scope(|s| {
        let slow: Vec<_> = (0..slow_threads)
            .map(|_| {
                s.spawn(|| {
                    let r = read_in_one_call(slow_path);
                    slow_done.store(true, Ordering::SeqCst);
                    r
                })
            })
            .collect();
        match std::env::var("VFS_FIXTURE_SLOW_STARTED") {
            Ok(marker) => {
                while !std::path::Path::new(&marker).exists() {
                    if started.elapsed() > SLOW_START_PATIENCE {
                        eprintln!("FIXTURE FAIL: {marker} never appeared: the slow reads did not all start");
                        exit(1);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            Err(_) => std::thread::sleep(SLOW_HEAD_START),
        }
        let fast: Vec<_> = (0..threads)
            .map(|t| {
                s.spawn(move || {
                    for i in 0..rounds {
                        let data = match std::fs::read(path) {
                            Ok(d) => d,
                            Err(e) => {
                                eprintln!("FIXTURE FAIL: thread {t} read {i} of {path}: {e}");
                                exit(1);
                            }
                        };
                        if data.len() != expect_len
                            || fill.is_some_and(|b| data.iter().any(|&x| x != b))
                        {
                            eprintln!(
                                "FIXTURE FAIL: thread {t} read {i} of {path}: wrong bytes ({} of them)",
                                data.len()
                            );
                            exit(1);
                        }
                    }
                })
            })
            .collect();
        for h in fast {
            h.join().unwrap();
        }
        let fast_ms = started.elapsed().as_millis();
        // Sampled the moment the last fast read returned, before the slow
        // thread is joined: this is the whole assertion.
        let slow_was_done = slow_done.load(Ordering::SeqCst);
        if let Ok(release) = std::env::var("VFS_FIXTURE_SLOW_RELEASE") {
            // The lookup is the message; the path need not exist.
            let _ = std::fs::metadata(release);
        }
        for h in slow {
            match h.join().unwrap() {
                Ok(d) if !d.is_empty() => {}
                Ok(_) => {
                    eprintln!("FIXTURE FAIL: slow read of {slow_path} came back empty");
                    exit(1);
                }
                Err(e) => {
                    eprintln!("FIXTURE FAIL: slow read of {slow_path}: {e}");
                    exit(1);
                }
            }
        }
        (fast_ms, slow_was_done)
    });
    if slow_was_done {
        eprintln!(
            "FIXTURE FAIL: a slow read finished before the {} fast reads did ({fast_ms} ms): \
             they waited for it",
            threads * rounds
        );
        exit(1);
    }
    println!(
        "FIXTURE CONCURRENT OK: {} reads on {threads} threads done at {fast_ms} ms, slow read at {} ms",
        threads * rounds,
        started.elapsed().as_millis()
    );
}

fn main() {
    #[cfg(windows)]
    names::run();
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
    #[cfg(windows)]
    cache::run();
    if let Ok(wpath) = std::env::var("VFS_FIXTURE_WRITE_PATH") {
        let data = std::env::var("VFS_FIXTURE_WRITE_DATA").unwrap_or_else(|_| "written".into());
        if let Err(e) = std::fs::write(&wpath, data.as_bytes()) {
            eprintln!("FIXTURE FAIL: write {wpath}: {e}");
            exit(1);
        }
        println!("FIXTURE WROTE: {wpath}");
    }
    if let Ok(slow_path) = std::env::var("VFS_FIXTURE_SLOW_PATH") {
        concurrent_phase(&path, &slow_path, expect_len, fill);
    }
    println!("FIXTURE OK: {} bytes", data.len());
    exit(0);
}

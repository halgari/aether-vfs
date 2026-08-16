//! Gate 5, Task 8 — **a reproduction harness and a set of measurements, not a
//! passing test.** Both tests are `#[ignore]`d; nothing here runs in
//! `cargo test --workspace`.
//!
//! Task 8 asked for a fix to the `DllMain` shutdown stall, with Step 1 being
//! "get a reliable trigger, because a fix for an intermittent hang verified by
//! *it didn't hang this time* is not verified". This file is where that step
//! got to. **The fix was not delivered**: the trigger work produced evidence
//! that contradicts the mechanism the task was written around, and shipping a
//! fix against a mechanism the measurements do not support would be the exact
//! failure the instruction exists to prevent.
//!
//! Everything below was measured on this machine. Nothing is inferred.
//!
//! # 1. What the real zombies are actually doing
//!
//! Three genuine `vfs-fixture-escape.exe` zombies were alive and were sampled
//! directly (`GetThreadContext` on each, plus a stack read):
//!
//! - **one thread each** — so each is past `NtTerminateProcess(NULL, …)`,
//!   which kills every thread but the caller, and is inside
//!   `LdrShutdownProcess` walking `DLL_PROCESS_DETACH`;
//! - `RIP` **identical in all three**, at `ntdll!ZwWaitForAlertByThreadId+0x14`,
//!   reached from `ntdll!RtlWaitOnAddress+0x213`. That is a lock wait, not a
//!   spin — 0.047 s of CPU consumed in total;
//! - `RtlAllocateHeap` frames and a long run of `vfs_shim_dll` frames on the
//!   stack.
//!
//! So the *class* of failure in the task description is right: a thread was
//! killed holding a lock, and the survivor waits on it forever.
//!
//! # 2. Three shapes that do **not** reproduce it — 60 runs, 0 hangs
//!
//! The task's suggested mechanism is the reporter thread: it does nothing but
//! render (allocate) and write, forever, and was never stopped or joined, so
//! it is the likeliest thread to be inside the heap when it is killed. Turning
//! its duty cycle up to the maximum — `VFS_SHIM_STATS_INTERVAL_MS=0`, so it
//! never sleeps — did not reproduce anything:
//!
//! | shape | runs | hangs |
//! |---|---|---|
//! | reporter at interval 0, in a plain EXE | 15 | 0 |
//! | …plus `vfs_shim_dll.dll` mapped by `LoadLibraryW` | 15 | 0 |
//! | …plus hooks installed in-process and four threads hammering them | 20 | 0 |
//! | …plus `ExitProcess` directly, so CRT teardown runs at detach | 20 | 0 |
//!
//! The common miss is that in an EXE the CRT's teardown runs *before*
//! `ExitProcess`, while the whole point of the defect is teardown running
//! *after* the thread kill. Only a real DLL has that ordering.
//!
//! # 3. The shape that does hang — and why it is not yet proof
//!
//! [`child`] loads the real `vfs_shim_dll.dll`, lets its own `DllMain` →
//! bootstrap attach to a real ring (a `fakedirector` served from the parent),
//! install the hooks and start the reporter, then exits. That hangs
//! **14 of 20** runs at the default settings, and 3 of 3 even with the hook
//! hammering removed entirely.
//!
//! Its main thread wedges at `ZwWaitForAlertByThreadId+0x14` — *byte-identical
//! to the three real zombies*, with the same ~0.047 s CPU signature.
//!
//! **But it is not confirmed to be the same defect, and the difference is
//! recorded rather than glossed:**
//!
//! - the hung child still has **5 threads**, so it has *not* reached the
//!   thread kill, whereas the real zombies have; and
//! - it is **killable** — `child.kill()` reaps it, which a process wedged
//!   inside `LdrShutdownProcess` is not;
//! - its stack above the lock wait is `KERNELBASE!WaitOnAddress` and then
//!   *test-binary* frames, not `vfs_shim_dll` frames.
//!
//! Same instruction, different caller. It is a real hang and the closest lead
//! anyone has produced, which is why it is committed instead of discarded —
//! but calling it "the trigger" would be the same unearned claim this gate
//! keeps catching.
//!
//! # 4. The decisive negative result
//!
//! The suggested fix — make the reporter stoppable and join it before the
//! threads are killed — was **built and measured**, hung off a new
//! `NtTerminateProcess` detour (the one point where "every thread is alive and
//! the process is about to end" is knowable; `ZwTerminateProcess` shares its
//! RVA, so internal ntdll callers are covered too), with the reporter's sleep
//! replaced by a `Condvar` so the join returns promptly.
//!
//! It did not help: **16 of 20 still hung.** A breadcrumb written from the top
//! of that hook never appeared on any run, hung or clean — **the process
//! wedges before it ever reaches `NtTerminateProcess`.**
//!
//! That is the load-bearing finding. Any fix attached to process termination
//! is dead code for this failure, because the stall begins earlier, inside
//! `exit()`'s own teardown, while every thread is still running. The task's
//! suggested direction cannot work as written, and the production changes for
//! it were reverted rather than shipped unverified.
//!
//! # 5. Where the next session should start
//!
//! Un-`#[ignore]` [`a_process_carrying_the_shim_still_exits`], confirm it still
//! hangs, and then answer the one question this stopped at: **which Rust lock
//! is the main thread blocked on**, given the frames are in the test binary
//! rather than in the shim. Resolve the test binary's own frames against its
//! PDB — the addresses are stable within a build — rather than sampling
//! further. Until that name is known, no fix can be aimed.
//!
//! Run it as:
//!
//! ```text
//! cargo build -p vfs-shim-dll
//! cargo test -p vfs-shim --test exit_stall_repro -- --ignored --exact \
//!     a_process_carrying_the_shim_still_exits --nocapture --test-threads=1
//! ```
//!
//! **It leaves hung processes behind.** They are killable (see §3), but check
//! for and reap them afterwards, or the next `cargo build` fails `os error 5`
//! on a locked artifact.

mod fakedirector;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Set on the re-exec'd child. Absent in an ordinary run, so [`child`] is a
/// no-op then and only the parent below spawns anything.
///
/// Not `VFS_`-prefixed, and neither are its two siblings: `vfs-env`'s
/// `no_crate_reads_a_switch_that_is_not_registered` requires every `VFS_*`
/// name the workspace reads to be declared in `vfs_env::ALL`, and these three
/// are harness plumbing for one ignored test — registering them would put
/// them in `describe`'s user-facing switch list, which is worse than a prefix.
const CHILD_ENV: &str = "AETHER_TASK8_STALL_CHILD";

/// A healthy child does a few tens of milliseconds of work. Past this it is
/// hung, not slow.
const CHILD_TIMEOUT: Duration = Duration::from_secs(10);

/// Twenty gives the rate its significant figure. Raise it, do not lower it: at
/// the measured ~70% a handful of runs cannot tell a fix from a quiet spell.
const RUNS: usize = 20;

/// Threads doing continuous hooked file I/O. Not required to hang the child
/// (measured: 3 of 3 hang with this at zero), but kept because it raises the
/// rate and because a thread killed *inside* a shim mutex is the variant the
/// real zombies' `vfs_shim_dll` frames suggest.
const HAMMER_THREADS: usize = 4;

/// The child. A `#[test]` so libtest can select it by name; inert unless
/// [`CHILD_ENV`] is set, so running this file normally does not fork.
#[test]
#[ignore = "reproduction harness: the parent drives this; see the module docs"]
fn child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("AETHER_TASK8_ROOT").expect("root"));

    // The real shim DLL, mapped and bootstrapped. Deliberately *not*
    // `vfs_shim::install` in this image: the shim's teardown has to run at
    // `DLL_PROCESS_DETACH`, i.e. after the thread kill, and that ordering only
    // exists for a DLL. Three EXE-shaped variants of this file failed to
    // reproduce anything for exactly that reason (module docs §2).
    let dll: Vec<u16> = {
        use std::os::windows::ffi::OsStrExt;
        std::env::var_os("AETHER_TASK8_DLL")
            .expect("dll")
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    extern "system" {
        fn LoadLibraryW(p: *const u16) -> *mut core::ffi::c_void;
    }
    assert!(!unsafe { LoadLibraryW(dll.as_ptr()) }.is_null(), "LoadLibraryW failed");

    // `DllMain` spawns bootstrap on its own thread and signals through the
    // ready file. Waiting for it is what makes "the hooks were live" a fact
    // rather than a hope — a run that hung with a failed bootstrap would prove
    // nothing at all.
    let ready = std::path::PathBuf::from(std::env::var_os("VFS_SHIM_READY").expect("ready"));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        assert!(std::time::Instant::now() < deadline, "shim never signalled ready");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        std::fs::read_to_string(&ready).unwrap_or_default(),
        "ok",
        "the shim bootstrapped into a failure state, so this run proves nothing"
    );

    let stop = Arc::new(AtomicBool::new(false));
    for t in 0..HAMMER_THREADS {
        let stop = Arc::clone(&stop);
        let f = root.join(format!("hammer-{t}.bin"));
        let root = root.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                // Each of these enters `create_hook`/`close_hook`, which take
                // `HANDLE_PATHS`, `PATH_TABLE` and `DIR_TABLE`.
                let _ = std::fs::read(&f);
                let _ = std::fs::metadata(&f);
                let _ = std::fs::read_dir(&root).map(|d| d.count());
            }
        });
    }

    // Let everything reach steady state before the exit lands. Without this the
    // child sometimes exits before any thread has taken a lock, which is the
    // absence of the race rather than a pass.
    std::thread::sleep(Duration::from_millis(150));

    std::process::exit(0);
}

/// Locate the cdylib, loudly.
///
/// `cargo build --all-targets` does **not** build it — `--tests` filters the
/// cdylib exactly as `--bin` does, printing `Compiling vfs-shim-dll` and
/// exiting 0 while the artifact stays stale. A silently stale DLL here would
/// mean measuring the previous build's behaviour and believing it.
fn dll_path() -> std::path::PathBuf {
    let mut d = std::env::current_exe().unwrap();
    d.pop();
    if d.ends_with("deps") {
        d.pop();
    }
    let p = d.join("vfs_shim_dll.dll");
    assert!(p.exists(), "{} is missing — run `cargo build -p vfs-shim-dll`", p.display());
    p
}

/// Spawn the child [`RUNS`] times and report how many never exited.
#[test]
#[ignore = "hangs on purpose and leaves processes behind; see the module docs"]
fn a_process_carrying_the_shim_still_exits() {
    let exe = std::env::current_exe().expect("current exe");
    let dll = dll_path();
    let base = std::env::temp_dir().join(format!("vfs-task8-stall-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let vroot = base.join("root");
    std::fs::create_dir_all(&vroot).unwrap();

    // A real ring in this process, which every child attaches to exactly as an
    // injected game does. `install` publishes the section name and geometry
    // into this process's environment, and the children inherit it.
    fakedirector::install(&vroot, fakedirector::Fake::new().with_dir("."), 0);

    let cfg = base.join("shim.cfg");
    std::fs::write(
        &cfg,
        vfs_shim::encode_config_with_overlay(
            vroot.to_str().unwrap(),
            base.join("overlay").to_str().unwrap(),
            &[],
        ),
    )
    .unwrap();

    let mut stalled = Vec::new();
    for i in 0..RUNS {
        let dir = base.join(format!("run-{i}"));
        std::fs::create_dir_all(&dir).unwrap();
        for t in 0..HAMMER_THREADS {
            std::fs::write(dir.join(format!("hammer-{t}.bin")), b"x").unwrap();
        }
        let mut child = std::process::Command::new(&exe)
            .args(["--exact", "child", "--ignored", "--nocapture"])
            .env(CHILD_ENV, "1")
            .env("AETHER_TASK8_ROOT", &dir)
            .env("AETHER_TASK8_DLL", &dll)
            .env(vfs_env::SHIM_CONFIG, &cfg)
            .env(vfs_env::SHIM_READY, dir.join("ready"))
            .env(vfs_env::SHIM_STATS_LOG, dir.join("stats.txt"))
            // Zero interval: the reporter never sleeps, so it is nearly always
            // inside the heap when the exit reaches it.
            .env(vfs_env::SHIM_STATS_INTERVAL_MS, "0")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn child");

        let deadline = std::time::Instant::now() + CHILD_TIMEOUT;
        let exited = loop {
            match child.try_wait().expect("try_wait") {
                Some(_) => break true,
                None if std::time::Instant::now() >= deadline => break false,
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        };
        if !exited {
            stalled.push(i);
            let _ = child.kill();
            let _ = child.wait();
        }
        eprintln!("run {i}: exited={exited}");
    }

    let _ = std::fs::remove_dir_all(&base);
    assert!(
        stalled.is_empty(),
        "{} of {RUNS} children never exited (runs {stalled:?}). Baseline for this harness at \
         the time it was written was 14 of 20 — a lower number is not a fix unless it is zero, \
         and a zero needs the caller of the blocking lock named first (module docs §5).",
        stalled.len()
    );
}

//! **The Proton path end to end**: the public API — `Session::serve()`
//! then `Session::launch()` — starts a real Windows executable under
//! GE-Proton on Linux, with the shim injected, and the child reads a file that
//! exists **only** inside this native Linux Director's provider.
//!
//! Nothing here is a harness. The test builds a `Session`, mounts one
//! disk-backed provider over a directory the Wine process cannot name, serves
//! the file-backed ring, and launches `vfs-fixture-read.exe` — which opens its
//! path through `std::fs::read` → `CreateFileW` → `NtCreateFile`, i.e. through
//! the shim's hooks — and exits 0 only if the bytes and the length match.
//!
//! Two independent witnesses, and both are asserted, because either one alone
//! is weak:
//!
//! * the child's **exit code** is 0, which is the fixture asserting the
//!   content it read;
//! * the **provider's own call log** contains an `open` and a `read_at` for
//!   `data/hello.txt`, which is the Director asserting that the bytes came
//!   from it. A fixture that somehow found the file on a real filesystem would
//!   still exit 0, and a provider log with no `read_at` would mean the ring
//!   answered from somewhere else.
//!
//! The provider log is also the "Director side" output of a run: this Director
//! is *in process*, so `--nocapture` is the only way to see it.
//!
//! ## Why the fixture's path is a hard-coded `C:\` string
//!
//! `Session::launch` links the session's root and state directory
//! into `<prefix>/drive_c/vfs-session/{root,state}` — a Wine process
//! can only name what is under one of its drives, and those live
//! wherever the host put them. So the managed root is always
//! `C:\vfs-session\root` inside the child, whatever the host path is, and the
//! virtual file is at `C:\vfs-session\root\data\hello.txt`. That is a private
//! constant of `Session` (`WINE_LINK_DIR`), not public API; there is no
//! accessor for it. If the two ever drift, this test fails with the fixture
//! reporting the path it could not read, which names the drift.
#![cfg(unix)]

mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use support::Loud;
use vfs_embed::{
    Capabilities, DirEntry, DiskProvider, Handle, LaunchOpts, Provider, Session, SetAttr, Stat,
    VPath,
};

/// The one file that exists only in the provider, as the child names it.
/// See the module docs for why this is a literal.
const CHILD_PATH: &str = r"C:\vfs-session\root\data\hello.txt";
/// Its vpath in root 0's graph — what the shim asks the ring for once it has
/// folded [`CHILD_PATH`] against `VFS_VIRTUAL_DIR`.
const VPATH: &str = "data/hello.txt";
/// Content: one page of a single non-zero byte, so a short read, a zero-filled
/// buffer and a wrong-file read are each distinguishable by the fixture's own
/// length + fill checks. Small enough to stay inline in the ring (the payload
/// cap is 1 MiB), which is the path a first end-to-end run should exercise.
const FILL: u8 = 0x5A;
const LEN: usize = 4096;

/// One Wine launch at a time: each test boots its own prefix, and two booting
/// at once on one machine is slower than either alone and proves nothing more.
static ONE_LAUNCH: Mutex<()> = Mutex::new(());

fn tmp(name: &str) -> PathBuf {
    support::scratch("vfs-proton-launch", name)
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

/// **The whole path.** `Session::serve()` + `Session::launch()` start a
/// Windows executable under GE-Proton, the injected shim routes its
/// `NtCreateFile`/`NtReadFile` back over a file-backed ring to this native
/// Linux Director, and the file it reads exists on no filesystem the Wine
/// process can see.
///
/// Requires, and cannot provide for itself:
/// * a verified **GE-Proton runtime** (install with `vfs-proton install`;
///   `support::runtime_dir` says where it is looked for);
/// * a **Wine prefix**, which `Session::launch` boots itself under the test's
///   throwaway home, `sessions/<id>/prefix` — so a 32-bit runtime must be installed
///   (`lib32-glibc`, `lib32-gcc-libs` on Arch), since `wine`'s launcher probes
///   for the 32-bit loader even under `WINEARCH=win64`;
/// * the four **Windows artifacts** (`support::windows_artifacts`), from
///   `bin/build-windows` for this test's profile.
///
/// A missing prerequisite prints `SKIP ...` with the command to run and passes
/// (`tests/support/mod.rs`); the launch runs in a throwaway aether home under
/// `target/tmp`, never the user's.
#[test]
#[ignore = "needs a GE-Proton runtime, a bootable Wine prefix, and Windows-built artifacts \
            (vfs-injector.exe, vfs_shim_dll.dll, vfs_payload.dll, vfs-fixture-read.exe) for \
            this profile — see bin/build-windows"]
fn session_launches_a_windows_fixture_under_proton_that_reads_from_the_provider() {
    launch_fixture(
        "proton_launch::session_launches_a_windows_fixture_under_proton_that_reads_from_the_provider",
        BTreeMap::new(),
    );
}

/// **A child of the launched process is virtualised too.** The same launch, with
/// the fixture asked (`VFS_FIXTURE_SPAWN_CHILD`) to run a second copy of itself
/// after its own read. That copy is created through the shim's
/// `CreateProcessInternalW` hook, so it is injected like the launch was; it
/// reads the same provider-only file and exits non-zero if it cannot. The
/// fixture exits non-zero if the child does, so a child that ran without the
/// shim (or a spawn that the hook refused) fails the launch.
///
/// This is the success half of the shim's fail-closed child rule: the failure
/// half (a child that cannot be injected is killed and its `CreateProcess`
/// fails) is `vfs-shim`'s `child_inject_fails_closed`.
#[test]
#[ignore = "needs a GE-Proton runtime, a bootable Wine prefix, and Windows-built artifacts \
            (vfs-injector.exe, vfs_shim_dll.dll, vfs_payload.dll, vfs-fixture-read.exe) for \
            this profile — see bin/build-windows"]
fn a_child_the_fixture_spawns_is_virtualised_too_under_proton() {
    let mut env = BTreeMap::new();
    env.insert("VFS_FIXTURE_SPAWN_CHILD".to_string(), "1".to_string());
    launch_fixture(
        "proton_launch::a_child_the_fixture_spawns_is_virtualised_too_under_proton",
        env,
    );
}

/// Spike: the same launch with the fixture's import table patched to load the
/// shim first, started without `CREATE_SUSPENDED` or any injection.
#[test]
#[ignore = "spike: import-table activation under Proton"]
fn spike_import_activated_fixture_reads_from_the_provider() {
    let mut env = BTreeMap::new();
    env.insert("VFS_ACTIVATION".to_string(), "import".to_string());
    launch_fixture("proton_launch::spike_import_activated", env);
}

/// Spike: an import-activated fixture spawns a child, which the shim's
/// existing child hook injects the classic way.
#[test]
#[ignore = "spike: import-table activation under Proton"]
fn spike_import_activated_missing_shim_refuses_to_start() {
    let mut env = BTreeMap::new();
    env.insert("VFS_ACTIVATION".to_string(), "import".to_string());
    if std::env::var_os("SPIKE_FUSE_FAIL").is_some() {
        env.insert("VFS_TEST_FUSE_INIT_FAIL".to_string(), "1".to_string());
    } else {
        env.insert("SPIKE_NO_SHIM".to_string(), "1".to_string());
    }
    launch_fixture("proton_launch::spike_import_missing_shim", env);
}

#[test]
#[ignore = "spike"]
fn spike_import_activated_fixture_child_is_virtualised() {
    let mut env = BTreeMap::new();
    env.insert("VFS_ACTIVATION".to_string(), "import".to_string());
    env.insert("VFS_FIXTURE_SPAWN_CHILD".to_string(), "1".to_string());
    launch_fixture("proton_launch::spike_import_activated_child", env);
}

/// The body of both launches above: serve [`VPATH`] from a provider, launch the
/// fixture with `extra_env` on top of the read it always does, and check it
/// exited 0 having read every byte through the ring.
fn launch_fixture(test_name: &str, extra_env: BTreeMap<String, String>) {
    let _one = ONE_LAUNCH.lock().unwrap_or_else(|e| e.into_inner());
    let Some(rig) = support::rig(test_name, "launch", &[vfs_proton::artifacts::FIXTURE_READ])
    else {
        return;
    };
    let art = &rig.art;

    let root = tmp("root");
    let state = tmp("state");
    let overlay = tmp("overlay");
    // The bytes live here, and this directory is under no Wine drive: it is
    // not the managed root, not in the prefix, and nothing links it in. The
    // only way the child can see it is through the ring.
    let content = tmp("content");
    std::fs::create_dir_all(content.join("data")).unwrap();
    std::fs::write(content.join("data").join("hello.txt"), [FILL; LEN]).unwrap();

    // The image is a **real file** under the managed root, not staged out of
    // the graph (`launch` stages a graph-only image on this path too): this
    // test is about the *data* going over the ring, so the fixture is copied in
    // and the data is what stays virtual.
    let image = root.join("fixture.exe");
    std::fs::copy(art.path(vfs_proton::artifacts::FIXTURE_READ), &image)
        .expect("copy the fixture into the root");
    // Import activation (spike): the exe imports the shim first, and the shim
    // sits beside it under the name the import asks for.
    if extra_env.get("VFS_ACTIVATION").map(String::as_str) == Some("import") {
        let raw = std::fs::read(&image).unwrap();
        let mut patched =
            vfs_pe::add_first_import(&raw, "vfs_shim_dll.dll", "vfs_shim_sync_bootstrap")
                .expect("patch the fixture's imports");
        vfs_pe::raise_stack_reserve(&mut patched, 16 * 1024 * 1024).unwrap();
        std::fs::write(&image, patched).unwrap();
        if extra_env.get("SPIKE_NO_SHIM").is_none() {
            std::fs::copy(art.shim_dll(), root.join("vfs_shim_dll.dll")).unwrap();
        }
    }

    let provider = Arc::new(Loud::new(&content));

    let mut s = Session::new();
    s.set_home(&rig.home);
    s.set_root(&root);
    s.set_state_dir(&state);
    s.set_overlay(&overlay);
    s.mount("", Arc::clone(&provider) as Arc<dyn Provider>)
        .expect("mount the disk-backed provider over root 0");
    s.serve().expect("serve");

    let ipc = s.ipc().expect("serve() must leave a live ring");
    eprintln!(
        "DIRECTOR: ring {} map_bytes={} arena_offset={} arena_len={} payload_cap={}",
        ipc.ring_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        ipc.map_bytes,
        ipc.arena_offset,
        ipc.arena_len,
        ipc.payload_cap
    );

    // Read it back through the graph first. If this fails, the launch was
    // never going to work and the diagnosis is on this side of the ring.
    assert_eq!(
        s.read_file(VPATH)
            .expect("the provider must serve the vpath")
            .len(),
        LEN,
        "the host-side read through the same graph the child will use"
    );
    assert!(
        !root.join("data").join("hello.txt").exists(),
        "the file must exist only in the provider — a copy under the managed root \
         would make the child's read prove nothing"
    );

    let mut env = extra_env;
    env.insert("VFS_FIXTURE_PATH".to_string(), CHILD_PATH.to_string());
    env.insert("VFS_FIXTURE_EXPECT".to_string(), LEN.to_string());
    env.insert("VFS_FIXTURE_FILL".to_string(), FILL.to_string());

    let code = s
        .launch(&LaunchOpts {
            image: "fixture.exe".into(),
            wait: true,
            // `vfs-injector.exe` is taken from the directory holding `shim_dll`,
            // which is why setting this one path is enough for all three.
            shim_dll: Some(art.shim_dll()),
            payload_dll: Some(art.payload_dll()),
            env,
            ..Default::default()
        })
        .unwrap_or_else(|e| panic!("launch: {e}\nDIRECTOR saw: {:?}", provider.transcript()));

    let seen = provider.transcript();
    assert_eq!(
        code, 0,
        "the fixture exits 0 only if it read {LEN} bytes of {FILL:#04x} from {CHILD_PATH}. \
         DIRECTOR saw: {seen:?}"
    );
    assert!(
        provider.saw("open", VPATH),
        "the child's open must have reached this provider — otherwise the ring answered \
         from somewhere else. DIRECTOR saw: {seen:?}"
    );
    assert!(
        provider.saw("read_at", VPATH),
        "the child's bytes must have come from this provider. DIRECTOR saw: {seen:?}"
    );

    s.stop_serve();
    for d in [&root, &state, &overlay, &content] {
        let _ = std::fs::remove_dir_all(d);
    }
}

// ---------------------------------------------------------------------------
// Concurrent file operations across the Wine boundary
// ---------------------------------------------------------------------------

/// The file the fast threads read, as the child names it. Two bulk chunks
/// and an inline tail, so every read is a pipeline through the arena.
const BIG_CHILD_PATH: &str = r"C:\vfs-session\root\data\big.bin";
const BIG_LEN: usize = 2 * 1024 * 1024 + 4096;
/// The slow file, as the child names it and as the graph does. Eight 1 MiB
/// chunks: a read of it is the deepest pipeline the shim runs.
const SLOW_CHILD_PATH: &str = r"C:\vfs-session\root\data\slow.bin";
const SLOW_VPATH: &str = "data/slow.bin";
const SLOW_LEN: usize = 8 * 1024 * 1024;
/// Appears, in the provider's directory, once the slow reads are all inside
/// the provider; the fixture waits for it before it starts its fast threads.
const STARTED_CHILD_PATH: &str = r"C:\vfs-session\root\data\slow.started";
const STARTED_VPATH: &str = "data/slow.started";
/// Looked up by the fixture once its fast threads have finished: the cue to
/// let the slow reads go. It never exists.
const RELEASE_CHILD_PATH: &str = r"C:\vfs-session\root\data\release.slow";
const RELEASE_VPATH: &str = "data/release.slow";
/// Two threads read the slow file at once.
const SLOW_THREADS: usize = 2;
/// Sixteen workers give the shim twelve data permits. One read may hold six
/// and may not take the last three beyond its first, so two eight-chunk
/// reads have 6 + 3 requests in flight — not 8 + 4, which is every permit.
const IO_WORKERS: usize = 16;
const SLOW_IN_FLIGHT: usize = 9;
/// How long the provider holds the slow reads if the fixture never gives the
/// cue. Only a failing run waits this long.
const SLOW_PATIENCE: Duration = Duration::from_secs(40);
const FAST_THREADS: usize = 4;
const FAST_ROUNDS: usize = 50;

/// A [`DiskProvider`] whose reads of [`SLOW_VPATH`] block until the fixture
/// looks [`RELEASE_VPATH`] up, and which counts what it serves meanwhile.
struct Stalling {
    disk: DiskProvider,
    /// Where [`STARTED_VPATH`] is created: the provider's own directory.
    content: PathBuf,
    slow_handles: Mutex<Vec<Handle>>,
    released: Mutex<bool>,
    release: std::sync::Condvar,
    /// Slow reads blocked in here now, and the most there ever were.
    slow_held: AtomicUsize,
    slow_peak: AtomicUsize,
    /// Reads of anything else that arrived while slow reads were blocked.
    reads_during_slow: AtomicUsize,
    cue_seen: AtomicBool,
}

impl Stalling {
    /// Let the slow reads go if `p` is the path the fixture looks up to say
    /// so. A lookup reaches a provider as a stat or as an open, depending on
    /// how the caller's runtime asks.
    fn cue(&self, p: VPath) {
        if p.rel.eq_ignore_ascii_case(RELEASE_VPATH) {
            self.cue_seen.store(true, Ordering::SeqCst);
            *self.released.lock().unwrap() = true;
            self.release.notify_all();
        }
    }
}

impl Provider for Stalling {
    fn capabilities(&self) -> Capabilities {
        self.disk.capabilities()
    }
    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        self.cue(p);
        self.disk.getattr(p)
    }
    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        self.disk.readdir(p)
    }
    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        self.cue(p);
        let r = self.disk.open(p, flags)?;
        if p.rel.eq_ignore_ascii_case(SLOW_VPATH) {
            self.slow_handles.lock().unwrap().push(r.0);
        }
        Ok(r)
    }
    fn close(&self, h: Handle) -> Result<(), i32> {
        self.slow_handles.lock().unwrap().retain(|&s| s != h);
        self.disk.close(h)
    }
    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let slow = self.slow_handles.lock().unwrap().contains(&h);
        if slow {
            let held = self.slow_held.fetch_add(1, Ordering::SeqCst) + 1;
            self.slow_peak.fetch_max(held, Ordering::SeqCst);
            if held == SLOW_IN_FLIGHT {
                let _ = std::fs::write(self.content.join(STARTED_VPATH), b"started");
            }
            let released = self.released.lock().unwrap();
            let _ = self
                .release
                .wait_timeout_while(released, SLOW_PATIENCE, |released| !*released)
                .unwrap();
            self.slow_held.fetch_sub(1, Ordering::SeqCst);
        } else if self.slow_held.load(Ordering::SeqCst) > 0 {
            self.reads_during_slow.fetch_add(1, Ordering::SeqCst);
        }
        self.disk.read_at(h, offset, buf)
    }
    fn set_attr(&self, p: VPath, attr: SetAttr) -> Result<(), i32> {
        self.disk.set_attr(p, attr)
    }
}

/// **Stalled reads on two threads do not freeze the others**, across the real
/// boundary: the shipped shim inside Wine, this native Director, the
/// file-backed ring between them.
///
/// Two fixture threads each read 8 MiB of a file whose reads block in the
/// provider. Once those are all inside it, four other threads each read a
/// 2 MiB file fifty times — pipelined, through the arena. Only when they
/// have finished does the fixture give the cue that lets the slow reads go.
/// Nothing here is timed: the slow reads are held until the fast ones are
/// done, however long that takes.
///
/// Witnesses:
///
/// * the **fixture** exits 0 only if all two hundred reads were right and had
///   returned while both slow reads were still in flight;
/// * the **provider** saw those reads arrive while it was holding the slow
///   ones, saw the fixture's cue, and never held more than nine slow requests.
///
/// What it fails on:
///
/// * One process-wide lock around every round trip, which the shim had: the
///   second slow read never starts, so the fixture never sees the start
///   marker.
/// * A gate that lets two deep reads take every permit (8 + 4 of 12): the
///   provider holds twelve slow requests, not nine, and the fast reads wait
///   at the gate until the provider's patience runs out.
///
/// It also runs the pieces a native test cannot: the shim's thread-local slot
/// hint, its permit gate with callers waiting in line (four fast threads on
/// three free permits), and its yield-then-sleep wait, in an injected DLL on
/// Wine threads.
#[test]
#[ignore = "needs a GE-Proton runtime, a bootable Wine prefix, and Windows-built artifacts \
            for this profile — see bin/build-windows"]
fn stalled_reads_on_two_threads_do_not_hold_up_file_operations_on_others_under_proton() {
    let _one = ONE_LAUNCH.lock().unwrap_or_else(|e| e.into_inner());
    let Some(rig) = support::rig("proton_launch::stalled_reads_on_two_threads_do_not_hold_up_file_operations_on_others_under_proton", "launch", &[vfs_proton::artifacts::FIXTURE_READ])
    else {
        return;
    };
    let art = &rig.art;

    let root = tmp("c-root");
    let state = tmp("c-state");
    let overlay = tmp("c-overlay");
    let content = tmp("c-content");
    std::fs::create_dir_all(content.join("data")).unwrap();
    std::fs::write(content.join("data").join("big.bin"), vec![FILL; BIG_LEN]).unwrap();
    std::fs::write(
        content.join("data").join("slow.bin"),
        vec![0xA5u8; SLOW_LEN],
    )
    .unwrap();
    let image = root.join("fixture.exe");
    std::fs::copy(art.path(vfs_proton::artifacts::FIXTURE_READ), &image)
        .expect("copy the fixture into the root");

    let provider = Arc::new(Stalling {
        disk: DiskProvider::new(&content),
        content: content.clone(),
        slow_handles: Mutex::new(Vec::new()),
        released: Mutex::new(false),
        release: std::sync::Condvar::new(),
        slow_held: AtomicUsize::new(0),
        slow_peak: AtomicUsize::new(0),
        reads_during_slow: AtomicUsize::new(0),
        cue_seen: AtomicBool::new(false),
    });

    let mut s = Session::new();
    s.set_home(&rig.home);
    s.set_root(&root);
    s.set_state_dir(&state);
    s.set_overlay(&overlay);
    s.set_io_workers(IO_WORKERS);
    s.mount("", Arc::clone(&provider) as Arc<dyn Provider>)
        .expect("mount the provider over root 0");
    s.serve().expect("serve");

    let mut env = BTreeMap::new();
    for (name, value) in [
        ("VFS_FIXTURE_PATH", BIG_CHILD_PATH.to_string()),
        ("VFS_FIXTURE_EXPECT", BIG_LEN.to_string()),
        ("VFS_FIXTURE_FILL", FILL.to_string()),
        ("VFS_FIXTURE_SLOW_PATH", SLOW_CHILD_PATH.to_string()),
        ("VFS_FIXTURE_SLOW_THREADS", SLOW_THREADS.to_string()),
        ("VFS_FIXTURE_SLOW_STARTED", STARTED_CHILD_PATH.to_string()),
        ("VFS_FIXTURE_SLOW_RELEASE", RELEASE_CHILD_PATH.to_string()),
        ("VFS_FIXTURE_THREADS", FAST_THREADS.to_string()),
        ("VFS_FIXTURE_ROUNDS", FAST_ROUNDS.to_string()),
    ] {
        env.insert(name.to_string(), value);
    }

    let code = s
        .launch(&LaunchOpts {
            image: "fixture.exe".into(),
            wait: true,
            shim_dll: Some(art.shim_dll()),
            payload_dll: Some(art.payload_dll()),
            env,
            ..Default::default()
        })
        .unwrap_or_else(|e| panic!("launch: {e}"));

    let peak = provider.slow_peak.load(Ordering::SeqCst);
    let during = provider.reads_during_slow.load(Ordering::SeqCst);
    let cue = provider.cue_seen.load(Ordering::SeqCst);
    eprintln!(
        "DIRECTOR: at most {peak} slow requests held; {during} other reads served meanwhile; \
         release cue seen: {cue}"
    );
    assert_eq!(
        code,
        0,
        "the fixture exits 0 only if its {} fast reads all returned, with the right bytes, \
         while its slow reads were still in flight. The Director held at most {peak} slow \
         requests and served {during} reads meanwhile.",
        FAST_THREADS * FAST_ROUNDS
    );
    assert!(
        cue,
        "the slow reads were let go by the provider's patience, not by the fixture's cue"
    );
    assert_eq!(
        peak, SLOW_IN_FLIGHT,
        "two eight-chunk reads must have 6 + 3 requests with the Director at once"
    );
    assert!(
        during >= FAST_THREADS * FAST_ROUNDS,
        "only {during} reads reached the Director while the slow reads were held, of {} \
         fast reads of several chunks each: the shim made them wait",
        FAST_THREADS * FAST_ROUNDS
    );

    s.stop_serve();
    for d in [&root, &state, &overlay, &content] {
        let _ = std::fs::remove_dir_all(d);
    }
}

// ---------------------------------------------------------------------------
// Final paths of virtual handles
// ---------------------------------------------------------------------------

/// Where root 0 is in the child for the names test: a location with capitals
/// in it, as a host's is, so "spelled as stored" is a claim about the root's
/// own path too.
const NAMES_ROOT: &str = r"C:\Haskill\TestList\game";

/// **A virtual directory has a final path, and it is the prefix of the final
/// path of every file under it.**
///
/// `std::filesystem::canonical` is `CreateFileW` with
/// `FILE_FLAG_BACKUP_SEMANTICS` then `GetFinalPathNameByHandleW`. On a
/// virtual handle that used to fail outright — the shim answered no name
/// query for a handle the director serves — so a plugin that checks a file is
/// inside its own directory by comparing canonical paths (Community Shaders,
/// for its fonts) rejected every one of them as a path traversal.
///
/// The fixture asks, for four kinds of directory and for files under them —
/// the root itself, a directory two providers both have, one only one
/// provider has, and one that exists only on the real disk under the root —
///
/// * `GetFinalPathNameByHandleW` in every volume-name and file-name form;
/// * `canonicalize`, `GetFileAttributesW`, `metadata`,
///   `GetFileInformationByHandle` and `…Ex` (attribute tag, name, id);
/// * all of it again with the path in the opposite letter case, which must
///   give the same final path and the same file id;
/// * that `canonical(dir)` is a byte prefix of `canonical(dir/file)`, with
///   the directory given in another case than the file;
/// * that a listing of a merged directory has both providers' entries.
///
/// The expected paths are spelled as the providers store them, which is not
/// how the fixture is told to open them.
#[test]
#[ignore = "needs a GE-Proton runtime, a bootable Wine prefix, and Windows-built artifacts \
            for this profile — see bin/build-windows"]
fn a_virtual_directory_has_a_final_path_that_prefixes_its_files_under_proton() {
    let _one = ONE_LAUNCH.lock().unwrap_or_else(|e| e.into_inner());
    let Some(rig) = support::rig(
        "proton_launch::a_virtual_directory_has_a_final_path_that_prefixes_its_files_under_proton",
        "launch",
        &[vfs_proton::artifacts::FIXTURE_READ],
    ) else {
        return;
    };
    let art = &rig.art;

    let root = tmp("n-root");
    let state = tmp("n-state");
    let overlay = tmp("n-overlay");
    let upper = tmp("n-upper");
    let lower = tmp("n-lower");
    let write = |base: &Path, rel: &str, bytes: &[u8]| {
        let p = base.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    };
    // One provider has the font, the other a second file in the same
    // directory and a directory of its own; the real root has a third.
    write(
        &upper,
        "Data/Interface/CommunityShaders/Fonts/Jost/Jost-Regular.ttf",
        b"font",
    );
    write(&upper, "Data/hello.txt", &[FILL; LEN]);
    write(
        &lower,
        "Data/Interface/CommunityShaders/Fonts/Other.ttf",
        b"other",
    );
    write(&lower, "Data/OnlyInLower/b.txt", b"b");
    write(&root, "RealOnly/r.txt", b"r");
    let image = root.join("fixture.exe");
    std::fs::copy(art.path(vfs_proton::artifacts::FIXTURE_READ), &image)
        .expect("copy the fixture into the root");

    // A write layer like a host's: what the fixture creates lands here, and
    // the providers below stay read-only.
    let storage_dir = tmp("n-storage");
    let storage = vfs_embed::Storage::open(&storage_dir, vfs_embed::StorageConfig::default())
        .expect("open storage");

    let mut s = Session::new();
    s.set_home(&rig.home);
    s.set_root(&root);
    s.declare_root(0, NAMES_ROOT);
    s.set_state_dir(&state);
    s.set_overlay(&overlay);
    for dir in [&upper, &lower, &root] {
        s.mount("", Arc::new(DiskProvider::new(dir)) as Arc<dyn Provider>)
            .expect("mount a provider over root 0");
    }
    s.set_write_layer(storage.layer("write").expect("a write layer"))
        .expect("set the write layer");
    s.serve().expect("serve");

    let at = |rel: &str| format!(r"{NAMES_ROOT}\{rel}");
    let fonts = at(r"Data\Interface\CommunityShaders\Fonts");
    let font = at(r"Data\Interface\CommunityShaders\Fonts\Jost\Jost-Regular.ttf");
    // kind | how the fixture opens it | its final path. The fixture also
    // opens each in the opposite letter case of its own accord.
    let names = [
        ("d", NAMES_ROOT.to_string(), NAMES_ROOT.to_string()),
        ("d", at("Data"), at("Data")),
        ("d", fonts.clone(), fonts.clone()),
        (
            "d",
            at(r"DATA\interface\COMMUNITYSHADERS\fonts\"),
            fonts.clone(),
        ),
        ("d", at(r"Data\OnlyInLower"), at(r"Data\OnlyInLower")),
        ("d", at("RealOnly"), at("RealOnly")),
        ("f", font.clone(), font.clone()),
        (
            "f",
            at(r"data\INTERFACE\communityshaders\FONTS\other.TTF"),
            at(r"Data\Interface\CommunityShaders\Fonts\Other.ttf"),
        ),
        (
            "f",
            at(r"Data\OnlyInLower\b.txt"),
            at(r"Data\OnlyInLower\b.txt"),
        ),
        ("f", at(r"realonly\R.TXT"), at(r"RealOnly\r.txt")),
    ];
    let prefixes = [
        (fonts.clone(), font.clone()),
        (fonts.to_uppercase(), font.to_lowercase()),
        (NAMES_ROOT.to_lowercase(), at(r"Data\hello.txt")),
        (at("realonly"), at(r"RealOnly\r.txt")),
        // A directory named before a write under it, a file named after.
        (
            at("DATA"),
            at(r"data\interface\communityshaders\fonts\new font.ttf"),
        ),
        (fonts.to_lowercase(), font.clone()),
    ];
    // Written by the fixture after it has named the prefix directories:
    // under directories the providers have, spelled in lower case the way
    // nothing on disk is (that is how `Data` became `data`); a directory and
    // a file of the game's own; a long save name; and a rename of one to
    // another letter case and of another to a new name.
    const SAVE: &str = "Save12_ABCDEF01_0_4E6F726420486572_Tamriel_000123_20261002150000_1_1.ess";
    let creates = [
        at(r"data\interface\communityshaders\fonts\New Font.TTF"),
        at(r"data\SKSE\"),
        at(r"Data\SKSE\CommunityShaders.log"),
        at(r"Saves\"),
        at(&format!(r"Saves\{SAVE}")),
        at(r"Saves\quicksave.ESS"),
        at(r"Saves\Old Name.ess"),
    ];
    let renames = [
        (at(r"Saves\quicksave.ESS"), at(r"Saves\QuickSave.ess")),
        (at(r"Saves\Old Name.ess"), at(r"Saves\New Name.ESS")),
    ];
    let lists = [
        (fonts.clone(), "Jost,Other.ttf"),
        (NAMES_ROOT.to_string(), "Data,RealOnly,fixture.exe"),
        (at("DATA"), "Interface,OnlyInLower,hello.txt,SKSE"),
        (NAMES_ROOT.to_string(), "Data,RealOnly,fixture.exe,Saves"),
    ];

    let mut env = BTreeMap::new();
    env.insert("VFS_FIXTURE_PATH".to_string(), at(r"Data\hello.txt"));
    env.insert("VFS_FIXTURE_EXPECT".to_string(), LEN.to_string());
    env.insert("VFS_FIXTURE_FILL".to_string(), FILL.to_string());
    env.insert(
        "VFS_FIXTURE_NAMES".to_string(),
        names
            .iter()
            .map(|(k, o, w)| format!("{k}|{o}|{w}"))
            .collect::<Vec<_>>()
            .join(";"),
    );
    env.insert(
        "VFS_FIXTURE_NAME_PREFIXES".to_string(),
        prefixes
            .iter()
            .map(|(d, f)| format!("{d}|{f}"))
            .collect::<Vec<_>>()
            .join(";"),
    );
    env.insert("VFS_FIXTURE_NAME_CREATES".to_string(), creates.join(";"));
    env.insert(
        "VFS_FIXTURE_NAME_RENAMES".to_string(),
        renames
            .iter()
            .map(|(f, t)| format!("{f}|{t}"))
            .collect::<Vec<_>>()
            .join(";"),
    );
    env.insert(
        "VFS_FIXTURE_NAME_LISTS".to_string(),
        lists
            .iter()
            .map(|(d, c)| format!("{d}|{c}"))
            .collect::<Vec<_>>()
            .join(";"),
    );

    let code = s
        .launch(&LaunchOpts {
            image: "fixture.exe".into(),
            wait: true,
            shim_dll: Some(art.shim_dll()),
            payload_dll: Some(art.payload_dll()),
            env,
            ..Default::default()
        })
        .unwrap_or_else(|e| panic!("launch: {e}"));
    assert_eq!(
        code, 0,
        "the fixture exits 0 only if every name query on every path agreed; its own \
         `FIXTURE FAIL: names:` line above says which did not"
    );

    s.stop_serve();
    drop(s);
    drop(storage);
    for d in [&root, &state, &overlay, &upper, &lower, &storage_dir] {
        let _ = std::fs::remove_dir_all(d);
    }
}

// ---------------------------------------------------------------------------
// The shim's read cache
// ---------------------------------------------------------------------------

/// The immutable file the fixture reads in small pieces, as the child names
/// it: three blocks and a ragged tail, so reads straddle blocks and the last
/// block is short.
const CACHE_CHILD_PATH: &str = r"C:\vfs-session\root\data\cached.esm";
const CACHE_VPATH: &str = "data/cached.esm";
const CACHE_LEN: usize = 3 * 1024 * 1024 + 12_345;
/// A base file the fixture reads, rewrites through another handle, and reads
/// again. The rewrite is the same size, so the file's version (size, mount
/// generation) does not change: what keeps the old bytes from being served
/// is the cache dropping the file, not a new key.
const RW_CHILD_PATH: &str = r"C:\vfs-session\root\data\plugins.txt";
const RW_VPATH: &str = "data/plugins.txt";
const RW_BEFORE: &[u8] = b"*Skyrim.esm\n*Update.esm\n";
const RW_AFTER: &str = "*Skyrim.esm\n*Dragon.esm\n";
/// Where the shim's stats report lands: the session's state directory, which
/// the child sees as `C:\vfs-session\state`.
const STATS_CHILD_PATH: &str = r"C:\vfs-session\state\shim-stats.txt";

/// An immutable in-memory provider that counts the reads of each path that
/// reach it — what says the small reads were not each a round trip.
struct CountingImmutable {
    inner: vfs_embed::InlineProvider,
    handles: Mutex<BTreeMap<Handle, String>>,
    reads: Mutex<BTreeMap<String, (usize, usize)>>,
}

impl CountingImmutable {
    /// `(read_at calls, bytes)` that reached the provider for `vpath`.
    fn reads_of(&self, vpath: &str) -> (usize, usize) {
        self.reads
            .lock()
            .unwrap()
            .get(vpath)
            .copied()
            .unwrap_or_default()
    }
}

impl Provider for CountingImmutable {
    fn capabilities(&self) -> Capabilities {
        let caps = self.inner.capabilities();
        assert!(caps.immutable, "the test relies on an immutable base");
        caps
    }
    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        self.inner.getattr(p)
    }
    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        self.inner.readdir(p)
    }
    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        let r = self.inner.open(p, flags)?;
        self.handles
            .lock()
            .unwrap()
            .insert(r.0, p.rel.to_ascii_lowercase());
        Ok(r)
    }
    fn close(&self, h: Handle) -> Result<(), i32> {
        self.handles.lock().unwrap().remove(&h);
        self.inner.close(h)
    }
    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let r = self.inner.read_at(h, offset, buf);
        if let Some(path) = self.handles.lock().unwrap().get(&h).cloned() {
            let mut reads = self.reads.lock().unwrap();
            let e = reads.entry(path).or_default();
            e.0 += 1;
            e.1 += *r.as_ref().unwrap_or(&0);
        }
        r
    }
}

/// **The read cache, across the real boundary.** The fixture reads a 3 MiB
/// immutable file once in one call and then some hundred thousand times in
/// small pieces — explicit offsets of every size around the threshold, its
/// own position a byte at a time, seeks, six threads on shared and private
/// handles — and every piece must equal the big read. It then rewrites a
/// base file through another handle (a copy-up into the write layer) and
/// must read the new bytes back in small pieces.
///
/// Witnesses:
///
/// * the **fixture** exits 0 only if every small read agreed and the rewrite
///   read back fresh;
/// * the **provider** saw a few hundred reads of the cached file, not the
///   hundred thousand the fixture made;
/// * the shim's **stats report** shows read-cache hits.
#[test]
#[ignore = "needs a GE-Proton runtime, a bootable Wine prefix, and Windows-built artifacts \
            for this profile — see bin/build-windows"]
fn small_reads_of_an_immutable_file_are_served_by_the_shim_read_cache_under_proton() {
    let _one = ONE_LAUNCH.lock().unwrap_or_else(|e| e.into_inner());
    let Some(rig) = support::rig("proton_launch::small_reads_of_an_immutable_file_are_served_by_the_shim_read_cache_under_proton", "launch", &[vfs_proton::artifacts::FIXTURE_READ])
    else {
        return;
    };
    let art = &rig.art;

    let root = tmp("rc-root");
    let state = tmp("rc-state");
    let overlay = tmp("rc-overlay");
    let storage_dir = tmp("rc-storage");
    let image = root.join("fixture.exe");
    std::fs::copy(art.path(vfs_proton::artifacts::FIXTURE_READ), &image)
        .expect("copy the fixture into the root");

    let mut x = 0x0123_4567_89AB_CDEFu64;
    let cached: Vec<u8> = (0..CACHE_LEN)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect();
    let provider = Arc::new(CountingImmutable {
        inner: vfs_embed::InlineProvider::from_files([
            ("data/hello.txt", vec![FILL; LEN]),
            (CACHE_VPATH, cached),
            (RW_VPATH, RW_BEFORE.to_vec()),
        ]),
        handles: Mutex::new(BTreeMap::new()),
        reads: Mutex::new(BTreeMap::new()),
    });
    let storage = vfs_embed::Storage::open(&storage_dir, vfs_embed::StorageConfig::default())
        .expect("open storage");

    let mut s = Session::new();
    s.set_home(&rig.home);
    s.set_root(&root);
    s.set_state_dir(&state);
    s.set_overlay(&overlay);
    s.mount("", Arc::clone(&provider) as Arc<dyn Provider>)
        .expect("mount the provider over root 0");
    s.set_write_layer(storage.layer("write").expect("a write layer"))
        .expect("set the write layer");
    s.serve().expect("serve");

    let mut env = BTreeMap::new();
    for (name, value) in [
        ("VFS_FIXTURE_PATH", CHILD_PATH.to_string()),
        ("VFS_FIXTURE_EXPECT", LEN.to_string()),
        ("VFS_FIXTURE_FILL", FILL.to_string()),
        ("VFS_FIXTURE_CACHE_PATH", CACHE_CHILD_PATH.to_string()),
        ("VFS_FIXTURE_CACHE_RW_PATH", RW_CHILD_PATH.to_string()),
        ("VFS_FIXTURE_CACHE_RW_DATA", RW_AFTER.to_string()),
        ("VFS_FIXTURE_LINGER_MS", "600".to_string()),
        ("VFS_SHIM_STATS_LOG", STATS_CHILD_PATH.to_string()),
        ("VFS_SHIM_STATS_INTERVAL_MS", "100".to_string()),
    ] {
        env.insert(name.to_string(), value);
    }

    assert_eq!(RW_BEFORE.len(), RW_AFTER.len(), "a same-size rewrite");
    let code = s
        .launch(&LaunchOpts {
            image: "fixture.exe".into(),
            wait: true,
            shim_dll: Some(art.shim_dll()),
            payload_dll: Some(art.payload_dll()),
            env,
            ..Default::default()
        })
        .unwrap_or_else(|e| panic!("launch: {e}"));

    let (reads, bytes) = provider.reads_of(CACHE_VPATH);
    let report = std::fs::read_to_string(state.join("shim-stats.txt")).unwrap_or_default();
    let section: String = report
        .lines()
        .skip_while(|l| !l.starts_with("read cache"))
        .take(4)
        .collect::<Vec<_>>()
        .join("\n");
    eprintln!(
        "DIRECTOR: {reads} reads ({bytes} bytes) of {CACHE_VPATH} reached the provider\n\
         SHIM: {section}"
    );
    assert_eq!(
        code, 0,
        "the fixture exits 0 only if every small read equalled the big one and the rewritten \
         file read back fresh; its `FIXTURE FAIL: cache:` line above says what differed"
    );
    // The fixture made well over 100,000 small reads of the file; uncached,
    // each is at least one provider read. Cached, the provider sees one read
    // per block fetch (a block is one bulk request) plus the large reads
    // (whole-file and those at or over 64 KiB, a few per offset tried).
    assert!(
        reads < 30_000,
        "{reads} reads of {CACHE_VPATH} reached the provider: the small reads were not cached"
    );
    let hits: u64 = section
        .split_whitespace()
        .skip_while(|w| *w != "hits")
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    assert!(
        hits > 50_000,
        "the shim's stats must show read-cache hits; report section:\n{section}\n\nfull report:\n{report}"
    );

    s.stop_serve();
    drop(s);
    drop(storage);
    for d in [&root, &state, &overlay, &storage_dir] {
        let _ = std::fs::remove_dir_all(d);
    }
}

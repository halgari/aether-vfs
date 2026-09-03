//! **The increment's definition of done**: the public API — `Session::serve()`
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
//! `Session::launch` links the session's root, overlay and state directory
//! into `<prefix>/drive_c/vfs-session/{root,overlay,state}` — a Wine process
//! can only name what is under one of its drives, and those three live
//! wherever the host put them. So the managed root is always
//! `C:\vfs-session\root` inside the child, whatever the host path is, and the
//! virtual file is at `C:\vfs-session\root\data\hello.txt`. That is a private
//! constant of `Session` (`WINE_LINK_DIR`), not public API; there is no
//! accessor for it yet, and inventing one is a change to a shipped struct that
//! this increment does not make. If the two ever drift, this test fails with
//! the fixture reporting the path it could not read, which names the drift.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use vfs_embed::{
    Capabilities, DirEntry, DiskProvider, Handle, LaunchOpts, Provider, RootId, Session, SetAttr,
    Stat, VPath,
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

// ---------------------------------------------------------------------------
// The Director side: a provider that says what it was asked.
// ---------------------------------------------------------------------------

/// A [`DiskProvider`] that logs every call to stderr and records the vpaths it
/// served. The log is this run's Director-side transcript, and `served` is
/// what turns "the child exited 0" into "the child's bytes came from here".
struct Loud {
    disk: DiskProvider,
    /// `(op, vpath)` for the path-addressed calls, plus `("read_at", vpath)`
    /// resolved back through `handles`.
    calls: Mutex<Vec<(String, String)>>,
    /// Open handles, so a `read_at` (which carries no path) can be attributed.
    handles: Mutex<BTreeMap<Handle, String>>,
}

impl Loud {
    fn new(root: &Path) -> Self {
        Loud {
            disk: DiskProvider::new(root),
            calls: Mutex::new(Vec::new()),
            handles: Mutex::new(BTreeMap::new()),
        }
    }

    fn note(&self, op: &str, path: &str, detail: &str) {
        eprintln!("DIRECTOR: {op} {path:?} {detail}");
        self.calls
            .lock()
            .unwrap()
            .push((op.to_string(), path.to_string()));
    }

    fn saw(&self, op: &str, path: &str) -> bool {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .any(|(o, p)| o == op && p == path)
    }

    fn transcript(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(o, p)| format!("{o} {p}"))
            .collect()
    }
}

impl Provider for Loud {
    fn capabilities(&self) -> Capabilities {
        self.disk.capabilities()
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        let r = self.disk.getattr(p);
        self.note(
            "getattr",
            p.rel,
            &match &r {
                Ok(Some(s)) => format!("-> kind={} size={}", s.kind, s.size),
                Ok(None) => "-> absent".to_string(),
                Err(e) => format!("-> err {e}"),
            },
        );
        r
    }

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        let r = self.disk.readdir(p);
        self.note(
            "readdir",
            p.rel,
            &match &r {
                Ok(v) => format!("-> {} entries", v.len()),
                Err(e) => format!("-> err {e}"),
            },
        );
        r
    }

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        let r = self.disk.open(p, flags);
        if let Ok((h, _, _)) = &r {
            self.handles.lock().unwrap().insert(*h, p.rel.to_string());
        }
        self.note(
            "open",
            p.rel,
            &match &r {
                Ok((h, size, dir)) => format!("flags={flags:#x} -> fh={h} size={size} dir={dir}"),
                Err(e) => format!("flags={flags:#x} -> err {e}"),
            },
        );
        r
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        let path = self.handles.lock().unwrap().remove(&h).unwrap_or_default();
        let r = self.disk.close(h);
        self.note("close", &path, &format!("fh={h} -> {r:?}"));
        r
    }

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let path = self
            .handles
            .lock()
            .unwrap()
            .get(&h)
            .cloned()
            .unwrap_or_default();
        let r = self.disk.read_at(h, offset, buf);
        self.note(
            "read_at",
            &path,
            &format!("fh={h} offset={offset} want={} -> {r:?}", buf.len()),
        );
        r
    }

    fn set_attr(&self, p: VPath, attr: SetAttr) -> Result<(), i32> {
        self.disk.set_attr(p, attr)
    }
}

// ---------------------------------------------------------------------------
// Locating what a Linux box cannot build
// ---------------------------------------------------------------------------

fn profile_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let dir = exe.parent().unwrap();
    if dir.file_name().and_then(|s| s.to_str()) == Some("deps") {
        dir.parent().unwrap().to_path_buf()
    } else {
        dir.to_path_buf()
    }
}

/// `vfs-injector.exe`, `vfs_shim_dll.dll`, `vfs_payload.dll` and the fixture
/// are **Windows PEs and cannot be built here**, so they are copied in beside
/// the test binary. Missing ones are named together with where they go: one
/// message per run instead of one per artifact.
fn windows_artifacts() -> BTreeMap<&'static str, PathBuf> {
    let profile = profile_dir();
    let names = [
        "vfs-injector.exe",
        "vfs_shim_dll.dll",
        "vfs_payload.dll",
        "vfs-fixture-read.exe",
    ];
    let mut found = BTreeMap::new();
    let mut missing = Vec::new();
    for name in names {
        let mut hit = None;
        for cand in [profile.join(name), profile.join("deps").join(name)] {
            if cand.is_file() {
                hit = Some(cand);
                break;
            }
        }
        match hit {
            Some(p) => {
                found.insert(name, p);
            }
            None => missing.push(name),
        }
    }
    assert!(
        missing.is_empty(),
        "these Windows artifacts are missing: {}.\n\
         Cross-build them (`cargo xwin build --target x86_64-pc-windows-msvc -p vfs-inject \
         -p vfs-shim-dll -p vfs-fixture-read` plus the separate `crates/vfs-payload` \
         workspace) or build them on Windows, and copy them into {} (or its `deps/`).",
        missing.join(", "),
        profile.display()
    );
    found
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("vfs-proton-launch-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

/// **The whole increment.** `Session::serve()` + `Session::launch()` start a
/// Windows executable under GE-Proton, the injected shim routes its
/// `NtCreateFile`/`NtReadFile` back over a file-backed ring to this native
/// Linux Director, and the file it reads exists on no filesystem the Wine
/// process can see.
///
/// Requires, and cannot provide for itself:
/// * a verified **GE-Proton runtime** under `$VFS_HOME/runtimes` (install with
///   `vfs-proton install`);
/// * a **Wine prefix**, which `Session::launch` boots itself under
///   `$VFS_HOME/sessions/<id>/prefix` — so a 32-bit runtime must be installed
///   (`lib32-glibc`, `lib32-gcc-libs` on Arch), since `wine`'s launcher probes
///   for the 32-bit loader even under `WINEARCH=win64`;
/// * the four **Windows-built artifacts** listed in [`windows_artifacts`].
#[test]
#[ignore = "needs a Wine host (Linux: a GE-Proton runtime under $VFS_HOME/runtimes; macOS: \
            CrossOver installed), a bootable prefix, and the Windows artifacts \
            (vfs-injector.exe, vfs_shim_dll.dll, vfs_payload.dll, vfs-fixture-read.exe) \
            copied beside the test binary"]
fn session_launches_a_windows_fixture_under_proton_that_reads_from_the_provider() {
    launch_the_fixture(None);
}

/// The same launch, with the prefix named by the host rather than derived.
///
/// **This is what protects an expensive prefix.** The derived name hashes
/// `state_dir` with `DefaultHasher`, whose output is not promised across Rust
/// releases, so a rebuilt host silently boots a *new* prefix. That is cheap
/// when a prefix is a `wineboot` and ruinous when it holds a logged-in Steam
/// client — which on macOS it must, because a Steam-DRM'd game checks with a
/// client in its own Windows environment. So the test asserts both halves:
/// the prefix is created where it was named, and nothing was derived under
/// the home's `sessions/`.
#[test]
#[ignore = "same requirements as the launch test above"]
fn set_prefix_dir_puts_the_prefix_where_the_host_named_it() {
    let home = PathBuf::from(std::env::var("VFS_HOME").expect("VFS_HOME"));
    let sessions_before = count_dirs(&home.join("sessions"));

    let named = tmp("named-prefix").join("prefix");
    launch_the_fixture(Some(&named));

    assert!(
        named.join("drive_c").join("windows").join("system32").is_dir(),
        "the prefix must be booted at the directory the host named, not merely created: {}",
        named.display()
    );
    assert_eq!(
        count_dirs(&home.join("sessions")),
        sessions_before,
        "naming a prefix must not also derive one under {}/sessions — a host that got both \
         would be paying for two prefixes and trusting the wrong one",
        home.display()
    );
    let _ = std::fs::remove_dir_all(named.parent().unwrap());
}

/// A launch whose file lives in **root 1**, not root 0.
///
/// The unix launch path refused a multi-root session outright until the root
/// map travelled with it, and the refusal was right: `VFS_VIRTUAL_ROOTS` is
/// how the shim learns that a second tree is virtualised at all, and a root it
/// has never heard of is one whose paths it classifies as nobody's and lets
/// fall through to **real disk** — no error, no log, just a game reading an
/// empty directory where its load order should be.
///
/// So this test is deliberately arranged so that fall-through cannot pass:
/// root 1's host directory is left empty and the bytes exist only inside its
/// provider. A shim that ignored the root map would find nothing and the
/// fixture would fail; one that read the wrong root would miss too.
#[test]
#[ignore = "same requirements as the launch test above"]
fn a_declared_second_root_is_served_through_the_ring() {
    let art = windows_artifacts();
    let root = tmp("mr-root");
    let state = tmp("mr-state");
    let overlay = tmp("mr-overlay");
    // Root 1's *host* directory. Deliberately empty: everything the child
    // reads from root 1 comes from the provider below, so a fall-through to
    // disk finds nothing rather than accidentally succeeding.
    let root1 = tmp("mr-root1");
    let content1 = tmp("mr-content1");
    std::fs::create_dir_all(content1.join("data")).unwrap();
    std::fs::write(content1.join("data").join("hello.txt"), [FILL; LEN]).unwrap();

    let image = root.join("fixture.exe");
    std::fs::copy(&art["vfs-fixture-read.exe"], &image).expect("copy the fixture");

    // Root 0 gets a provider with *nothing* in it, so that a read answered
    // from root 0 by mistake is a failure rather than a coincidence.
    let empty = tmp("mr-empty");
    let p0 = Arc::new(Loud::new(&empty));
    let p1 = Arc::new(Loud::new(&content1));

    let mut s = Session::new();
    s.set_root(&root);
    s.set_state_dir(&state);
    s.set_overlay(&overlay);
    s.declare_root(1, &root1);
    s.mount("", Arc::clone(&p0) as Arc<dyn Provider>).expect("mount root 0");
    s.mount_at(RootId(1), "", Arc::clone(&p1) as Arc<dyn Provider>).expect("mount root 1");
    s.serve().expect("serve");

    // `link_into_prefix` names a declared root `root-<id>` beside `root`.
    let child_path = r"C:\vfs-session\root-1\data\hello.txt";
    let mut env = BTreeMap::new();
    env.insert("VFS_FIXTURE_PATH".to_string(), child_path.to_string());
    env.insert("VFS_FIXTURE_EXPECT".to_string(), LEN.to_string());
    env.insert("VFS_FIXTURE_FILL".to_string(), FILL.to_string());

    let code = s
        .launch(&LaunchOpts {
            image: "fixture.exe".into(),
            wait: true,
            shim_dll: Some(art["vfs_shim_dll.dll"].to_string_lossy().into_owned()),
            payload_dll: Some(art["vfs_payload.dll"].to_string_lossy().into_owned()),
            env,
            ..Default::default()
        })
        .unwrap_or_else(|e| {
            panic!("launch: {e}\nroot0 saw: {:?}\nroot1 saw: {:?}", p0.transcript(), p1.transcript())
        });

    assert_eq!(
        code, 0,
        "the fixture exits 0 only if it read {LEN} bytes from {child_path}. \
         root1 saw: {:?}",
        p1.transcript()
    );
    assert!(
        p1.saw("open", VPATH) && p1.saw("read_at", VPATH),
        "root 1's provider must have served the bytes — otherwise the root map never \
         reached the shim. root1 saw: {:?}",
        p1.transcript()
    );
    assert!(
        !p0.saw("read_at", VPATH),
        "root 0 must not have answered a read addressed to root 1. root0 saw: {:?}",
        p0.transcript()
    );

    s.stop_serve();
    for d in [&root, &state, &overlay, &root1, &content1, &empty] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// A launch whose **image itself** exists only in the provider graph.
///
/// The Wine path refused this by name until now — "staging a graph-only image
/// is not wired to this path yet" — and the refusal was about wiring, not
/// about a barrier: `Session::stage_launch` was already portable, and it
/// writes into the managed root, which is already linked into the prefix as
/// `C:\vfs-session\root`. So the staged exe is nameable from inside Wine the
/// moment it exists.
///
/// It has to work, because it is how a real loadout launches. A script
/// extender spawns the game with its own `CreateProcess`, which no hook of
/// ours intercepts, so the game exe must be a **real file** beside the loader
/// even though the collection serves it out of archives.
///
/// The managed root starts empty here, so a launch that skipped staging would
/// find no image at all.
#[test]
#[ignore = "same requirements as the launch test above"]
fn an_image_served_only_by_the_graph_is_staged_and_launched() {
    let art = windows_artifacts();
    let root = tmp("stage-root");
    let state = tmp("stage-state");
    let overlay = tmp("stage-overlay");

    // The provider's content: the fixture exe *and* the data it will read.
    // Neither is under the managed root.
    let content = tmp("stage-content");
    std::fs::create_dir_all(content.join("data")).unwrap();
    std::fs::write(content.join("data").join("hello.txt"), [FILL; LEN]).unwrap();
    std::fs::copy(&art["vfs-fixture-read.exe"], content.join("fixture.exe"))
        .expect("the image goes in the provider, not the root");

    assert!(
        !root.join("fixture.exe").exists(),
        "the managed root must start without the image — that is the whole point"
    );

    let provider = Arc::new(Loud::new(&content));
    let mut s = Session::new();
    s.set_root(&root);
    s.set_state_dir(&state);
    s.set_overlay(&overlay);
    s.mount("", Arc::clone(&provider) as Arc<dyn Provider>).expect("mount root 0");
    s.serve().expect("serve");

    let mut env = BTreeMap::new();
    env.insert("VFS_FIXTURE_PATH".to_string(), CHILD_PATH.to_string());
    env.insert("VFS_FIXTURE_EXPECT".to_string(), LEN.to_string());
    env.insert("VFS_FIXTURE_FILL".to_string(), FILL.to_string());

    let code = s
        .launch(&LaunchOpts {
            image: "fixture.exe".into(),
            wait: true,
            shim_dll: Some(art["vfs_shim_dll.dll"].to_string_lossy().into_owned()),
            payload_dll: Some(art["vfs_payload.dll"].to_string_lossy().into_owned()),
            env,
            ..Default::default()
        })
        .unwrap_or_else(|e| panic!("launch: {e}\nDIRECTOR saw: {:?}", provider.transcript()));

    assert_eq!(
        code, 0,
        "the staged image must run and read {LEN} bytes from the graph. DIRECTOR saw: {:?}",
        provider.transcript()
    );
    assert!(
        root.join("fixture.exe").is_file(),
        "staging must write the image into the managed root, where Wine can name it"
    );

    s.stop_serve();
    for d in [&root, &state, &overlay, &content] {
        let _ = std::fs::remove_dir_all(d);
    }
}

fn count_dirs(at: &Path) -> usize {
    std::fs::read_dir(at).map(|d| d.flatten().count()).unwrap_or(0)
}

fn launch_the_fixture(prefix: Option<&Path>) {
    let art = windows_artifacts();
    assert!(
        std::env::var_os("VFS_HOME").is_some(),
        "set VFS_HOME to the aether-vfs home: on Linux it holds runtimes/GE-Proton…, and on \
         both platforms it is where Session::launch boots this session's prefix"
    );

    let root = tmp("root");
    let state = tmp("state");
    let overlay = tmp("overlay");
    // The bytes live here, and this directory is under no Wine drive: it is
    // not the managed root, not in the prefix, and nothing links it in. The
    // only way the child can see it is through the ring.
    let content = tmp("content");
    std::fs::create_dir_all(content.join("data")).unwrap();
    std::fs::write(content.join("data").join("hello.txt"), [FILL; LEN]).unwrap();

    // The image must be a **real file** under the managed root: `CreateProcess`
    // inside Wine reads it before any hook of ours exists in the child, and
    // staging a graph-only image is not wired to the Proton path (`launch`
    // refuses it by name). So the fixture is copied in, and the *data* is what
    // stays virtual.
    let image = root.join("fixture.exe");
    std::fs::copy(&art["vfs-fixture-read.exe"], &image).expect("copy the fixture into the root");

    let provider = Arc::new(Loud::new(&content));

    let mut s = Session::new();
    s.set_root(&root);
    s.set_state_dir(&state);
    s.set_overlay(&overlay);
    if let Some(dir) = prefix {
        s.set_prefix_dir(dir);
    }
    s.mount("", Arc::clone(&provider) as Arc<dyn Provider>)
        .expect("mount the disk-backed provider over root 0");
    s.serve().expect("serve");

    let ipc = s.ipc().expect("serve() must leave a live ring");
    eprintln!(
        "DIRECTOR: ring {} map_bytes={} arena_offset={} arena_len={} payload_cap={}",
        ipc.ring_path().map(|p| p.display().to_string()).unwrap_or_default(),
        ipc.map_bytes,
        ipc.arena_offset,
        ipc.arena_len,
        ipc.payload_cap
    );

    // Read it back through the graph first. If this fails, the launch was
    // never going to work and the diagnosis is on this side of the ring.
    assert_eq!(
        s.read_file(VPATH).expect("the provider must serve the vpath").len(),
        LEN,
        "the host-side read through the same graph the child will use"
    );
    assert!(
        !root.join("data").join("hello.txt").exists(),
        "the file must exist only in the provider — a copy under the managed root \
         would make the child's read prove nothing"
    );

    let mut env = BTreeMap::new();
    env.insert("VFS_FIXTURE_PATH".to_string(), CHILD_PATH.to_string());
    env.insert("VFS_FIXTURE_EXPECT".to_string(), LEN.to_string());
    env.insert("VFS_FIXTURE_FILL".to_string(), FILL.to_string());

    let code = s
        .launch(&LaunchOpts {
            image: "fixture.exe".into(),
            wait: true,
            // `vfs-injector.exe` is taken from the directory holding `shim_dll`,
            // which is why setting this one path is enough for all three.
            shim_dll: Some(art["vfs_shim_dll.dll"].to_string_lossy().into_owned()),
            payload_dll: Some(art["vfs_payload.dll"].to_string_lossy().into_owned()),
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

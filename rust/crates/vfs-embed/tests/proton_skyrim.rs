//! The Proton launch spike, automated: vanilla Skyrim Special Edition started
//! through `vfs_embed::Session` under GE-Proton from a **fully virtual** game
//! directory, kept running for a while, then stopped.
//!
//! What it proves, each asserted:
//!
//! * **Root 0 is virtual.** Its backing directory starts empty; the game
//!   directory is a read-only `DiskProvider` mount the Wine prefix cannot
//!   name. `SkyrimSE.exe` reaches disk only by staging (`stage_also`), with
//!   its import closure, and nothing else does.
//! * **The game runs from it.** A `SkyrimSE.exe` process whose image is under
//!   root 0's location stays alive for `VFS_TEST_SKYRIM_ALIVE_SECS`, and the
//!   provider saw it open game data (`.esm`/`.bsa` under `Data/`).
//! * **The Haskill launch shape works**: an explicit aether home
//!   (`Session::set_home`) whose runtime is a **symlink** to the real one, a
//!   Proton-initialized named prefix (`PrefixInit::Proton`), `SteamAppId`
//!   passed in the child's environment only, more I/O workers, and a
//!   detached launch stopped through its handle.
//!
//! **Env-gated, and `#[ignore]`d.** It needs a local, Steam-installed Skyrim
//! SE and a running Steam client, which no CI runner has; it returns early
//! (passing) unless `VFS_TEST_SKYRIM_DIR` names the game directory. Run:
//!
//! ```text
//! bin/build-windows
//! VFS_TEST_SKYRIM_DIR="$HOME/.local/share/Steam/steamapps/common/Skyrim Special Edition" \
//!   timeout 900 cargo test -p vfs-embed --test proton_skyrim -- --ignored --nocapture
//! ```
//!
//! Optional: `VFS_TEST_PROTON_RUNTIME` (a GE-Proton directory; default the
//! newest verified runtime in the environment's aether home),
//! `VFS_TEST_STEAM_CLIENT` (default `~/.local/share/Steam`),
//! `VFS_TEST_SKYRIM_ALIVE_SECS` (default 30), `VFS_TEST_SKYRIM_IMAGE`
//! (default `SkyrimSE.exe`; `skse64_loader.exe` tries the SKSE path),
//! `VFS_TEST_SKYRIM_DXVK=1` (launch with Proton's DXVK/vkd3d overrides).
//! The game directory is never written: root 0 has a write layer of its own.
//!
//! The launch's liveness is the **prefix's**, not the launched image's: with
//! `VFS_TEST_SKYRIM_IMAGE=skse64_loader.exe` the loader starts `SkyrimSE.exe`
//! and exits, and the handle must still report the launch running while the
//! game does (`try_wait` is `None` throughout the alive window) and stop it
//! afterwards. After the stop no process is left in the prefix.
//!
//! Scratch lives under Cargo's `CARGO_TARGET_TMPDIR`, not `/tmp`, and is
//! removed by a guard that runs on a panic too.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vfs_embed::{
    Capabilities, DirEntry, DiskProvider, Handle, LaunchExit, LaunchOpts, PrefixInit, Provider,
    ReadOnlyProvider, RootId, Session, Stat, VPath, PROTON_GRAPHICS_OVERRIDES,
};

/// Root 0's location in the prefix: unique per run, so a process match below
/// cannot be some other Skyrim.
fn root0_location() -> String {
    format!(r"C:\aether-e2e\skyrim-{}", std::process::id())
}

/// Root 0's provider, recording every path it opened.
struct Recording {
    inner: Arc<dyn Provider>,
    opened: Mutex<Vec<String>>,
}

impl Recording {
    fn opened(&self) -> Vec<String> {
        self.opened.lock().unwrap().clone()
    }
}

impl Provider for Recording {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        self.inner.getattr(p)
    }
    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        self.inner.readdir(p)
    }
    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        let r = self.inner.open(p, flags);
        if r.is_ok() {
            self.opened.lock().unwrap().push(p.rel.to_ascii_lowercase());
        }
        r
    }
    fn close(&self, h: Handle) -> Result<(), i32> {
        self.inner.close(h)
    }
    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        self.inner.read_at(h, offset, buf)
    }
}

fn profile_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let dir = exe.parent().unwrap();
    if dir.file_name().and_then(|s| s.to_str()) == Some("deps") {
        dir.parent().unwrap().to_path_buf()
    } else {
        dir.to_path_buf()
    }
}

/// `vfs_shim_dll.dll` and `vfs_payload.dll` beside the test binary, where
/// `bin/build-windows` copies them (with `vfs-injector.exe`).
fn artifacts() -> (String, String) {
    let p = profile_dir();
    let (shim, payload) = (p.join("vfs_shim_dll.dll"), p.join("vfs_payload.dll"));
    for f in [&shim, &payload, &p.join("vfs-injector.exe")] {
        assert!(f.is_file(), "{} is missing: run bin/build-windows", f.display());
    }
    (shim.to_string_lossy().into_owned(), payload.to_string_lossy().into_owned())
}

/// This run's scratch root, under the target directory.
fn scratch_root() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("vfs-skyrim-{}", std::process::id()))
}

fn tmp(name: &str) -> PathBuf {
    let d = scratch_root().join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Removes this run's scratch when dropped — after the session (declared
/// later, so dropped first) has stopped the game and let go of the prefix,
/// and on a panic as much as on success.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Pids of processes whose environment names `prefix` as their
/// `WINEPREFIX`: every Wine process of the launch inherits it.
fn prefix_processes(prefix: &Path) -> Vec<u32> {
    let want = format!("WINEPREFIX={}", prefix.display()).into_bytes();
    let Ok(rd) = std::fs::read_dir("/proc") else { return Vec::new() };
    rd.flatten()
        .filter_map(|e| {
            let pid: u32 = e.file_name().to_str()?.parse().ok()?;
            let env = std::fs::read(e.path().join("environ")).ok()?;
            env.split(|b| *b == 0).any(|kv| kv == want.as_slice()).then_some(pid)
        })
        .collect()
}

/// The environment's aether home, as `Session` resolves it without `set_home`.
fn env_home() -> PathBuf {
    if let Some(h) = std::env::var_os("VFS_HOME") {
        return PathBuf::from(h);
    }
    if let Some(x) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(x).join("aether-vfs");
    }
    PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".local/share/aether-vfs")
}

/// The newest runtime directory under `home/runtimes` with a GE `version`.
fn newest_runtime(home: &Path) -> Option<PathBuf> {
    let mut found: Vec<(String, PathBuf)> = std::fs::read_dir(home.join("runtimes"))
        .ok()?
        .flatten()
        .filter_map(|e| {
            let v = std::fs::read_to_string(e.path().join("version")).ok()?;
            let tag = v.split_whitespace().find(|t| t.starts_with("GE-Proton"))?.to_string();
            Some((tag, e.path()))
        })
        .collect();
    // Numeric `(N, M)` order, as `vfs_proton::cmp_tags` does it.
    let key = |tag: &str| -> (u64, u64) {
        let (n, m) = tag.trim_start_matches("GE-Proton").split_once('-').unwrap_or(("0", "0"));
        (n.parse().unwrap_or(0), m.parse().unwrap_or(0))
    };
    found.sort_by_key(|(tag, _)| key(tag));
    found.pop().map(|(_, p)| p)
}

/// The GE-Proton runtime to launch with.
fn runtime() -> PathBuf {
    if let Some(p) = std::env::var_os("VFS_TEST_PROTON_RUNTIME") {
        return PathBuf::from(p);
    }
    let home = env_home();
    newest_runtime(&home).unwrap_or_else(|| {
        panic!("no GE-Proton runtime under {}/runtimes: set VFS_TEST_PROTON_RUNTIME", home.display())
    })
}

/// Argv[0] of every live process, lowercased.
fn process_images() -> Vec<(u32, String)> {
    let Ok(rd) = std::fs::read_dir("/proc") else { return Vec::new() };
    rd.flatten()
        .filter_map(|e| {
            let pid: u32 = e.file_name().to_str()?.parse().ok()?;
            let cmd = std::fs::read(e.path().join("cmdline")).ok()?;
            let argv0 = cmd.split(|b| *b == 0).next()?;
            Some((pid, String::from_utf8_lossy(argv0).to_ascii_lowercase()))
        })
        .collect()
}

/// The pid of a `SkyrimSE.exe` whose image is under `location`, if running.
/// Wine shows a process's Windows image path as its argv[0].
fn skyrim_pid(location: &str) -> Option<u32> {
    let want = location.to_ascii_lowercase();
    process_images()
        .into_iter()
        .find(|(_, a)| a.starts_with(&want) && a.ends_with("skyrimse.exe"))
        .map(|(pid, _)| pid)
}

#[test]
#[ignore = "needs a local Skyrim SE (VFS_TEST_SKYRIM_DIR), a running Steam client, a GE-Proton \
            runtime and bin/build-windows artifacts"]
fn vanilla_skyrim_runs_from_a_fully_virtual_root_under_proton() {
    let Some(game) = std::env::var_os("VFS_TEST_SKYRIM_DIR").map(PathBuf::from) else {
        eprintln!("VFS_TEST_SKYRIM_DIR is unset: skipping the Skyrim launch");
        return;
    };
    assert!(game.join("SkyrimSE.exe").is_file(), "{} has no SkyrimSE.exe", game.display());
    let alive = Duration::from_secs(
        std::env::var("VFS_TEST_SKYRIM_ALIVE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(30),
    );
    let image = std::env::var("VFS_TEST_SKYRIM_IMAGE").unwrap_or_else(|_| "SkyrimSE.exe".into());
    let steam = std::env::var_os("VFS_TEST_STEAM_CLIENT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".local/share/Steam")
        });
    let (shim, payload) = artifacts();
    let _scratch = Scratch(scratch_root());

    // An aether home of this run's own, whose one runtime is a symlink.
    let home = tmp("home");
    std::fs::create_dir_all(home.join("runtimes")).unwrap();
    let rt = runtime();
    std::os::unix::fs::symlink(&rt, home.join("runtimes").join(rt.file_name().unwrap())).unwrap();

    let root0 = tmp("root0");
    let upper = tmp("upper");
    let state = tmp("state");
    let overlay = tmp("overlay");
    let location = root0_location();
    let provider = Arc::new(Recording {
        inner: Arc::new(ReadOnlyProvider::new(Arc::new(DiskProvider::new(&game)))),
        opened: Mutex::new(Vec::new()),
    });

    let mut s = Session::new();
    s.set_home(&home);
    s.set_root(&root0);
    s.set_state_dir(&state);
    s.set_overlay(&overlay);
    s.declare_root(0, &location);
    s.set_prefix_name("skyrim-e2e").unwrap();
    s.set_prefix_init(PrefixInit::Proton { steam_client: steam, app_id: Some(489830) });
    s.set_io_workers(8);
    s.mount_at(RootId(0), "", Arc::clone(&provider) as Arc<dyn Provider>).unwrap();
    s.set_write_layer_at(RootId(0), Arc::new(DiskProvider::new(&upper))).unwrap();
    s.serve().unwrap();
    assert_eq!(std::fs::read_dir(&root0).unwrap().count(), 0, "root 0 starts empty");

    let mut env = BTreeMap::from([
        ("SteamAppId".to_string(), "489830".to_string()),
        ("SteamGameId".to_string(), "489830".to_string()),
    ]);
    if std::env::var("VFS_TEST_SKYRIM_DXVK").as_deref() == Ok("1") {
        env.insert("WINEDLLOVERRIDES".to_string(), PROTON_GRAPHICS_OVERRIDES.to_string());
    }
    let mut h = s
        .launch_detached(&LaunchOpts {
            image,
            stage_also: vec!["SkyrimSE.exe".into()],
            shim_dll: Some(shim),
            payload_dll: Some(payload),
            env,
            ready_timeout: Some(Duration::from_secs(300)),
            ..Default::default()
        })
        .expect("launch");

    let started = Instant::now();
    let pid = loop {
        if let Some(pid) = skyrim_pid(&location) {
            break pid;
        }
        if let Some(exit) = h.try_wait().unwrap() {
            panic!("the launch ended before SkyrimSE.exe appeared: {exit:?}; opened: {:?}", provider.opened());
        }
        assert!(started.elapsed() < Duration::from_secs(300), "SkyrimSE.exe never appeared");
        std::thread::sleep(Duration::from_millis(250));
    };
    eprintln!("SkyrimSE.exe is pid {pid} after {:?}", started.elapsed());

    let up = Instant::now();
    while up.elapsed() < alive {
        assert!(
            Path::new(&format!("/proc/{pid}")).exists(),
            "SkyrimSE.exe died after {:?}; opened: {:?}",
            up.elapsed(),
            provider.opened()
        );
        assert_eq!(h.try_wait().unwrap(), None, "the launch ended after {:?}", up.elapsed());
        std::thread::sleep(Duration::from_secs(1));
    }

    let opened = provider.opened();
    eprintln!("the provider served {} opens", opened.len());
    assert!(
        opened.iter().any(|p| p.starts_with("data/") && (p.ends_with(".esm") || p.ends_with(".bsa"))),
        "the game must have opened its data through the provider: {opened:?}"
    );
    // Only staging wrote into root 0: the images and their imports, no Data.
    for e in std::fs::read_dir(&root0).unwrap().flatten() {
        let name = e.file_name().to_string_lossy().to_ascii_lowercase();
        assert!(
            e.path().is_file() && (name.ends_with(".exe") || name.ends_with(".dll")),
            "root 0 holds only staged images, not {name}"
        );
    }

    assert_eq!(h.stop().unwrap(), LaunchExit::Stopped);
    let gone = Instant::now();
    while skyrim_pid(&location).is_some() {
        assert!(gone.elapsed() < Duration::from_secs(30), "SkyrimSE.exe outlived the stop");
        std::thread::sleep(Duration::from_millis(250));
    }
    // `Stopped` is reported only once the prefix is quiet.
    let pfx = home.join("sessions").join("skyrim-e2e").join("compat").join("pfx");
    let left = prefix_processes(&pfx);
    assert!(left.is_empty(), "processes left in the prefix after the stop: {left:?}");
    s.stop_serve();
}

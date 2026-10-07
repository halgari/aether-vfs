//! Shared support for the vfs-embed end-to-end tests that launch a Windows program under
//! GE-Proton (`proton_launch`, `proton_registry`, `proton_steam`, `proton_nvapi`,
//! `proton_skyrim`; `proton_fake_runtime` uses only [`scratch`]). Each test file pulls it in
//! with `mod support;`, so every file compiles its own copy and uses a subset of it.
//!
//! # The three rules
//!
//! **Scratch.** [`scratch`] puts a test's directories under Cargo's `CARGO_TARGET_TMPDIR`
//! (`target/tmp`), never `/tmp`. [`throwaway_home`] gives a launch an aether home of its own
//! there, so a Wine prefix never lands in the user's real home.
//!
//! **Artefacts.** The Windows binaries come from `bin/build-windows`, which copies them into
//! `target/<profile>`. A test looks only in its own profile (and that profile's `deps/`);
//! it never borrows a build from the other one, because a shim from a different build can
//! carry another ring `VERSION` and refuse to attach. If they are missing but the other
//! profile has them, the message says so and names the command. `VFS_WINDOWS_ARTIFACTS=<dir>`
//! points the tests at one directory instead.
//!
//! **Prerequisites.** A missing prerequisite (the runtime, the artefacts, a Steam client, a
//! `steam_api64.dll`, a local Skyrim, an NVIDIA GPU) makes the test print
//! `SKIP <test>: <reason, with the command to run>` and pass. It never panics. With
//! `VFS_TEST_REQUIRE_ALL=1` the skip is a failure instead, for a fully provisioned machine.
//! `VFS_HOME` is never required: the runtime is found through `vfs_proton::Root::from_env`
//! (`VFS_HOME`, else the XDG data home), or `VFS_TEST_PROTON_RUNTIME` names one directly.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use vfs_embed::{Capabilities, DirEntry, DiskProvider, Handle, Provider, SetAttr, Stat, VPath};

// ---------------------------------------------------------------------------
// Scratch
// ---------------------------------------------------------------------------

/// A fresh, empty directory `target/tmp/<group>-<pid>-<tag>`.
pub fn scratch(group: &str, tag: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("{group}-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

// ---------------------------------------------------------------------------
// Prerequisite policy
// ---------------------------------------------------------------------------

/// `VFS_TEST_REQUIRE_ALL=1`: a skipped prerequisite fails the test.
pub fn require_all() -> bool {
    std::env::var("VFS_TEST_REQUIRE_ALL").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Report a missing prerequisite: print `SKIP <test>: <why>` and return (the caller then
/// returns), or panic under `VFS_TEST_REQUIRE_ALL=1`.
pub fn skip(test: &str, why: impl Display) {
    if require_all() {
        panic!("{test}: {why} (VFS_TEST_REQUIRE_ALL is set, so a skip is a failure)");
    }
    eprintln!("SKIP {test}: {why}");
}

// ---------------------------------------------------------------------------
// Windows artefacts
// ---------------------------------------------------------------------------

/// The engine binaries every Proton launch needs.
pub const ENGINE: [&str; 3] = ["vfs-injector.exe", "vfs_shim_dll.dll", "vfs_payload.dll"];

/// `target/<profile>` of the running test binary.
pub fn profile_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let dir = exe.parent().unwrap();
    if dir.file_name().and_then(|s| s.to_str()) == Some("deps") {
        dir.parent().unwrap().to_path_buf()
    } else {
        dir.to_path_buf()
    }
}

/// The build flag for this test's profile: `""` (debug) or `" --release"`.
fn profile_flag() -> &'static str {
    if cfg!(debug_assertions) {
        ""
    } else {
        " --release"
    }
}

/// Located Windows binaries, by file name.
pub struct Artifacts(BTreeMap<&'static str, PathBuf>);

impl Artifacts {
    pub fn path(&self, name: &str) -> &Path {
        self.0
            .get(name)
            .unwrap_or_else(|| panic!("{name} was not asked for"))
    }

    /// `LaunchOpts::shim_dll`. The injector is looked for beside it.
    pub fn shim_dll(&self) -> String {
        self.path("vfs_shim_dll.dll").to_string_lossy().into_owned()
    }

    /// `LaunchOpts::payload_dll`.
    pub fn payload_dll(&self) -> String {
        self.path("vfs_payload.dll").to_string_lossy().into_owned()
    }
}

fn find_in(
    dir: &Path,
    names: &[&'static str],
) -> (BTreeMap<&'static str, PathBuf>, Vec<&'static str>) {
    let mut found = BTreeMap::new();
    let mut missing = Vec::new();
    for &name in names {
        match [dir.join(name), dir.join("deps").join(name)]
            .into_iter()
            .find(|p| p.is_file())
        {
            Some(p) => {
                found.insert(name, p);
            }
            None => missing.push(name),
        }
    }
    (found, missing)
}

/// [`ENGINE`] plus `extra`, looked up by the single artefact rule (see the module docs).
/// `Err` is the skip reason.
pub fn windows_artifacts(extra: &[&'static str]) -> Result<Artifacts, String> {
    let names: Vec<&'static str> = ENGINE
        .iter()
        .copied()
        .chain(extra.iter().copied())
        .collect();
    let mine = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    if let Some(dir) = std::env::var_os("VFS_WINDOWS_ARTIFACTS").map(PathBuf::from) {
        let (found, missing) = find_in(&dir, &names);
        return if missing.is_empty() {
            Ok(Artifacts(found))
        } else {
            Err(format!(
                "Windows artifacts missing from VFS_WINDOWS_ARTIFACTS={}: {}; \
                 build them with `bin/build-windows{}` and point it at their directory",
                dir.display(),
                missing.join(", "),
                profile_flag()
            ))
        };
    }
    let profile = profile_dir();
    let (found, missing) = find_in(&profile, &names);
    if missing.is_empty() {
        return Ok(Artifacts(found));
    }
    let other_name = if mine == "debug" { "release" } else { "debug" };
    let other = profile.with_file_name(other_name);
    let (_, other_missing) = find_in(&other, &names);
    let hint = if other_missing.is_empty() {
        let other_flag = if other_name == "release" {
            " --release"
        } else {
            ""
        };
        format!(
            "; the {other_name} build has them, so either run `cargo test{other_flag} ...` \
             or build for this {mine} test with `bin/build-windows{}`",
            profile_flag()
        )
    } else {
        format!("; run `bin/build-windows{}`", profile_flag())
    };
    Err(format!(
        "Windows artifacts missing from {} ({mine} build): {}{hint}",
        profile.display(),
        missing.join(", ")
    ))
}

// ---------------------------------------------------------------------------
// Runtime and home
// ---------------------------------------------------------------------------

/// The GE-Proton runtime directory to launch with: `VFS_TEST_PROTON_RUNTIME`, else the
/// newest verified runtime of the environment's aether home (`vfs_proton::Root::from_env`).
/// Only read from, never written. `Err` is the skip reason.
pub fn runtime_dir() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("VFS_TEST_PROTON_RUNTIME") {
        let p = PathBuf::from(p);
        return if p.is_dir() {
            Ok(p)
        } else {
            Err(format!(
                "VFS_TEST_PROTON_RUNTIME={} is not a directory",
                p.display()
            ))
        };
    }
    let root = vfs_proton::Root::from_env().map_err(|e| format!("no aether home: {e}"))?;
    vfs_proton::runtime::installed_dirs(&root)
        .ok()
        .and_then(|v| v.into_iter().next())
        .map(|(_, dir)| dir)
        .ok_or_else(|| {
            format!(
                "no verified GE-Proton runtime under {}: run `cargo run -p vfs-proton -- install`, \
                 or set VFS_HOME to a home that has one, or VFS_TEST_PROTON_RUNTIME to a GE-Proton \
                 directory",
                root.runtimes().display()
            )
        })
}

/// An aether home of the test's own under `target/tmp`, whose `runtimes` holds one symlink to
/// the real runtime ([`runtime_dir`]). Prefixes and sessions land in it, not in the user's
/// real home. It is kept between runs (a booted prefix is reused); `group` names it.
pub fn throwaway_home(group: &str) -> Result<PathBuf, String> {
    let runtime = runtime_dir()?;
    let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("vfs-test-home-{group}"));
    let runtimes = home.join("runtimes");
    std::fs::create_dir_all(&runtimes).unwrap();
    let link = runtimes.join(runtime.file_name().expect("runtime dir name"));
    if std::fs::read_link(&link).ok().as_deref() != Some(runtime.as_path()) {
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&runtime, &link).unwrap();
    }
    Ok(home)
}

/// What a Proton end-to-end test needs from the machine: a home and the artefacts.
pub struct Rig {
    pub home: PathBuf,
    pub art: Artifacts,
}

/// The runtime (as a throwaway home) and the Windows artefacts ([`ENGINE`] plus `fixtures`),
/// or `None` after [`skip`]ping with the reason.
pub fn rig(test: &str, group: &str, fixtures: &[&'static str]) -> Option<Rig> {
    let art = match windows_artifacts(fixtures) {
        Ok(a) => a,
        Err(why) => {
            skip(test, why);
            return None;
        }
    };
    match throwaway_home(group) {
        Ok(home) => Some(Rig { home, art }),
        Err(why) => {
            skip(test, why);
            None
        }
    }
}

/// The Steam client's install directory: `VFS_TEST_STEAM_CLIENT`, else
/// `~/.local/share/Steam`. `Err` is the skip reason.
pub fn steam_client() -> Result<PathBuf, String> {
    let dir = match std::env::var_os("VFS_TEST_STEAM_CLIENT") {
        Some(p) => PathBuf::from(p),
        None => match std::env::var_os("HOME") {
            Some(h) => PathBuf::from(h).join(".local/share/Steam"),
            None => return Err("no Steam client directory: set VFS_TEST_STEAM_CLIENT".into()),
        },
    };
    if dir.is_dir() {
        Ok(dir)
    } else {
        Err(format!(
            "no Steam client directory at {}: install Steam or set VFS_TEST_STEAM_CLIENT",
            dir.display()
        ))
    }
}

/// The `<prefix>key=value` lines of a fixture's log (`steam-probe: `, `nvapi-probe: `).
pub fn probe_lines(log: &str, prefix: &str) -> BTreeMap<String, String> {
    log.lines()
        .filter_map(|l| l.trim().strip_prefix(prefix))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

// ---------------------------------------------------------------------------
// A provider that records what it was asked
// ---------------------------------------------------------------------------

/// Wraps a provider, records the calls made on it, and (unless [`Loud::quiet`]) logs each
/// to stderr as `DIRECTOR: ...`. The log is a run's Director-side transcript, and the record
/// is what turns "the child exited 0" into "the child's bytes came from here".
pub struct Loud {
    inner: Arc<dyn Provider>,
    quiet: bool,
    /// `(op, vpath)` for the path-addressed calls, plus `("read_at", vpath)` resolved back
    /// through `handles`.
    calls: Mutex<Vec<(String, String)>>,
    /// Lowercased vpaths of the opens that succeeded.
    opened: Mutex<Vec<String>>,
    /// Open handles, so a `read_at` (which carries no path) can be attributed.
    handles: Mutex<BTreeMap<Handle, String>>,
}

impl Loud {
    /// Over a [`DiskProvider`] on `root`.
    pub fn new(root: &Path) -> Self {
        Self::over(Arc::new(DiskProvider::new(root)))
    }

    pub fn over(inner: Arc<dyn Provider>) -> Self {
        Loud {
            inner,
            quiet: false,
            calls: Mutex::new(Vec::new()),
            opened: Mutex::new(Vec::new()),
            handles: Mutex::new(BTreeMap::new()),
        }
    }

    /// Record without logging every call.
    pub fn quiet(mut self) -> Self {
        self.quiet = true;
        self
    }

    fn note(&self, op: &str, path: &str, detail: &str) {
        if !self.quiet {
            eprintln!("DIRECTOR: {op} {path:?} {detail}");
        }
        self.calls
            .lock()
            .unwrap()
            .push((op.to_string(), path.to_string()));
    }

    pub fn saw(&self, op: &str, path: &str) -> bool {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .any(|(o, p)| o == op && p == path)
    }

    pub fn transcript(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(o, p)| format!("{o} {p}"))
            .collect()
    }

    /// The (lowercased) vpaths opened successfully.
    pub fn opened(&self) -> Vec<String> {
        self.opened.lock().unwrap().clone()
    }
}

impl Provider for Loud {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        let r = self.inner.getattr(p);
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
        let r = self.inner.readdir(p);
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
        let r = self.inner.open(p, flags);
        if let Ok((h, _, _)) = &r {
            self.handles.lock().unwrap().insert(*h, p.rel.to_string());
            self.opened.lock().unwrap().push(p.rel.to_ascii_lowercase());
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
        let r = self.inner.close(h);
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
        let r = self.inner.read_at(h, offset, buf);
        self.note(
            "read_at",
            &path,
            &format!("fh={h} offset={offset} want={} -> {r:?}", buf.len()),
        );
        r
    }

    fn set_attr(&self, p: VPath, attr: SetAttr) -> Result<(), i32> {
        self.inner.set_attr(p, attr)
    }
}

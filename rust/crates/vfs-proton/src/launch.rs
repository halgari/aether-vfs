//! Launching a Windows target under GE-Proton with the shim injected.
//!
//! # Why the command and the environment are pure functions
//!
//! A real launch needs Wine, so it cannot run in this repo's Windows CI job —
//! but nothing about a launch is *interesting* except the two things a test
//! can check without spawning anything:
//!
//! 1. **The injector's argv is positional.** `vfs-injector <target> <shim>
//!    <payload> <config> <ready> [-- args…]` — swap two of those and every
//!    process still starts; the shim just never attaches, or attaches with the
//!    payload as its config. There is no error to observe.
//! 2. **The environment is the whole handshake.** The shim decides whether it
//!    is configured at all from [`vfs_env`] names it reads inside the Wine
//!    process (see `vfs-shim/src/fuse_client.rs::try_init_from_env`), and two
//!    of those values are silently wrong-by-default rather than absent.
//!
//! So [`command_line`] and [`launch_env`] are separate from [`run`], and the
//! tests at the bottom of this file cover the part where the mistakes live.
//!
//! # The two values that are fatal when defaulted
//!
//! - **`VFS_RING_BYTES` must be the Director's real map size.** The shim
//!   defaults it to 2 MiB. Measured 2026-09-02 against a ~34 MiB ring: a
//!   256 KiB read *passes* (its arena bank happens to land inside the
//!   under-sized view) and only a 4 MiB read fails — while the server logs
//!   every read as answered. Under-mapping is silent at attach and fatal
//!   under load, so the geometry travels in [`WineLaunch`] from the live
//!   `IpcServe` rather than being guessed here.
//! - **`PROTONPATH` must be absolute.** Unset or relative, Proton resolves it
//!   to UMU-Proton — *stock* Valve Proton — which is the silent downgrade this
//!   crate exists to prevent. [`launch_env`] absolutizes it, and [`run`]
//!   refuses to launch a runtime that does not pass
//!   [`verify_ge`](crate::runtime::verify_ge).

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use crate::runtime::verify_ge;

/// Everything one Wine launch needs, with the ring geometry carried
/// explicitly.
///
/// The `PathBuf` fields are **host** (Linux) paths — the injector and the DLLs
/// are passed to `wine` as host paths, which it accepts. The `String` fields
/// (`target`, `virtual_dir`) and `ring_path` are paths **as Wine sees them**
/// (`C:\…`): `target` is resolved by the injected process, and `ring_path` and
/// `virtual_dir` are read by the shim *inside* Wine, where a Linux path means
/// nothing. Use [`Prefix::windows_path`](crate::prefix::Prefix::windows_path)
/// to build them.
#[derive(Debug, Clone)]
pub struct WineLaunch {
    /// The GE-Proton runtime directory (`…/GE-ProtonN-M-x86_64`). Verified
    /// before launch and exported as `PROTONPATH`.
    pub runtime: PathBuf,
    /// The Wine prefix directory, exported as `WINEPREFIX`.
    pub prefix: PathBuf,
    /// Host path to `vfs-injector.exe`.
    pub injector: PathBuf,
    /// Host path to `vfs_shim_dll.dll`.
    pub shim_dll: PathBuf,
    /// Host path to `vfs_payload.dll`.
    pub payload_dll: PathBuf,
    /// The target executable, as Wine sees it (`C:\…`).
    pub target: String,
    /// Host path to the shim config file the injector hands the shim.
    pub config_file: PathBuf,
    /// Host path to the ready file the injector waits on.
    pub ready_file: PathBuf,
    /// The ring file **as Wine sees it** (`C:\…`) — the shim maps it by path.
    pub ring_path: PathBuf,
    /// The Director's real map size. See this module's docs: defaulting this
    /// is silent at attach and fatal under load.
    pub ring_bytes: usize,
    /// Byte offset of the bulk arena within the ring mapping.
    pub arena_offset: usize,
    /// Byte length of the bulk arena.
    pub arena_len: usize,
    /// Inline ring payload capacity, in bytes.
    pub payload_cap: u32,
    /// The managed root as Wine sees it (`C:\…`) — root 0 for the shim.
    pub virtual_dir: String,
    /// Arguments for the target, passed after `--`.
    pub args: Vec<String>,
}

/// Why a launch did not happen, or did not finish cleanly.
#[derive(Debug)]
pub enum LaunchError {
    /// Filesystem or process I/O failed before the child could be waited on.
    Io(io::Error),
    /// `runtime` is not a verified GE-Proton build. Never a warning: launching
    /// anyway means launching on stock Proton, which is the failure this crate
    /// exists to prevent.
    NotGe(String),
    /// `wine` could not be started at all, or exited without an exit code
    /// (killed by a signal). Carries a description including the wine path.
    Spawn(String),
    /// The injector itself failed and the target never ran — its documented
    /// exit codes 2 (bad argv) and 3 (injection failed), from
    /// `vfs-inject/src/bin/vfs-injector.rs`.
    ///
    /// A target that *runs* and exits non-zero is **not** an error: [`run`]
    /// returns its code as `Ok`. Codes 2 and 3 are ambiguous in principle (a
    /// target could pick them too), so this variant carries the code and
    /// loses nothing — whereas reporting `Ok(3)` would hide an injection that
    /// never happened, which is the failure mode worth being loud about.
    NonZeroWine(i32),
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LaunchError::Io(e) => write!(f, "io error: {e}"),
            LaunchError::NotGe(s) => write!(f, "runtime is not GE-Proton: {s}"),
            LaunchError::Spawn(s) => write!(f, "could not run wine: {s}"),
            LaunchError::NonZeroWine(c) => write!(
                f,
                "vfs-injector exited {c} without running the target \
                 (2 = bad argv, 3 = injection failed)"
            ),
        }
    }
}

impl std::error::Error for LaunchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LaunchError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for LaunchError {
    fn from(e: io::Error) -> Self {
        LaunchError::Io(e)
    }
}

/// `wine` inside a GE-Proton runtime directory.
pub fn wine_binary(runtime: &Path) -> PathBuf {
    runtime.join("files").join("bin").join("wine")
}

/// The program and argv for a launch: `wine <injector> <target> <shim>
/// <payload> <config> <ready> [-- args…]`.
///
/// The order is the injector's positional contract (`parse_injector_args`) and
/// must not be rearranged to suit a caller: every permutation starts
/// successfully and fails silently.
pub fn command_line(l: &WineLaunch) -> (String, Vec<String>) {
    let prog = wine_binary(&l.runtime).to_string_lossy().into_owned();
    let mut argv = vec![
        l.injector.to_string_lossy().into_owned(),
        l.target.clone(),
        l.shim_dll.to_string_lossy().into_owned(),
        l.payload_dll.to_string_lossy().into_owned(),
        l.config_file.to_string_lossy().into_owned(),
        l.ready_file.to_string_lossy().into_owned(),
    ];
    if !l.args.is_empty() {
        // The separator is optional for the parser but not for the target: an
        // argument that looks like a path would otherwise be indistinguishable
        // from a sixth positional if the contract ever grows one.
        argv.push("--".to_string());
        argv.extend(l.args.iter().cloned());
    }
    (prog, argv)
}

/// The environment for a launch: Wine's own three, plus exactly the `VFS_*`
/// names the shim's `try_init_from_env` consults in file-backed mode.
///
/// Mined from `vfs-shim/src/fuse_client.rs` rather than from memory:
/// `VFS_RING_PATH` (which *wins* over `VFS_RING_SECTION`), `VFS_RING_BYTES`,
/// `VFS_RING_PAYLOAD_CAP`, `VFS_ARENA_LEN` and `VFS_VIRTUAL_DIR` — the last
/// being the only one with no default, because "which tree is virtualised"
/// has no sensible guess.
///
/// Deliberately **not** set:
/// - `VFS_RING_SECTION` — no named section exists in file-backed mode, and
///   `VFS_RING_PATH` would shadow it anyway.
/// - `VFS_SERVER_EV` / `VFS_CLIENT_EV` — a Wine event cannot wake a native
///   Linux Director, so `connect_source` does not even consult them on the
///   file path (it passes a null event on purpose; the Director spins).
/// - `VFS_VIRTUAL_ROOTS` — single-root launches only, for now.
///
/// `VFS_ARENA_OFFSET` *is* exported even though today's client derives the
/// offset from the ring header: it is what the working `vfs-serve-fb` run
/// published, it is what the Windows `IpcServe::apply_env` sets, and a
/// geometry field that exists at one end and not the other is exactly the
/// drift `vfs-env` was created to stop.
pub fn launch_env(l: &WineLaunch) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("WINEPREFIX".to_string(), path_string(&l.prefix));
    // Absolutized, not passed through: a relative `PROTONPATH` resolves to
    // UMU-Proton (stock Valve Proton), and that downgrade produces no error.
    env.insert("PROTONPATH".to_string(), path_string(&absolute(&l.runtime)));
    // Mono and Gecko prompts would otherwise block a launch on a fresh prefix.
    env.insert("WINEDLLOVERRIDES".to_string(), "mscoree=d;mshtml=d".to_string());
    env.insert("WINEDEBUG".to_string(), "-all".to_string());

    env.insert(vfs_env::RING_PATH.to_string(), path_string(&l.ring_path));
    env.insert(vfs_env::RING_BYTES.to_string(), l.ring_bytes.to_string());
    env.insert(vfs_env::RING_PAYLOAD_CAP.to_string(), l.payload_cap.to_string());
    env.insert(vfs_env::ARENA_OFFSET.to_string(), l.arena_offset.to_string());
    env.insert(vfs_env::ARENA_LEN.to_string(), l.arena_len.to_string());
    env.insert(vfs_env::VIRTUAL_DIR.to_string(), l.virtual_dir.clone());
    env
}

/// Spawns the launch, waits for it, and returns the target's exit code.
///
/// GE is verified first: `PROTONPATH` pointing at a non-GE runtime is a hard
/// error ([`LaunchError::NotGe`]), never a fallback.
pub fn run(l: &WineLaunch) -> Result<i32, LaunchError> {
    verify_ge(&l.runtime).map_err(|e| LaunchError::NotGe(e.to_string()))?;

    let (prog, argv) = command_line(l);
    let status = std::process::Command::new(&prog)
        .args(&argv)
        .envs(launch_env(l))
        .status()
        .map_err(|e| LaunchError::Spawn(format!("{prog}: {e}")))?;

    match status.code() {
        // 2 and 3 are the injector's own "the target never ran" exits.
        Some(code @ (2 | 3)) => Err(LaunchError::NonZeroWine(code)),
        Some(code) => Ok(code),
        None => Err(LaunchError::Spawn(format!(
            "{prog} exited without a code (signalled): {status}"
        ))),
    }
}

fn path_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// A relative path made absolute against the current directory. Not
/// `canonicalize`: the runtime directory must not have to exist yet for the
/// *string* to be right, and resolving symlinks would rename a runtime a user
/// deliberately linked.
fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(p),
        Err(_) => p.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An absolute path on whichever host is running the test. `/x` is not
    /// absolute on Windows and `C:\x` is not absolute on Linux, and the
    /// `PROTONPATH` test below is about absoluteness itself.
    fn abs(rest: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!(r"C:\ge\{rest}"))
        } else {
            PathBuf::from(format!("/ge/{rest}"))
        }
    }

    fn sample() -> WineLaunch {
        WineLaunch {
            runtime: abs("GE-Proton11-6-x86_64"),
            prefix: abs("probe-prefix"),
            injector: abs("bin/vfs-injector.exe"),
            shim_dll: abs("bin/vfs_shim_dll.dll"),
            payload_dll: abs("bin/vfs_payload.dll"),
            target: r"C:\probe\target.exe".to_string(),
            config_file: abs("state/shim.cfg"),
            ready_file: abs("state/ready.txt"),
            ring_path: PathBuf::from(r"C:\probe\ring.bin"),
            ring_bytes: 33_751_040,
            arena_offset: 65_536,
            arena_len: 33_554_432,
            payload_cap: 1_048_576,
            virtual_dir: r"C:\probe\managed".to_string(),
            args: vec!["-arg1".to_string(), "arg2".to_string()],
        }
    }

    #[test]
    fn argv_is_the_injector_contract_in_order() {
        // vfs-injector <target_exe> <shim_dll> <payload_dll> <config> <ready>
        // [-- args]. Order is positional, so a swap is silent and this is the
        // only thing that catches it.
        let (prog, argv) = command_line(&sample());
        assert!(prog.ends_with("wine"), "{prog}");
        assert_eq!(argv[0], sample().injector.to_string_lossy());
        assert_eq!(argv[1], sample().target);
        assert_eq!(argv[2], sample().shim_dll.to_string_lossy());
        assert_eq!(argv[3], sample().payload_dll.to_string_lossy());
        assert_eq!(argv[4], sample().config_file.to_string_lossy());
        assert_eq!(argv[5], sample().ready_file.to_string_lossy());
    }

    #[test]
    fn env_carries_the_real_ring_size_not_a_default() {
        let mut l = sample();
        l.ring_bytes = 33_751_040;
        let env = launch_env(&l);
        assert_eq!(env.get("VFS_RING_BYTES").map(String::as_str), Some("33751040"));
        assert!(env.contains_key("VFS_ARENA_LEN"));
        assert!(env.contains_key("VFS_ARENA_OFFSET"));
        assert!(env.contains_key("VFS_RING_PAYLOAD_CAP"));
    }

    #[test]
    fn protonpath_is_absolute_because_a_relative_one_silently_means_stock() {
        let env = launch_env(&sample());
        let p = env.get("PROTONPATH").expect("PROTONPATH");
        assert!(std::path::Path::new(p).is_absolute(), "{p}");
    }

    #[test]
    fn a_relative_runtime_is_absolutized_rather_than_passed_through() {
        // The reason `launch_env` absolutizes at all: a caller holding a
        // relative runtime path would otherwise export a `PROTONPATH` that
        // resolves to stock Proton, with nothing to observe.
        let mut l = sample();
        l.runtime = PathBuf::from("runtimes/GE-Proton11-6-x86_64");
        let env = launch_env(&l);
        let p = env.get("PROTONPATH").expect("PROTONPATH");
        assert!(Path::new(p).is_absolute(), "{p}");
        assert!(p.ends_with("GE-Proton11-6-x86_64"), "{p}");
    }

    #[test]
    fn target_args_follow_a_separator_and_keep_their_order() {
        let (_, argv) = command_line(&sample());
        assert_eq!(&argv[6..], &["--", "-arg1", "arg2"]);
    }

    #[test]
    fn no_separator_is_emitted_when_there_are_no_target_args() {
        let mut l = sample();
        l.args.clear();
        let (_, argv) = command_line(&l);
        assert_eq!(argv.len(), 6, "{argv:?}");
    }

    #[test]
    fn the_ring_is_named_by_its_wine_path_and_no_section_is_offered() {
        let env = launch_env(&sample());
        assert_eq!(
            env.get(vfs_env::RING_PATH).map(String::as_str),
            Some(r"C:\probe\ring.bin"),
            "the shim maps the ring inside Wine, so this must be the C: form"
        );
        // `VFS_RING_PATH` wins over `VFS_RING_SECTION` in the shim, and no
        // section exists here; setting one would only mislead a reader.
        assert!(!env.contains_key(vfs_env::RING_SECTION));
        // A Wine event cannot wake a native Linux director, and the shim's
        // file-backed path does not consult these at all.
        assert!(!env.contains_key(vfs_env::SERVER_EV));
        assert!(!env.contains_key(vfs_env::CLIENT_EV));
    }

    #[test]
    fn the_managed_root_is_always_set_because_the_shim_has_no_default_for_it() {
        let env = launch_env(&sample());
        assert_eq!(
            env.get(vfs_env::VIRTUAL_DIR).map(String::as_str),
            Some(r"C:\probe\managed"),
        );
        assert_eq!(
            env.get("WINEPREFIX").map(String::as_str),
            Some(path_string(&sample().prefix).as_str()),
        );
        assert_eq!(
            env.get("WINEDLLOVERRIDES").map(String::as_str),
            Some("mscoree=d;mshtml=d"),
        );
    }

    #[test]
    fn a_non_ge_runtime_is_refused_before_anything_is_spawned() {
        let dir = std::env::temp_dir().join(format!("vfs-launch-notge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("version"), "1700000000 proton-9.0-4\n").unwrap();
        let mut l = sample();
        l.runtime = dir.clone();
        // No wine exists under this directory, so reaching a spawn at all
        // would surface as `Spawn`; `NotGe` proves the gate ran first.
        match run(&l) {
            Err(LaunchError::NotGe(msg)) => assert!(msg.contains("proton-9.0-4"), "{msg}"),
            other => panic!("expected NotGe, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wine_comes_from_inside_the_runtime_not_the_host_path() {
        // A host `wine` on `PATH` is not the verified GE build, and using it
        // would make the GE gate decorative.
        let w = wine_binary(Path::new(&abs("GE-Proton11-6-x86_64")));
        assert!(w.ends_with(Path::new("files").join("bin").join("wine")), "{}", w.display());
    }
}

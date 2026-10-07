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
use crate::steam::SteamSide;

mod env;
mod injector;

use env::stale_env;
pub use env::{
    check_extra_env, is_reserved_env, launch_env, merge_dll_overrides, BASE_DLL_OVERRIDES,
    DEFAULT_WINEDEBUG,
};
pub use injector::{describe_injector_error, injector_error_path};

/// The host files a launch hands the injector.
#[derive(Debug, Clone)]
pub struct LaunchFiles {
    /// Host path to `vfs-injector.exe`.
    pub injector: PathBuf,
    /// Host path to `vfs_shim_dll.dll`.
    pub shim_dll: PathBuf,
    /// Host path to `vfs_payload.dll`.
    pub payload_dll: PathBuf,
    /// Host path to the shim config file the injector hands the shim.
    pub config_file: PathBuf,
    /// Host path to the ready file the injector waits on.
    pub ready_file: PathBuf,
}

/// The ring a launch maps, as [`WineLaunch`] carries it.
#[derive(Debug, Clone)]
pub struct RingGeometry {
    /// The ring file **as Wine sees it** (`C:\…`).
    pub path: PathBuf,
    /// The same file as a host path, for [`spawn`]'s length check; `None`
    /// skips it.
    pub host_path: Option<PathBuf>,
    /// The Director's real map size.
    pub bytes: usize,
    /// Byte offset of the bulk arena within the ring mapping.
    pub arena_offset: usize,
    /// Byte length of the bulk arena.
    pub arena_len: usize,
    /// Inline ring payload capacity, in bytes.
    pub payload_cap: u32,
}

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
    /// The same ring file as a **host** path, so [`spawn`] can check its real
    /// length against [`ring_bytes`](Self::ring_bytes) (`ring_path` is the Wine
    /// spelling and means nothing on the host). `None` skips that check.
    pub ring_host_path: Option<PathBuf>,
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
    /// Roots beyond root 0, `(id, location as Wine sees it)`. Sent as
    /// `VFS_VIRTUAL_ROOTS` in the format `IpcServe::apply_env_roots` writes on
    /// Windows, so the shim's parser is shared.
    pub virtual_roots: Vec<(u32, String)>,
    /// Arguments for the target, passed after `--`.
    pub args: Vec<String>,
    /// The host's own variables for the child, applied over [`launch_env`]'s:
    /// `WINEDLLOVERRIDES` is merged with [`BASE_DLL_OVERRIDES`] (see
    /// [`merge_dll_overrides`]), `LD_LIBRARY_PATH` and `WINEDLLPATH` go after
    /// the runtime's own directories, `WINEDEBUG` replaces
    /// [`DEFAULT_WINEDEBUG`], and anything else is added. A name the launch's own handshake uses is
    /// refused ([`LaunchError::ReservedEnv`]). Child-only: nothing here is
    /// written into this process's environment.
    pub extra_env: BTreeMap<String, String>,
    /// The target's working directory as it sees it (`C:\…`), sent to the
    /// injector as [`vfs_env::INJECT_CWD`]. `None`: the target inherits the
    /// injector's directory, which is `wine`'s host cwd seen through `Z:`.
    pub cwd: Option<String>,
    /// Seconds the injector waits for the shim to report ready, sent as
    /// [`vfs_env::READY_TIMEOUT_SECS`]. `None`: the injector's default (180),
    /// or whatever this process's environment already says.
    pub ready_timeout_secs: Option<u64>,
    /// Host path the `wine` child's stdout **and** stderr are written to.
    /// Created (parent directories too) and truncated by [`spawn`], before
    /// `wine` starts; an error doing so fails the launch
    /// ([`LaunchError::Io`]). Everything `wine` starts inherits the two
    /// descriptors — `wineserver` when this launch starts it, the injector,
    /// the target and whatever the target spawns — so Wine's own `err:`/`warn:`
    /// channels ([`DEFAULT_WINEDEBUG`] silences them; set `WINEDEBUG` in
    /// [`extra_env`](Self::extra_env)) and its unhandled-exception report land
    /// here, including output written after `wine` itself has exited.
    /// `None`: both streams are inherited from this process, as before.
    pub log_file: Option<PathBuf>,
    /// The Steam side of the launch (see [`crate::steam`] and
    /// [`SteamSide`]): nothing ([`SteamSide::Untouched`], as before), only
    /// clearing a stale helper pid ([`SteamSide::Off`]), or Proton's Steam
    /// helper started by the injector before the target plus the environment
    /// it and the target's Steam API read ([`SteamSide::Helper`]).
    pub steam: SteamSide,
    /// Lines [`spawn`] writes before `wine` starts, each on its own line: at
    /// the top of [`log_file`](Self::log_file), or to this process's stderr
    /// when there is none — where the launch's own output goes.
    pub notes: Vec<String>,
    /// NVIDIA NVAPI/NGX for this launch ([`crate::nvapi::setup`]), already
    /// installed into the prefix: [`launch_env`] adds its environment, puts
    /// [`NVAPI_OVERRIDES`](crate::nvapi::NVAPI_OVERRIDES) under the caller's
    /// `WINEDLLOVERRIDES` and wine-nvml first in `WINEDLLPATH`, as the
    /// `proton` script does. `None`: none of that.
    pub nvapi: Option<crate::nvapi::Setup>,
    /// The session has a registry layer attached: [`launch_env`] sets
    /// [`vfs_env::REGISTRY`] so the shim installs its registry hooks.
    pub registry: bool,
}

impl WineLaunch {
    /// A launch with everything required and nothing optional: no extra
    /// roots, arguments, environment, working directory, timeout or log,
    /// [`SteamSide::Untouched`], no notes, no NVAPI, no registry layer. Set
    /// the public fields for the rest.
    pub fn new(
        runtime: PathBuf,
        prefix: PathBuf,
        target: String,
        virtual_dir: String,
        files: LaunchFiles,
        ring: RingGeometry,
    ) -> WineLaunch {
        WineLaunch {
            runtime,
            prefix,
            injector: files.injector,
            shim_dll: files.shim_dll,
            payload_dll: files.payload_dll,
            target,
            config_file: files.config_file,
            ready_file: files.ready_file,
            ring_path: ring.path,
            ring_host_path: ring.host_path,
            ring_bytes: ring.bytes,
            arena_offset: ring.arena_offset,
            arena_len: ring.arena_len,
            payload_cap: ring.payload_cap,
            virtual_dir,
            virtual_roots: Vec::new(),
            args: Vec::new(),
            extra_env: BTreeMap::new(),
            cwd: None,
            ready_timeout_secs: None,
            log_file: None,
            steam: SteamSide::Untouched,
            notes: Vec::new(),
            nvapi: None,
            registry: false,
        }
    }
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
    /// The ring geometry cannot describe the ring, so the child would attach
    /// cleanly and fail only under load. See `check_geometry`.
    Geometry(String),
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
    /// [`WineLaunch::extra_env`] names a variable the launch sets itself.
    ReservedEnv(String),
    /// The injector failed and said why (its report beside the ready file);
    /// carries [`describe_injector_error`]'s rendering of it.
    Injector(String),
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LaunchError::Io(e) => write!(f, "io error: {e}"),
            LaunchError::NotGe(s) => write!(f, "runtime is not GE-Proton: {s}"),
            LaunchError::Spawn(s) => write!(f, "could not run wine: {s}"),
            LaunchError::Geometry(s) => write!(f, "ring geometry is inconsistent: {s}"),
            LaunchError::NonZeroWine(c) => write!(
                f,
                "vfs-injector exited {c} without running the target \
                 (2 = bad argv, 3 = injection failed)"
            ),
            LaunchError::ReservedEnv(k) => write!(
                f,
                "{k} is part of the launch's own handshake and cannot be set through the \
                 launch environment"
            ),
            LaunchError::Injector(s) => write!(f, "vfs-injector did not run the target: {s}"),
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

/// Spawns the launch, waits for it, and returns the target's exit code.
///
/// GE is verified first: `PROTONPATH` pointing at a non-GE runtime is a hard
/// error ([`LaunchError::NotGe`]), never a fallback.
pub fn run(l: &WineLaunch) -> Result<i32, LaunchError> {
    let mut child = spawn(l)?;
    let status = child.wait()?;
    finish(l, status)
}

/// Spawns the launch and returns without waiting: every check [`run`]
/// makes, then `wine`. [`finish`] turns the child's exit status into
/// [`run`]'s result.
pub fn spawn(l: &WineLaunch) -> Result<std::process::Child, LaunchError> {
    check_extra_env(&l.extra_env)?;
    verify_ge(&l.runtime).map_err(|e| LaunchError::NotGe(e.to_string()))?;
    check_geometry(l)?;
    // A report left by an earlier launch would be read as this one's.
    for stale in [
        injector_error_path(&l.ready_file),
        crate::steam::helper_report_path(&l.ready_file),
    ] {
        match std::fs::remove_file(stale) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }

    let (prog, argv) = command_line(l);
    let mut cmd = std::process::Command::new(&prog);
    let env = launch_env(l);
    cmd.args(&argv).envs(&env);
    if let Some(log) = &l.log_file {
        let mut file = open_log(log)
            .map_err(|e| io::Error::new(e.kind(), format!("wine log {}: {e}", log.display())))?;
        for note in &l.notes {
            use io::Write;
            writeln!(file, "{note}").map_err(|e| {
                io::Error::new(e.kind(), format!("wine log {}: {e}", log.display()))
            })?;
        }
        // One open file description for both streams, so their writes share
        // an offset and interleave instead of overwriting each other.
        cmd.stdout(file.try_clone()?).stderr(file);
    } else {
        for note in &l.notes {
            eprintln!("{note}");
        }
    }
    // Explicitly unset the transport variables this launch does not use.
    //
    // `Command::envs` *adds to* the parent environment, so a host process that
    // has served a named-section session earlier still has `VFS_RING_SECTION`
    // and friends set, and the child would inherit them. `VFS_RING_PATH` wins
    // over `VFS_RING_SECTION` by design, so the ring itself is safe — but
    // `VFS_VIRTUAL_ROOTS` is not adjudicated that way, and a stale value would
    // point the child's root map somewhere this session never chose. That is
    // the same stale-value hazard `IpcServe::apply_env_roots` clears with
    // `remove_var`, and it applies here for the same reason.
    for stale in stale_env(&env) {
        cmd.env_remove(stale);
    }
    crate::process::spawn_retrying_busy(&mut cmd)
        .map_err(|e| LaunchError::Spawn(format!("{prog}: {e}")))
}

/// Creates `path` (and its parent directories) for a launch's output,
/// truncating whatever an earlier launch left there.
fn open_log(path: &Path) -> io::Result<std::fs::File> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::File::create(path)
}

/// How a launch [`spawn`] started ended: the target's exit code, or why the
/// injector never ran it (its report, if it wrote one, else its exit code).
pub fn finish(l: &WineLaunch, status: std::process::ExitStatus) -> Result<i32, LaunchError> {
    match status.code() {
        // 2 and 3 are the injector's own "the target never ran" exits.
        Some(code @ (2 | 3)) => {
            match std::fs::read_to_string(injector_error_path(&l.ready_file)) {
                Ok(raw) if !raw.trim().is_empty() => {
                    Err(LaunchError::Injector(describe_injector_error(&raw)))
                }
                _ => Err(LaunchError::NonZeroWine(code)),
            }
        }
        Some(code) => Ok(code),
        None => Err(LaunchError::Spawn(format!(
            "{} exited without a code (signalled): {status}",
            wine_binary(&l.runtime).display()
        ))),
    }
}

/// Refuse a launch whose ring geometry cannot describe the ring.
///
/// Under-mapping is the failure this exists for, and it is nasty because it is
/// **silent at attach**: `ring::open` succeeds — the header is present and
/// valid — and only a read whose arena bank falls outside the mapped view
/// fails. Measured 2026-09-02 with a 2 MiB view over a ~34 MiB ring: a 256 KiB
/// read passed because its bank happened to land inside, and only a 4 MiB read
/// failed, with the server logging every read answered. A game would attach
/// cleanly and then die under load.
///
/// So the arithmetic is checked before anything is spawned, and the ring file's
/// real length is checked too — `ring_bytes` describing more than the file
/// holds is the same bug wearing a different hat.
fn check_geometry(l: &WineLaunch) -> Result<(), LaunchError> {
    let need = l.arena_offset.saturating_add(l.arena_len);
    if need > l.ring_bytes {
        return Err(LaunchError::Geometry(format!(
            "arena_offset {} + arena_len {} = {need} exceeds ring_bytes {}; the child would \
             map a view too small to hold the arena and fail only under load",
            l.arena_offset, l.arena_len, l.ring_bytes
        )));
    }
    let Some(host_ring) = &l.ring_host_path else {
        return Ok(());
    };
    match std::fs::metadata(host_ring) {
        Ok(m) if (m.len() as usize) < l.ring_bytes => Err(LaunchError::Geometry(format!(
            "ring file {} is {} bytes but ring_bytes is {}; mapping past the end of a file \
             faults on touch rather than failing at map time",
            host_ring.display(),
            m.len(),
            l.ring_bytes
        ))),
        // A missing ring file is the caller's sequencing error, not a geometry
        // one: `serve()` creates it. Let the launch proceed and let the child
        // report the open failure, which names the path.
        _ => Ok(()),
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

    #[test]
    fn a_view_too_small_for_the_arena_is_refused_before_spawning() {
        // The whole point: this must fail up front, not attach cleanly and die
        // on the first read whose bank falls outside the view.
        let mut l = sample();
        l.ring_bytes = 2 * 1024 * 1024;
        l.arena_offset = 132_136;
        l.arena_len = 33_554_432;
        match check_geometry(&l) {
            Err(LaunchError::Geometry(m)) => {
                assert!(m.contains("exceeds ring_bytes"), "{m}");
            }
            other => panic!("expected a Geometry refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_consistent_geometry_passes() {
        let mut l = sample();
        l.ring_bytes = 33_751_040;
        l.arena_offset = 132_136;
        l.arena_len = 33_554_432;
        assert!(check_geometry(&l).is_ok());
    }

    #[test]
    fn a_ring_file_shorter_than_ring_bytes_is_refused() {
        // Mapping past the end of a file faults on touch rather than failing at
        // map time, so a short file is the same hazard as a small view.
        let mut l = sample();
        let p = crate::test_tmp::dir().join(format!("vfs-launch-short-{}.bin", std::process::id()));
        std::fs::write(&p, [0u8; 128]).unwrap();
        l.ring_host_path = Some(p.clone());
        l.ring_bytes = 64 * 1024;
        l.arena_offset = 0;
        l.arena_len = 0;
        match check_geometry(&l) {
            Err(LaunchError::Geometry(m)) => assert!(m.contains("but ring_bytes is"), "{m}"),
            other => panic!("expected a Geometry refusal, got {other:?}"),
        }
        let _ = std::fs::remove_file(&p);
    }

    /// An absolute path on whichever host is running the test. `/x` is not
    /// absolute on Windows and `C:\x` is not absolute on Linux, and the
    /// `PROTONPATH` test below is about absoluteness itself.
    pub(super) fn abs(rest: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!(r"C:\ge\{rest}"))
        } else {
            PathBuf::from(format!("/ge/{rest}"))
        }
    }

    pub(super) fn sample() -> WineLaunch {
        let mut l = WineLaunch::new(
            abs("GE-Proton11-6-x86_64"),
            abs("probe-prefix"),
            r"C:\probe\target.exe".to_string(),
            r"C:\probe\managed".to_string(),
            LaunchFiles {
                injector: abs("bin/vfs-injector.exe"),
                shim_dll: abs("bin/vfs_shim_dll.dll"),
                payload_dll: abs("bin/vfs_payload.dll"),
                config_file: abs("state/shim.cfg"),
                ready_file: abs("state/ready.txt"),
            },
            RingGeometry {
                path: PathBuf::from(r"C:\probe\ring.bin"),
                host_path: None,
                bytes: 33_751_040,
                arena_offset: 65_536,
                arena_len: 33_554_432,
                payload_cap: 1_048_576,
            },
        );
        l.args = vec!["-arg1".to_string(), "arg2".to_string()];
        l
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
    fn a_cwd_travels_to_the_injector() {
        let mut l = sample();
        l.cwd = Some(r"C:\Games\Skyrim".to_string());
        assert_eq!(launch_env(&l)[vfs_env::INJECT_CWD], r"C:\Games\Skyrim");
    }

    #[test]
    fn a_ready_timeout_travels_to_the_injector_and_beats_extra_env() {
        let mut l = sample();
        l.ready_timeout_secs = Some(0);
        l.extra_env.insert(vfs_env::READY_TIMEOUT_SECS.to_string(), "5".to_string());
        assert_eq!(launch_env(&l)[vfs_env::READY_TIMEOUT_SECS], "1", "the field wins, and 0 means 1");
    }

    #[cfg(unix)]
    fn exit_status(code: i32) -> std::process::ExitStatus {
        std::process::Command::new("sh").arg("-c").arg(format!("exit {code}")).status().unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn finish_maps_exits_and_reads_the_injector_report() {
        let dir = crate::test_tmp::dir().join(format!("vfs-launch-finish-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut l = sample();
        l.ready_file = dir.join("ready.flag");
        assert_eq!(finish(&l, exit_status(0)).unwrap(), 0);
        assert_eq!(finish(&l, exit_status(7)).unwrap(), 7, "a target's own code is Ok");
        assert!(matches!(finish(&l, exit_status(3)), Err(LaunchError::NonZeroWine(3))));
        std::fs::write(injector_error_path(&l.ready_file), "target-exited:0xc0000135").unwrap();
        match finish(&l, exit_status(3)) {
            Err(LaunchError::Injector(m)) => assert!(m.contains("STATUS_DLL_NOT_FOUND"), "{m}"),
            other => panic!("expected Injector, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_reserved_name_in_extra_env_is_refused_before_anything_else() {
        for k in [
            "WINEPREFIX",
            "PROTONPATH",
            vfs_env::RING_PATH,
            vfs_env::VIRTUAL_DIR,
            vfs_env::INJECT_CWD,
            vfs_env::REGISTRY,
            vfs_env::FUSE_CFG,
        ] {
            assert!(is_reserved_env(k), "{k}");
        }
        for k in vfs_env::handshake::all() {
            assert!(is_reserved_env(k), "{k}");
        }
        assert!(is_reserved_env("vfs_virtual_dir"), "Wine's environment names ignore case");
        assert!(is_reserved_env("WinePrefix"));
        assert!(!is_reserved_env("VFS_FIXTURE_PATH"), "a fixture's own switches pass through");
        assert!(!is_reserved_env(vfs_env::READY_TIMEOUT_SECS));
        let mut l = sample(); // its runtime does not exist: NotGe would come first
        l.extra_env.insert(vfs_env::RING_PATH.to_string(), "C:\\elsewhere".to_string());
        match run(&l) {
            Err(LaunchError::ReservedEnv(k)) => assert_eq!(k, vfs_env::RING_PATH),
            other => panic!("expected ReservedEnv, got {other:?}"),
        }
        assert!(!launch_env(&l)[vfs_env::RING_PATH].contains("elsewhere"));
    }

    #[test]
    fn a_non_ge_runtime_is_refused_before_anything_is_spawned() {
        let dir = crate::test_tmp::dir().join(format!("vfs-launch-notge-{}", std::process::id()));
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

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
    /// Roots beyond root 0, `(id, location as Wine sees it)`. Sent as
    /// `VFS_VIRTUAL_ROOTS` in the format `IpcServe::apply_env_roots` writes on
    /// Windows, so the shim's parser is shared.
    pub virtual_roots: Vec<(u32, String)>,
    /// Arguments for the target, passed after `--`.
    pub args: Vec<String>,
    /// The host's own variables for the child, applied over [`launch_env`]'s:
    /// `WINEDLLOVERRIDES` is merged with [`BASE_DLL_OVERRIDES`] (see
    /// [`merge_dll_overrides`]), `WINEDEBUG` replaces [`DEFAULT_WINEDEBUG`],
    /// and anything else is added. A name the launch's own handshake uses is
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
}

/// `WINEDLLOVERRIDES` every launch carries: Mono and Gecko prompts would
/// otherwise block a launch on a fresh prefix.
pub const BASE_DLL_OVERRIDES: &str = "mscoree=d;mshtml=d";
/// `WINEDEBUG` unless the host sets its own.
pub const DEFAULT_WINEDEBUG: &str = "-all";

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
///
/// `VFS_VIRTUAL_ROOTS` is set iff there are extra roots.
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
    env.insert("WINEDLLOVERRIDES".to_string(), BASE_DLL_OVERRIDES.to_string());
    env.insert("WINEDEBUG".to_string(), DEFAULT_WINEDEBUG.to_string());

    env.insert(vfs_env::RING_PATH.to_string(), path_string(&l.ring_path));
    env.insert(vfs_env::RING_BYTES.to_string(), l.ring_bytes.to_string());
    env.insert(vfs_env::RING_PAYLOAD_CAP.to_string(), l.payload_cap.to_string());
    env.insert(vfs_env::ARENA_OFFSET.to_string(), l.arena_offset.to_string());
    env.insert(vfs_env::ARENA_LEN.to_string(), l.arena_len.to_string());
    env.insert(vfs_env::VIRTUAL_DIR.to_string(), l.virtual_dir.clone());
    if !l.virtual_roots.is_empty() {
        let spec = l
            .virtual_roots
            .iter()
            .map(|(id, loc)| format!("{id}={loc}"))
            .collect::<Vec<_>>()
            .join(";");
        env.insert(vfs_env::VIRTUAL_ROOTS.to_string(), spec);
    }

    if let Some(cwd) = &l.cwd {
        env.insert(vfs_env::INJECT_CWD.to_string(), cwd.clone());
    }

    for (k, v) in &l.extra_env {
        if is_reserved_env(k) {
            continue; // refused by `check_extra_env` before any spawn
        }
        let v = if k == "WINEDLLOVERRIDES" {
            merge_dll_overrides(BASE_DLL_OVERRIDES, v)
        } else {
            v.clone()
        };
        env.insert(k.clone(), v);
    }

    // After `extra_env`: an explicit timeout on the launch beats an inherited
    // or host-supplied one.
    if let Some(secs) = l.ready_timeout_secs {
        env.insert(vfs_env::READY_TIMEOUT_SECS.to_string(), secs.max(1).to_string());
    }
    env
}

/// Whether `name` is one the launch sets (or clears) itself, and so one
/// [`WineLaunch::extra_env`] may not: the prefix, the runtime, and every
/// handshake name the shim or injector reads to find this session.
pub fn is_reserved_env(name: &str) -> bool {
    matches!(name, "WINEPREFIX" | "PROTONPATH")
        || [
            vfs_env::RING_PATH,
            vfs_env::RING_SECTION,
            vfs_env::RING_BYTES,
            vfs_env::RING_PAYLOAD_CAP,
            vfs_env::ARENA_OFFSET,
            vfs_env::ARENA_LEN,
            vfs_env::SERVER_EV,
            vfs_env::CLIENT_EV,
            vfs_env::VIRTUAL_DIR,
            vfs_env::VIRTUAL_ROOTS,
            vfs_env::INJECT_CWD,
        ]
        .contains(&name)
}

/// Refuses an `extra_env` that names a reserved variable ([`is_reserved_env`]).
pub fn check_extra_env(extra: &BTreeMap<String, String>) -> Result<(), LaunchError> {
    match extra.keys().find(|k| is_reserved_env(k)) {
        Some(k) => Err(LaunchError::ReservedEnv(k.clone())),
        None => Ok(()),
    }
}

/// `base` and `extra` as one `WINEDLLOVERRIDES` value, `extra` winning per
/// DLL. Entries are `;`-separated `names=mode`, where `names` may list
/// several DLLs with `,` and `mode` may itself contain `,` (`n,b`); each DLL
/// becomes its own `dll=mode` entry, in first-seen order, matched ASCII
/// case-insensitively. An entry without `=` is kept verbatim. Wine gives no
/// order guarantee between two entries for one DLL, so the merge must leave
/// exactly one.
pub fn merge_dll_overrides(base: &str, extra: &str) -> String {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut put = |name: &str, entry: String| {
        let key = name.to_ascii_lowercase();
        match out.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = entry,
            None => out.push((key, entry)),
        }
    };
    for src in [base, extra] {
        for entry in src.split(';').map(str::trim).filter(|e| !e.is_empty()) {
            match entry.split_once('=') {
                Some((names, mode)) => {
                    for name in names.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                        put(name, format!("{name}={}", mode.trim()));
                    }
                }
                None => put(entry, entry.to_string()),
            }
        }
    }
    out.into_iter().map(|(_, e)| e).collect::<Vec<_>>().join(";")
}

/// Where the injector reports why it failed: the ready file's path plus
/// [`vfs_env::INJECTOR_ERROR_SUFFIX`].
pub fn injector_error_path(ready_file: &Path) -> PathBuf {
    let mut s = ready_file.as_os_str().to_owned();
    s.push(vfs_env::INJECTOR_ERROR_SUFFIX);
    PathBuf::from(s)
}

/// A readable account of the injector's one-line failure report.
pub fn describe_injector_error(raw: &str) -> String {
    let raw = raw.trim();
    if let Some(code) = raw.strip_prefix(vfs_env::INJECTOR_TARGET_EXITED_PREFIX) {
        let hint = match u32::from_str_radix(code.trim_start_matches("0x"), 16) {
            Ok(0xC000_0135) => {
                " (STATUS_DLL_NOT_FOUND: a DLL the program imports is missing — stage it \
                 (stage_also / stage_fallback_dirs), or launch in a Proton-initialized prefix, \
                 which carries the DirectX and Visual C++ redistributables)"
            }
            Ok(0xC000_007B) => {
                " (STATUS_INVALID_IMAGE_FORMAT: an imported DLL is not a PE of the right \
                 architecture)"
            }
            Ok(0xC000_0142) => " (STATUS_DLL_INIT_FAILED: a DLL failed to initialise)",
            _ => "",
        };
        return format!("the target exited with {code}{hint} before the shim reported ready");
    }
    if let Some(secs) = raw.strip_prefix(vfs_env::INJECTOR_READY_TIMEOUT_PREFIX) {
        return format!(
            "the shim did not report ready within {secs} s — the target is hung or still \
             starting; raise the ready timeout if a cold prefix is this slow"
        );
    }
    format!(
        "injection failed: {}",
        raw.strip_prefix(vfs_env::INJECTOR_FAILED_PREFIX).unwrap_or(raw)
    )
}

/// Spawns the launch, waits for it, and returns the target's exit code.
///
/// GE is verified first: `PROTONPATH` pointing at a non-GE runtime is a hard
/// error ([`LaunchError::NotGe`]), never a fallback.
pub fn run(l: &WineLaunch) -> Result<i32, LaunchError> {
    check_extra_env(&l.extra_env)?;
    verify_ge(&l.runtime).map_err(|e| LaunchError::NotGe(e.to_string()))?;
    check_geometry(l)?;
    // A report left by an earlier launch would be read as this one's.
    match std::fs::remove_file(injector_error_path(&l.ready_file)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }

    let (prog, argv) = command_line(l);
    let mut cmd = std::process::Command::new(&prog);
    let env = launch_env(l);
    cmd.args(&argv).envs(&env);
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
    for stale in [
        "VFS_RING_SECTION",
        "VFS_SERVER_EV",
        "VFS_CLIENT_EV",
        "VFS_VIRTUAL_ROOTS",
        "VFS_INJECT_CWD",
    ] {
        if !env.contains_key(stale) {
            cmd.env_remove(stale);
        }
    }
    let status = cmd
        .status()
        .map_err(|e| LaunchError::Spawn(format!("{prog}: {e}")))?;
    finish(l, status)
}

/// How a launch ended: the target's exit code, or why the injector never ran
/// it (its report, if it wrote one, else its exit code).
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
            "arena_offset {} + arena_len {} = {need} exceeds ring_bytes {}; the child would              map a view too small to hold the arena and fail only under load",
            l.arena_offset, l.arena_len, l.ring_bytes
        )));
    }
    match std::fs::metadata(&l.ring_path) {
        Ok(m) if (m.len() as usize) < l.ring_bytes => Err(LaunchError::Geometry(format!(
            "ring file {} is {} bytes but ring_bytes is {}; mapping past the end of a file              faults on touch rather than failing at map time",
            l.ring_path.display(),
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
        let p = std::env::temp_dir().join(format!("vfs-launch-short-{}.bin", std::process::id()));
        std::fs::write(&p, [0u8; 128]).unwrap();
        l.ring_path = p.clone();
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
            virtual_roots: vec![],
            args: vec!["-arg1".to_string(), "arg2".to_string()],
            extra_env: BTreeMap::new(),
            cwd: None,
            ready_timeout_secs: None,
        }
    }

    #[test]
    fn extra_roots_travel_in_the_windows_format() {
        let mut l = sample();
        l.virtual_roots = vec![(1, r"C:\users\steamuser\Saves".into()), (2, r"C:\x".into())];
        let env = launch_env(&l);
        assert_eq!(
            env.get("VFS_VIRTUAL_ROOTS").map(String::as_str),
            Some(r"1=C:\users\steamuser\Saves;2=C:\x")
        );
    }

    #[test]
    fn no_extra_roots_means_no_root_map() {
        assert!(!launch_env(&sample()).contains_key("VFS_VIRTUAL_ROOTS"));
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
        assert_eq!(env.get("WINEDEBUG").map(String::as_str), Some("-all"));
        assert!(!env.contains_key(vfs_env::INJECT_CWD));
        assert!(!env.contains_key(vfs_env::READY_TIMEOUT_SECS));
    }

    #[test]
    fn dll_overrides_merge_with_the_caller_winning_per_dll() {
        assert_eq!(merge_dll_overrides(BASE_DLL_OVERRIDES, ""), "mscoree=d;mshtml=d");
        assert_eq!(
            merge_dll_overrides(BASE_DLL_OVERRIDES, "d3dx9_42=n,b"),
            "mscoree=d;mshtml=d;d3dx9_42=n,b",
            "a mode with a comma is one mode"
        );
        assert_eq!(
            merge_dll_overrides(BASE_DLL_OVERRIDES, "MSHTML=n;dxgi,d3d11=n; ;"),
            "mscoree=d;MSHTML=n;dxgi=n;d3d11=n",
            "the caller's entry replaces ours in place; a group splits per DLL"
        );
        assert_eq!(merge_dll_overrides("a=d", "winemenubuilder"), "a=d;winemenubuilder");
    }

    #[test]
    fn extra_env_reaches_the_child_env_with_overrides_merged() {
        let mut l = sample();
        l.extra_env = BTreeMap::from([
            ("WINEDLLOVERRIDES".to_string(), "d3dx9_42=n,b".to_string()),
            ("WINEDEBUG".to_string(), "+loaddll".to_string()),
            ("SteamAppId".to_string(), "489830".to_string()),
        ]);
        let env = launch_env(&l);
        assert_eq!(env["WINEDLLOVERRIDES"], "mscoree=d;mshtml=d;d3dx9_42=n,b");
        assert_eq!(env["WINEDEBUG"], "+loaddll");
        assert_eq!(env["SteamAppId"], "489830");
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

    #[test]
    fn injector_reports_are_described() {
        let dll = describe_injector_error("target-exited:0xc0000135\n");
        assert!(dll.contains("0xc0000135") && dll.contains("STATUS_DLL_NOT_FOUND"), "{dll}");
        let other = describe_injector_error("target-exited:0x1");
        assert!(other.contains("0x1") && other.contains("before the shim reported ready"), "{other}");
        let t = describe_injector_error("ready-timeout:180");
        assert!(t.contains("180 s"), "{t}");
        assert_eq!(describe_injector_error("inject:CreateProcess"), "injection failed: CreateProcess");
        assert_eq!(
            injector_error_path(Path::new("/s/ready.flag")),
            Path::new("/s/ready.flag.injector-error")
        );
    }

    #[cfg(unix)]
    fn exit_status(code: i32) -> std::process::ExitStatus {
        std::process::Command::new("sh").arg("-c").arg(format!("exit {code}")).status().unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn finish_maps_exits_and_reads_the_injector_report() {
        let dir = std::env::temp_dir().join(format!("vfs-launch-finish-{}", std::process::id()));
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
        ] {
            assert!(is_reserved_env(k), "{k}");
        }
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

//! Per-session Wine prefixes: creation, drive-letter rerouting, and dropping
//! `dosdevices/z:` for containment.
//!
//! A fresh Wine prefix maps `dosdevices/z: -> /`, putting the entire host
//! filesystem inside the game's namespace. `unmap_drive('z', ..)` is how this
//! crate removes that: containment Windows does not have, and worth keeping
//! as a first-class, tested operation rather than an incidental side effect.

use std::io;
use std::path::{Path, PathBuf};

use crate::layout::Root;
use crate::runtime::verify_ge;

/// A session's private Wine prefix: the directory `WINEPREFIX` points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prefix {
    pub dir: PathBuf,
}

/// Why [`ensure`], [`Prefix::map_drive`], or [`Prefix::unmap_drive`] failed.
#[derive(Debug)]
pub enum PrefixError {
    /// Filesystem I/O failed, including "the session id was rejected" and
    /// "`wineboot` could not even be launched" (e.g. binary not found).
    Io(io::Error),
    /// `wineboot -u` ran and exited non-zero for a reason other than the
    /// missing-32-bit-loader case. Carries its combined stdout/stderr.
    Wineboot(String),
    /// `runtime` is not a verified GE-Proton build. `PROTONPATH` defaults to
    /// stock Valve Proton, and silently launching a session on top of that
    /// default is the exact failure this crate exists to prevent, so this is
    /// always a hard error, never a fallback.
    NotGe(String),
    /// `wineboot` failed because no 32-bit runtime is installed. The `wine`
    /// launcher probes for the 32-bit loader even under `WINEARCH=win64`, so
    /// this can't be avoided by architecture choice — only by installing the
    /// packages.
    Missing32Bit,
    /// Another live launch holds this prefix's lock.
    Busy(PathBuf),
    /// A root location that cannot be linked into the prefix (not on `C:`,
    /// contains `..`, is the drive root, or is occupied by a real file, a
    /// directory, or a symlink aether-vfs did not create).
    BadLocation(String),
    /// Proton's own prefix setup ([`PrefixInit::Proton`]) could not run, did
    /// not finish, or finished without a prefix. Carries what happened and
    /// the tail of its log.
    ProtonInit(String),
}

impl std::fmt::Display for PrefixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrefixError::Io(e) => write!(f, "io error: {e}"),
            PrefixError::Wineboot(s) => write!(f, "wineboot failed: {s}"),
            PrefixError::NotGe(s) => write!(f, "runtime is not GE-Proton: {s}"),
            PrefixError::Busy(d) => {
                write!(f, "prefix {} is in use by another live launch", d.display())
            }
            PrefixError::BadLocation(s) => write!(f, "bad root location {s}"),
            PrefixError::ProtonInit(s) => write!(f, "proton prefix setup failed: {s}"),
            PrefixError::Missing32Bit => write!(
                f,
                "wineboot needs a 32-bit runtime: install lib32-glibc and \
                 lib32-gcc-libs (Arch) or your distro's equivalent packages"
            ),
        }
    }
}

impl std::error::Error for PrefixError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PrefixError::Io(e) => Some(e),
            PrefixError::Wineboot(_)
            | PrefixError::NotGe(_)
            | PrefixError::Missing32Bit
            | PrefixError::Busy(_)
            | PrefixError::BadLocation(_)
            | PrefixError::ProtonInit(_) => None,
        }
    }
}

impl From<io::Error> for PrefixError {
    fn from(e: io::Error) -> Self {
        PrefixError::Io(e)
    }
}

/// Creates (if needed) `root.sessions()/<session>/prefix` as a Wine prefix
/// for `session`, verifying `runtime` is GE-Proton first.
///
/// Idempotent: if `drive_c/windows/system32` already exists, the prefix is
/// treated as initialised and `wineboot` is not run again.
pub fn ensure(root: &Root, runtime: &Path, session: &str) -> Result<Prefix, PrefixError> {
    let session_dir = root.try_session_dir(session).map_err(|e| {
        PrefixError::Io(io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))
    })?;
    let dir = session_dir.join("prefix");

    if !is_initialised(&dir) {
        // Verify GE *before* touching anything else on disk: `PROTONPATH`
        // defaults to stock Proton, and refusing here is what stops a
        // silent downgrade rather than a partially-created prefix.
        verify_ge(runtime).map_err(|e| PrefixError::NotGe(e.to_string()))?;
        std::fs::create_dir_all(&dir)?;
        run_wineboot(runtime, &dir)?;
    }

    Ok(Prefix { dir })
}

/// How a session's prefix is created, and so where it lives.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PrefixInit {
    /// `wine wineboot -u` into `sessions/<name>/prefix` — what [`ensure`]
    /// does. Enough for a console program; it has none of what Proton adds
    /// to a prefix (DXVK and vkd3d-proton in `system32`, the DirectX and
    /// Visual C++ redistributables from `default_pfx`, and the Steam bridge
    /// `lsteamclient.dll` + `steam.exe`), so most games cannot start in it.
    #[default]
    Wineboot,
    /// Proton's own setup: `<runtime>/proton run cmd /c exit` with
    /// `STEAM_COMPAT_DATA_PATH=sessions/<name>/compat`, so the prefix is
    /// `sessions/<name>/compat/pfx`. Proton records the runtime that set it
    /// up in `compat/version`; a prefix recorded by another runtime is set
    /// up again, which is Proton's own upgrade (or downgrade) path.
    ///
    /// The runtime's `proton` is a Python 3 script, so `python3` must be on
    /// `PATH`. Launches still run the runtime's `wine` directly, so Proton's
    /// per-launch `WINEDLLOVERRIDES` are not applied: a host that wants DXVK
    /// passes [`PROTON_GRAPHICS_OVERRIDES`] with its launch environment.
    Proton {
        /// `STEAM_COMPAT_CLIENT_INSTALL_PATH`: the Steam client's install
        /// directory (`~/.local/share/Steam`). Must exist; Proton reads
        /// Steam's own files from it where they exist and does without them
        /// where they do not.
        steam_client: PathBuf,
        /// Sent as `SteamAppId` and `SteamGameId`; `0` when `None`.
        app_id: Option<u32>,
    },
}

/// The `WINEDLLOVERRIDES` Proton itself sets to run a game on DXVK and
/// vkd3d-proton (its default, non-wined3d configuration), for a host that
/// launches in a [`PrefixInit::Proton`] prefix and wants the same.
pub const PROTON_GRAPHICS_OVERRIDES: &str =
    "d3d11=n;d3d10core=n;d3d9=n;dxgi=n;d3d8=n;d3d12=n;d3d12core=n";

/// How long Proton's prefix setup may take before it is killed. A first
/// setup copies `default_pfx` (seconds); the bound is for a wedged one.
pub const PROTON_INIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// The log Proton's prefix setup writes to, inside `sessions/<name>/compat`.
pub const PROTON_INIT_LOG: &str = "aether-proton-init.log";

/// Where [`ensure_with`] puts `session`'s prefix under `init`, without
/// creating anything: `sessions/<session>/prefix`, or
/// `sessions/<session>/compat/pfx` for [`PrefixInit::Proton`].
pub fn prefix_dir(root: &Root, session: &str, init: &PrefixInit) -> Result<PathBuf, PrefixError> {
    let session_dir = root.try_session_dir(session).map_err(|e| {
        PrefixError::Io(io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))
    })?;
    Ok(match init {
        PrefixInit::Wineboot => session_dir.join("prefix"),
        PrefixInit::Proton { .. } => session_dir.join("compat").join("pfx"),
    })
}

/// [`ensure`], choosing how the prefix is set up. Idempotent: a Wineboot
/// prefix is set up once; a Proton prefix is set up again only when the
/// runtime that set it up (`compat/version`) is not `runtime`.
///
/// `runtime` is verified as GE-Proton before anything runs, as [`ensure`]
/// does.
pub fn ensure_with(
    root: &Root,
    runtime: &Path,
    session: &str,
    init: &PrefixInit,
) -> Result<Prefix, PrefixError> {
    let PrefixInit::Proton { steam_client, app_id } = init else {
        return ensure(root, runtime, session);
    };
    let dir = prefix_dir(root, session, init)?;
    let compat = dir.parent().expect("compat/pfx has a parent").to_path_buf();
    let tag = verify_ge(runtime).map_err(|e| PrefixError::NotGe(e.to_string()))?;
    if is_initialised(&dir) && proton_prefix_version(&compat).as_deref() == Some(tag.as_str()) {
        return Ok(Prefix { dir });
    }
    if !steam_client.is_dir() {
        return Err(PrefixError::ProtonInit(format!(
            "the Steam client directory {} (STEAM_COMPAT_CLIENT_INSTALL_PATH) does not exist",
            steam_client.display()
        )));
    }
    std::fs::create_dir_all(&compat)?;
    run_proton_init(runtime, &compat, steam_client, app_id.unwrap_or(0))?;
    if !is_initialised(&dir) {
        return Err(PrefixError::ProtonInit(format!(
            "proton exited cleanly but {} has no drive_c/windows/system32{}",
            dir.display(),
            log_tail(&compat.join(PROTON_INIT_LOG))
        )));
    }
    Ok(Prefix { dir })
}

/// The runtime tag Proton recorded when it last set up the prefix under
/// `compat` (its `version` file), if any.
fn proton_prefix_version(compat: &Path) -> Option<String> {
    let v = std::fs::read_to_string(compat.join("version")).ok()?;
    Some(v.trim().to_string()).filter(|v| !v.is_empty())
}

fn run_proton_init(
    runtime: &Path,
    compat: &Path,
    steam_client: &Path,
    app_id: u32,
) -> Result<(), PrefixError> {
    let proton = runtime.join("proton");
    let log_path = compat.join(PROTON_INIT_LOG);
    let log = std::fs::File::create(&log_path)?;
    let app = app_id.to_string();
    let mut cmd = std::process::Command::new(&proton);
    cmd.args(["run", "cmd", "/c", "exit"])
        .env("STEAM_COMPAT_DATA_PATH", compat)
        .env("STEAM_COMPAT_CLIENT_INSTALL_PATH", steam_client)
        .env("SteamAppId", &app)
        .env("SteamGameId", &app)
        // Game fixes are per-game, not prefix setup, and some of them
        // download (winetricks); none belongs in a prefix aether-vfs builds.
        .env("PROTONFIXES_DISABLE", "1")
        .env("WINEDEBUG", "-all")
        // Proton picks the prefix from STEAM_COMPAT_DATA_PATH; an inherited
        // WINEPREFIX would only confuse a reader of the log.
        .env_remove("WINEPREFIX")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    let status = match run_bounded_status(&mut cmd, PROTON_INIT_TIMEOUT) {
        Ok(s) => s,
        Err(e) => {
            return Err(PrefixError::ProtonInit(format!(
                "could not run {} (a Python 3 script: is python3 installed?): {e}",
                proton.display()
            )))
        }
    };
    match status {
        Some(s) if s.success() => Ok(()),
        Some(s) => Err(PrefixError::ProtonInit(format!(
            "{} run exited {s}{}",
            proton.display(),
            log_tail(&log_path)
        ))),
        None => Err(PrefixError::ProtonInit(format!(
            "{} run did not finish within {:?} and was killed{}",
            proton.display(),
            PROTON_INIT_TIMEOUT,
            log_tail(&log_path)
        ))),
    }
}

/// The last lines of `log`, formatted to follow an error message.
fn log_tail(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines[lines.len().saturating_sub(20)..].join("\n");
    format!("; log {}:\n{tail}", log.display())
}

fn is_initialised(prefix_dir: &Path) -> bool {
    prefix_dir
        .join("drive_c")
        .join("windows")
        .join("system32")
        .is_dir()
}

fn run_wineboot(runtime: &Path, prefix_dir: &Path) -> Result<(), PrefixError> {
    let wine = runtime.join("files").join("bin").join("wine");
    let output = std::process::Command::new(&wine)
        .arg("wineboot")
        .arg("-u")
        .env("WINEPREFIX", prefix_dir)
        .env("WINEDLLOVERRIDES", "mscoree=d;mshtml=d")
        .env("WINEDEBUG", "-all")
        .output()?;

    if output.status.success() {
        return Ok(());
    }

    // FreeType warnings are cosmetic for console targets and show up on
    // stderr even on success; only a non-zero exit gets here at all, so no
    // extra filtering of the "good" case is needed.
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if combined.contains("ld-linux.so.2") {
        return Err(PrefixError::Missing32Bit);
    }
    Err(PrefixError::Wineboot(combined))
}

/// Held for one launch; the OS releases the lock when the file closes.
#[derive(Debug)]
pub struct PrefixLock(#[allow(dead_code)] std::fs::File);

#[cfg(unix)]
fn make_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn make_symlink(_target: &Path, _link: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "root links are a unix (Wine) concept only",
    ))
}

/// The entry of `dir` named `name` under Wine's rule — ASCII
/// case-insensitive — preferring an exact match. `None` when absent.
///
/// Wine resolves `C:\Users` to `drive_c/users`; ext4 would happily create a
/// second, differently-spelled `Users` beside it that Wine never looks in.
fn find_entry_ci(dir: &Path, name: &str) -> io::Result<Option<std::ffi::OsString>> {
    match std::fs::symlink_metadata(dir.join(name)) {
        Ok(_) => return Ok(Some(name.into())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(it) => it,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let found = entry?.file_name();
        if found.to_str().is_some_and(|f| f.eq_ignore_ascii_case(name)) {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// The file in a prefix directory listing every root link aether-vfs
/// created there ([`Prefix::link_location`]), one host path per line. A
/// symlink at a root location that is **not** listed is someone else's — a
/// user's own `drive_c/Games/X -> ~/…` in a persistent prefix — and is
/// treated exactly like a real directory: refused, never replaced.
pub const LINK_MANIFEST: &str = ".aether-vfs-links";

/// A root location (`C:\…`, either separator, any case of `c`) as the
/// components under `drive_c` it names — the rule [`Prefix::link_location`]
/// applies, exposed so a host can refuse a bad location when it is
/// **declared** rather than at the first launch.
///
/// Refused, as [`PrefixError::BadLocation`] naming `location`: anything not on
/// drive `C:` (another letter, a host path, a relative or UNC path), the
/// drive root itself, and any `..` component. Empty and `.` components are
/// dropped.
pub fn parse_location(location: &str) -> Result<Vec<String>, PrefixError> {
    let bad = |why: &str| PrefixError::BadLocation(format!("{location}: {why}"));
    let norm = location.replace('/', "\\");
    let rest = norm
        .strip_prefix("C:\\")
        .or_else(|| norm.strip_prefix("c:\\"))
        .ok_or_else(|| bad("a root location must be on drive C: (C:\\...)"))?;
    let comps: Vec<String> = rest
        .split('\\')
        .filter(|c| !c.is_empty() && *c != ".")
        .map(str::to_string)
        .collect();
    if comps.is_empty() {
        return Err(bad("the drive root itself cannot be a root location"));
    }
    if comps.iter().any(|c| c == "..") {
        return Err(bad("'..' is not allowed"));
    }
    Ok(comps)
}

#[cfg(unix)]
fn path_bytes(p: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(p: &Path) -> Vec<u8> {
    p.to_string_lossy().into_owned().into_bytes()
}

#[cfg(unix)]
fn path_from_bytes(b: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(b))
}

#[cfg(not(unix))]
fn path_from_bytes(b: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(b).into_owned())
}

/// Deletes `root.sessions()/<session>` — prefix and all. Absent is fine.
pub fn remove_session(root: &Root, session: &str) -> io::Result<()> {
    let dir = root
        .try_session_dir(session)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
    match std::fs::remove_dir_all(&dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

impl Prefix {
    /// Links `location` (a `C:\…` path, as the program sees it) to `target` on
    /// the host, creating missing parent directories under `drive_c`. Returns
    /// the host path of the link.
    ///
    /// Each component is matched against what already exists **ASCII
    /// case-insensitively**, as Wine resolves it: `C:\Users\SteamUser\X`
    /// links at `drive_c/users/steamuser/X` when `drive_c/users/steamuser`
    /// exists. Only missing components are created, with the declared
    /// spelling.
    ///
    /// Replaces an existing symlink **only if aether-vfs created it** — it is
    /// listed in the prefix's [`LINK_MANIFEST`] (a relaunch relinks) — and
    /// refuses to touch anything else: a persistent prefix may hold a user's
    /// own files, directories or symlinks at that path, and a root is never
    /// placed over them. Every link created is recorded in the manifest;
    /// [`Prefix::unlink_location`] is its counterpart.
    ///
    /// The location rules are [`parse_location`]'s.
    pub fn link_location(&self, location: &str, target: &Path) -> Result<PathBuf, PrefixError> {
        let bad = |why: &str| PrefixError::BadLocation(format!("{location}: {why}"));
        let comps = parse_location(location)?;
        let (last, parents) = comps.split_last().expect("parse_location is non-empty");
        let mut parent = self.drive_c();
        std::fs::create_dir_all(&parent)?;
        for c in parents {
            match find_entry_ci(&parent, c)? {
                Some(existing) => parent.push(existing),
                None => {
                    parent.push(c);
                    match std::fs::create_dir(&parent) {
                        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e.into()),
                        _ => {}
                    }
                }
            }
        }
        let mut manifest = self.read_manifest()?;
        if let Some(existing) = find_entry_ci(&parent, last)? {
            let at = parent.join(existing);
            if !std::fs::symlink_metadata(&at)?.file_type().is_symlink() {
                return Err(bad(&format!(
                    "{} already exists in the prefix as a real file or directory; a root \
                     cannot be placed over it",
                    at.display()
                )));
            }
            if !manifest.contains(&at) {
                return Err(bad(&format!(
                    "{} already exists in the prefix as a symlink aether-vfs did not create \
                     (it is not listed in {}); a root cannot replace it — remove it yourself \
                     if it is stale",
                    at.display(),
                    self.manifest_path().display()
                )));
            }
            std::fs::remove_file(&at)?;
            manifest.retain(|l| *l != at);
            self.write_manifest(&manifest)?;
        }
        let link = parent.join(last);
        make_symlink(target, &link)?;
        manifest.push(link.clone());
        if let Err(e) = self.write_manifest(&manifest) {
            // An unrecorded link would be refused as foreign next time and
            // never removed: undo it rather than leave it.
            let _ = std::fs::remove_file(&link);
            return Err(e.into());
        }
        Ok(link)
    }

    /// Removes the root link at `link` — **only** if aether-vfs created it
    /// (listed in [`LINK_MANIFEST`]) and it is still a symlink to
    /// `expected_target` — and drops it from the manifest. Returns whether it
    /// removed the link.
    ///
    /// Anything else is left alone: a real file or directory now at that
    /// path, or a link repointed since (another live session relinked the
    /// location). A listed path that is no longer a symlink at all is
    /// dropped from the manifest, since nothing of ours remains there; one
    /// repointed by another session stays listed — that link is ours too.
    pub fn unlink_location(&self, link: &Path, expected_target: &Path) -> io::Result<bool> {
        let mut manifest = self.read_manifest()?;
        if !manifest.iter().any(|l| l == link) {
            return Ok(false);
        }
        let is_link = match std::fs::symlink_metadata(link) {
            Ok(m) => m.file_type().is_symlink(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(e),
        };
        let removed = if !is_link {
            false
        } else if std::fs::read_link(link)? == expected_target {
            std::fs::remove_file(link)?;
            true
        } else {
            return Ok(false);
        };
        manifest.retain(|l| l != link);
        self.write_manifest(&manifest)?;
        Ok(removed)
    }

    /// `<prefix>/.aether-vfs-links` — see [`LINK_MANIFEST`].
    pub fn manifest_path(&self) -> PathBuf {
        self.dir.join(LINK_MANIFEST)
    }

    /// Every link [`LINK_MANIFEST`] lists; empty when it does not exist.
    pub fn read_manifest(&self) -> io::Result<Vec<PathBuf>> {
        match std::fs::read(self.manifest_path()) {
            Ok(bytes) => Ok(bytes
                .split(|&b| b == b'\n')
                .filter(|l| !l.is_empty())
                .map(path_from_bytes)
                .collect()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Replaces the manifest (temp file + rename, so a reader never sees half
    /// of it); an empty list removes it.
    fn write_manifest(&self, links: &[PathBuf]) -> io::Result<()> {
        let path = self.manifest_path();
        if links.is_empty() {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                r => r,
            };
        }
        let mut bytes = Vec::new();
        for l in links {
            bytes.extend_from_slice(&path_bytes(l));
            bytes.push(b'\n');
        }
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(format!("{LINK_MANIFEST}.tmp"));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)
    }

    /// An exclusive lock on this prefix for the duration of one launch, so two
    /// live sessions cannot relink the same roots under each other.
    pub fn lock(&self) -> Result<PrefixLock, PrefixError> {
        std::fs::create_dir_all(&self.dir)?;
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join(".aether-vfs.lock"))?;
        match f.try_lock() {
            Ok(()) => Ok(PrefixLock(f)),
            Err(std::fs::TryLockError::WouldBlock) => Err(PrefixError::Busy(self.dir.clone())),
            Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
        }
    }

    /// Stops this prefix's `wineserver` (and any Wine process still in it) and
    /// waits until it has exited, using `runtime`'s own `wineserver`.
    ///
    /// Needed before deleting a prefix: `wineserver` outlives the last Wine
    /// process by a few seconds and **writes the registry back into the prefix
    /// as it exits**, so a prefix removed while it lingers is recreated
    /// (`system.reg`, `user.reg`, `userdef.reg`) moments later. Absent
    /// server: returns promptly.
    ///
    /// **Bounded**: each step gets [`WINESERVER_STOP_TIMEOUT`] and is killed
    /// past it, so a wedged server cannot hang the caller — a `Session`'s
    /// `Drop`, or a daemon draining on shutdown. A step that had to be killed
    /// is reported as `TimedOut`, after both steps have been tried.
    pub fn stop_wineserver(&self, runtime: &Path) -> io::Result<()> {
        let server = runtime.join("files").join("bin").join("wineserver");
        let mut timed_out = None;
        for flag in ["-k", "-w"] {
            // `-k` exits non-zero when no server is running; that is fine.
            let mut cmd = std::process::Command::new(&server);
            cmd.arg(flag)
                .env("WINEPREFIX", &self.dir)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            if !run_bounded(&mut cmd, WINESERVER_STOP_TIMEOUT)? {
                timed_out.get_or_insert(flag);
            }
        }
        match timed_out {
            None => Ok(()),
            Some(flag) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "wineserver {flag} for {} did not finish within {:?} and was killed",
                    self.dir.display(),
                    WINESERVER_STOP_TIMEOUT
                ),
            )),
        }
    }

    /// `<prefix>/drive_c`, the root of the Windows-visible filesystem.
    pub fn drive_c(&self) -> PathBuf {
        self.dir.join("drive_c")
    }

    /// Points `dosdevices/<letter>:` at `target`, replacing any existing
    /// mapping (a session reuses a prefix across launches, so remapping must
    /// not fail just because a link is already there).
    pub fn map_drive(&self, letter: char, target: &Path) -> Result<(), PrefixError> {
        let link = self.dosdevices_link(letter);
        remove_link_if_present(&link)?;

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, &link)?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = target;
            Err(PrefixError::Io(io::Error::new(
                io::ErrorKind::Unsupported,
                "dosdevices drive mapping is a unix (Wine) concept only",
            )))
        }
    }

    /// Removes `dosdevices/<letter>:`. In particular, removing `z:` — which
    /// a fresh prefix maps to `/`, the whole host filesystem — is how this
    /// crate achieves containment; it is a supported, intentional case, not
    /// an edge case.
    pub fn unmap_drive(&self, letter: char) -> Result<(), PrefixError> {
        remove_link_if_present(&self.dosdevices_link(letter))
    }

    fn dosdevices_link(&self, letter: char) -> PathBuf {
        self.dir.join("dosdevices").join(format!("{letter}:"))
    }

    /// Renders a host path under `drive_c` as the `C:\...` form Wine sees.
    /// Returns `None` for anything not under `drive_c` — such a path has no
    /// `C:` form and one must not be invented.
    pub fn windows_path(&self, host: &Path) -> Option<String> {
        let rel = host.strip_prefix(self.drive_c()).ok()?;
        let mut out = String::from("C:");
        for component in rel.components() {
            match component {
                std::path::Component::Normal(part) => {
                    out.push('\\');
                    out.push_str(&part.to_string_lossy());
                }
                _ => return None,
            }
        }
        Some(out)
    }
}

/// How long each step of [`Prefix::stop_wineserver`] may take.
pub const WINESERVER_STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Runs `cmd` to completion or for at most `timeout`, whichever is first; a
/// child still running at the deadline is killed and reaped. `Ok(true)` when
/// it finished by itself (whatever its exit status), `Ok(false)` when it had
/// to be killed.
fn run_bounded(cmd: &mut std::process::Command, timeout: std::time::Duration) -> io::Result<bool> {
    Ok(run_bounded_status(cmd, timeout)?.is_some())
}

/// [`run_bounded`], keeping the exit status: `Some` when the child finished
/// by itself, `None` when it had to be killed.
fn run_bounded_status(
    cmd: &mut std::process::Command,
    timeout: std::time::Duration,
) -> io::Result<Option<std::process::ExitStatus>> {
    let mut child = cmd.spawn()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn remove_link_if_present(link: &Path) -> Result<(), PrefixError> {
    match std::fs::remove_file(link) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(PrefixError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("vfs-prefix-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn windows_path_maps_only_paths_under_drive_c() {
        let p = Prefix { dir: scratch("wp") };
        std::fs::create_dir_all(p.drive_c().join("Games")).unwrap();
        let inside = p.drive_c().join("Games").join("g.exe");
        assert_eq!(
            p.windows_path(&inside).as_deref(),
            Some(r"C:\Games\g.exe"),
            "a path under drive_c must render as a C: path with backslashes"
        );
        assert_eq!(
            p.windows_path(std::path::Path::new("/etc/passwd")),
            None,
            "a path outside the prefix has no C: form and must not be invented"
        );
    }

    #[cfg(unix)]
    #[test]
    fn map_drive_points_dosdevices_at_the_target_and_unmap_removes_it() {
        let p = Prefix { dir: scratch("drv") };
        std::fs::create_dir_all(p.dir.join("dosdevices")).unwrap();
        let target = scratch("drv-target");
        p.map_drive('d', &target).unwrap();
        let link = p.dir.join("dosdevices").join("d:");
        assert_eq!(std::fs::read_link(&link).unwrap(), target);
        // Remapping must replace, not fail: a session reuses a prefix.
        let target2 = scratch("drv-target2");
        p.map_drive('d', &target2).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), target2);
        p.unmap_drive('d').unwrap();
        assert!(!link.exists(), "unmap must remove the link");
    }

    #[cfg(unix)]
    #[test]
    fn unmapping_z_is_how_containment_is_achieved() {
        // `dosdevices/z: -> /` maps the whole host filesystem into the game's
        // namespace. Removing it gives containment Windows does not have, so
        // this is a feature and needs to keep working.
        let p = Prefix { dir: scratch("z") };
        let dd = p.dir.join("dosdevices");
        std::fs::create_dir_all(&dd).unwrap();
        std::os::unix::fs::symlink("/", dd.join("z:")).unwrap();
        p.unmap_drive('z').unwrap();
        assert!(!dd.join("z:").exists());
    }

    #[cfg(unix)]
    #[test]
    fn link_location_creates_parents_and_links() {
        let p = Prefix { dir: scratch("ll") };
        let target = scratch("ll-target");
        let link = p.link_location(r"C:\Games\Fixture", &target).unwrap();
        assert_eq!(link, p.drive_c().join("Games").join("Fixture"));
        assert_eq!(std::fs::read_link(&link).unwrap(), target);
        // Relinking (a relaunch) replaces our own symlink.
        let target2 = scratch("ll-target2");
        p.link_location("c:/Games/Fixture/", &target2).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), target2);
    }

    /// Wine resolves names case-insensitively; ext4 does not. A location
    /// spelled `C:\Users\SteamUser\Saves` must land inside the prefix's own
    /// `drive_c/users/steamuser`, not beside it in a new `Users` tree Wine
    /// would never look in.
    #[cfg(unix)]
    #[test]
    fn link_location_reuses_existing_parents_case_insensitively() {
        let p = Prefix { dir: scratch("ll-case") };
        std::fs::create_dir_all(p.drive_c().join("users").join("steamuser")).unwrap();
        let target = scratch("ll-case-target");
        let link = p.link_location(r"C:\Users\SteamUser\Saves", &target).unwrap();
        assert_eq!(link, p.drive_c().join("users").join("steamuser").join("Saves"));
        assert_eq!(std::fs::read_link(&link).unwrap(), target);
        assert!(
            !p.drive_c().join("Users").exists(),
            "an existing parent must be reused, not shadowed by a sibling spelled differently"
        );
        // Relinking under another spelling finds and replaces our own link.
        let target2 = scratch("ll-case-target2");
        let link2 = p.link_location(r"c:\USERS\steamuser\saves", &target2).unwrap();
        let entries: Vec<_> = std::fs::read_dir(p.drive_c().join("users").join("steamuser"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1, "the old link must be replaced, not joined: {entries:?}");
        assert_eq!(std::fs::read_link(&link2).unwrap(), target2);
    }

    #[cfg(unix)]
    #[test]
    fn link_location_refuses_a_real_directory_spelled_differently() {
        let p = Prefix { dir: scratch("ll-case-real") };
        let real = p.drive_c().join("games").join("mine");
        std::fs::create_dir_all(&real).unwrap();
        let err = p.link_location(r"C:\Games\Mine", &scratch("ll-case-t")).unwrap_err();
        assert!(matches!(err, PrefixError::BadLocation(_)), "{err}");
        assert!(real.is_dir() && !real.is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn link_location_refuses_to_replace_a_real_directory() {
        let p = Prefix { dir: scratch("ll-real") };
        let real = p.drive_c().join("Games").join("Mine");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("keep.txt"), b"keep").unwrap();
        let err = p.link_location(r"C:\Games\Mine", &scratch("ll-t")).unwrap_err();
        assert!(matches!(err, PrefixError::BadLocation(_)), "{err}");
        assert!(real.join("keep.txt").is_file(), "a real directory must never be removed");
    }

    /// A user's own symlink at a root location in a persistent prefix is not
    /// ours to replace: refused like a real directory, and it survives.
    #[cfg(unix)]
    #[test]
    fn link_location_refuses_a_symlink_it_did_not_create() {
        let p = Prefix { dir: scratch("ll-foreign") };
        let theirs = scratch("ll-foreign-theirs");
        std::fs::create_dir_all(p.drive_c().join("Games")).unwrap();
        let at = p.drive_c().join("Games").join("Skyrim");
        std::os::unix::fs::symlink(&theirs, &at).unwrap();
        let err = p.link_location(r"C:\Games\Skyrim", &scratch("ll-foreign-t")).unwrap_err();
        assert!(
            matches!(&err, PrefixError::BadLocation(m) if m.contains("did not create")),
            "{err}"
        );
        assert_eq!(std::fs::read_link(&at).unwrap(), theirs, "their link must survive");
        assert!(p.read_manifest().unwrap().is_empty());
        // Nor does `unlink_location` touch it, even naming its exact target.
        assert!(!p.unlink_location(&at, &theirs).unwrap());
        assert_eq!(std::fs::read_link(&at).unwrap(), theirs);
    }

    #[cfg(unix)]
    #[test]
    fn our_links_are_recorded_and_unlink_removes_them_from_the_manifest() {
        let p = Prefix { dir: scratch("ll-manifest") };
        let t1 = scratch("ll-manifest-t1");
        let t2 = scratch("ll-manifest-t2");
        let a = p.link_location(r"C:\Games\A", &t1).unwrap();
        let b = p.link_location(r"C:\users\steamuser\B", &t1).unwrap();
        assert_eq!(p.read_manifest().unwrap(), [a.clone(), b.clone()]);
        // Relinking ours replaces it and keeps one entry for it.
        assert_eq!(p.link_location("c:/GAMES/A/", &t2).unwrap(), a);
        assert_eq!(std::fs::read_link(&a).unwrap(), t2);
        assert_eq!(p.read_manifest().unwrap(), [b.clone(), a.clone()]);

        // The wrong expected target: repointed since, so left alone, listed.
        assert!(!p.unlink_location(&a, &t1).unwrap());
        assert!(a.is_symlink());
        assert!(p.read_manifest().unwrap().contains(&a));
        // The right one: removed, and gone from the manifest.
        assert!(p.unlink_location(&a, &t2).unwrap());
        assert!(std::fs::symlink_metadata(&a).is_err());
        assert_eq!(p.read_manifest().unwrap(), std::slice::from_ref(&b));

        // A listed path that became a real directory: kept, and delisted —
        // nothing of ours is there any more.
        std::fs::remove_file(&b).unwrap();
        std::fs::create_dir(&b).unwrap();
        assert!(!p.unlink_location(&b, &t1).unwrap());
        assert!(b.is_dir());
        assert!(p.read_manifest().unwrap().is_empty());
        assert!(!p.manifest_path().exists(), "an empty manifest is removed");
    }

    #[test]
    fn parse_location_names_components_under_drive_c() {
        assert_eq!(parse_location(r"C:\Games\Fixture").unwrap(), ["Games", "Fixture"]);
        assert_eq!(parse_location("c:/Games/./Fixture/").unwrap(), ["Games", "Fixture"]);
        for bad in [r"D:\Games", r"C:\a\..\b", r"C:\", "C:", "Games", "/tmp/x", r"\\srv\share\x", ""] {
            match parse_location(bad) {
                Err(PrefixError::BadLocation(m)) => assert!(m.starts_with(bad), "{bad}: {m}"),
                other => panic!("{bad:?} must be refused, got {other:?}"),
            }
        }
    }

    #[test]
    fn link_location_refuses_other_drives_and_dot_dot() {
        let p = Prefix { dir: scratch("ll-bad") };
        for bad in [r"D:\Games", r"C:\a\..\b", r"C:\", "Games", r"\\srv\share\x"] {
            assert!(
                matches!(p.link_location(bad, Path::new("/tmp")), Err(PrefixError::BadLocation(_))),
                "{bad} must be refused"
            );
        }
    }

    #[test]
    fn a_second_lock_is_busy_and_the_lock_is_released_on_drop() {
        let p = Prefix { dir: scratch("lock") };
        let held = p.lock().unwrap();
        assert!(matches!(p.lock(), Err(PrefixError::Busy(_))));
        drop(held);
        p.lock().expect("the lock must be released on drop");
    }

    #[cfg(unix)]
    #[test]
    fn run_bounded_kills_a_child_past_its_deadline() {
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let finished =
            run_bounded(std::process::Command::new("sleep").arg("30"), Duration::from_millis(200))
                .unwrap();
        assert!(!finished, "a child past its deadline is reported as killed");
        assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());
        assert!(run_bounded(&mut std::process::Command::new("true"), Duration::from_secs(10)).unwrap());
        // A failing exit status still counts as finished.
        assert!(run_bounded(&mut std::process::Command::new("false"), Duration::from_secs(10)).unwrap());
    }

    #[test]
    fn remove_session_deletes_only_that_session() {
        let base = scratch("rm");
        let root = Root::at(base.clone());
        let a = root.try_session_dir("a").unwrap();
        let b = root.try_session_dir("b").unwrap();
        std::fs::create_dir_all(a.join("prefix")).unwrap();
        std::fs::create_dir_all(b.join("prefix")).unwrap();
        remove_session(&root, "a").unwrap();
        assert!(!a.exists() && b.exists());
        remove_session(&root, "a").expect("removing an absent session is not an error");
    }

    /// A runtime whose `proton` is a shell script: it logs its argv and the
    /// env Proton setup is given to `$STEAM_COMPAT_DATA_PATH/calls`, then
    /// does what Proton does to the disk (a `pfx/drive_c/windows/system32`
    /// and a `version` naming the runtime's tag) unless `body` exits first.
    #[cfg(unix)]
    fn fake_proton_runtime(tag: &str, version: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let rt = scratch(tag);
        std::fs::write(rt.join("version"), format!("1 {version}\n")).unwrap();
        let script = format!(
            "#!/bin/sh\n{body}\n\
             echo \"$*|$STEAM_COMPAT_CLIENT_INSTALL_PATH|$SteamAppId|$SteamGameId|$PROTONFIXES_DISABLE\" \
             >> \"$STEAM_COMPAT_DATA_PATH/calls\"\n\
             mkdir -p \"$STEAM_COMPAT_DATA_PATH/pfx/drive_c/windows/system32\"\n\
             awk '{{print $2}}' \"$(dirname \"$0\")/version\" > \"$STEAM_COMPAT_DATA_PATH/version\"\n"
        );
        let p = rt.join("proton");
        std::fs::write(&p, script).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        rt
    }

    #[cfg(unix)]
    fn calls(root: &Root, session: &str) -> Vec<String> {
        let f = root.try_session_dir(session).unwrap().join("compat").join("calls");
        std::fs::read_to_string(f).unwrap_or_default().lines().map(str::to_string).collect()
    }

    #[test]
    fn prefix_dir_follows_the_init() {
        let root = Root::at(PathBuf::from("/home/x"));
        let s = root.try_session_dir("s").unwrap();
        assert_eq!(prefix_dir(&root, "s", &PrefixInit::Wineboot).unwrap(), s.join("prefix"));
        let proton = PrefixInit::Proton { steam_client: PathBuf::from("/steam"), app_id: None };
        assert_eq!(prefix_dir(&root, "s", &proton).unwrap(), s.join("compat").join("pfx"));
        assert!(prefix_dir(&root, "../x", &proton).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn proton_init_runs_proton_once_and_is_idempotent() {
        let root = Root::at(scratch("pi-home"));
        let steam = scratch("pi-steam");
        let rt = fake_proton_runtime("pi-rt", "GE-Proton99-1", "");
        let init = PrefixInit::Proton { steam_client: steam.clone(), app_id: Some(489830) };
        let p = ensure_with(&root, &rt, "s", &init).unwrap();
        assert_eq!(p.dir, prefix_dir(&root, "s", &init).unwrap());
        assert!(p.drive_c().join("windows").join("system32").is_dir());
        assert_eq!(
            calls(&root, "s"),
            [format!("run cmd /c exit|{}|489830|489830|1", steam.display())]
        );
        ensure_with(&root, &rt, "s", &init).unwrap();
        assert_eq!(calls(&root, "s").len(), 1, "an up-to-date prefix is not set up again");
    }

    #[cfg(unix)]
    #[test]
    fn a_prefix_set_up_by_another_runtime_is_set_up_again() {
        let root = Root::at(scratch("pu-home"));
        let steam = scratch("pu-steam");
        let rt = fake_proton_runtime("pu-rt", "GE-Proton99-1", "");
        let init = PrefixInit::Proton { steam_client: steam, app_id: None };
        ensure_with(&root, &rt, "s", &init).unwrap();
        std::fs::write(rt.join("version"), "2 GE-Proton99-2\n").unwrap();
        ensure_with(&root, &rt, "s", &init).unwrap();
        let c = calls(&root, "s");
        assert_eq!(c.len(), 2, "{c:?}");
        assert!(c[1].ends_with("|0|0|1"), "no app id is sent as 0: {}", c[1]);
        let compat = root.try_session_dir("s").unwrap().join("compat");
        assert_eq!(proton_prefix_version(&compat).as_deref(), Some("GE-Proton99-2"));
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_proton_setup_reports_its_log() {
        let root = Root::at(scratch("pf-home"));
        let rt = fake_proton_runtime("pf-rt", "GE-Proton99-1", "echo boom-from-proton >&2; exit 7");
        let init = PrefixInit::Proton { steam_client: scratch("pf-steam"), app_id: None };
        match ensure_with(&root, &rt, "s", &init) {
            Err(PrefixError::ProtonInit(m)) => {
                assert!(m.contains("boom-from-proton") && m.contains(PROTON_INIT_LOG), "{m}")
            }
            other => panic!("expected ProtonInit, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn proton_setup_that_leaves_no_prefix_is_an_error() {
        let root = Root::at(scratch("pn-home"));
        let rt = fake_proton_runtime("pn-rt", "GE-Proton99-1", "exit 0");
        let init = PrefixInit::Proton { steam_client: scratch("pn-steam"), app_id: None };
        match ensure_with(&root, &rt, "s", &init) {
            Err(PrefixError::ProtonInit(m)) => assert!(m.contains("system32"), "{m}"),
            other => panic!("expected ProtonInit, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_steam_client_or_a_non_ge_runtime_is_refused_before_proton_runs() {
        let root = Root::at(scratch("pm-home"));
        let rt = fake_proton_runtime("pm-rt", "GE-Proton99-1", "");
        let missing = PrefixInit::Proton { steam_client: scratch("pm-x").join("nope"), app_id: None };
        assert!(matches!(ensure_with(&root, &rt, "s", &missing), Err(PrefixError::ProtonInit(m)) if m.contains("nope")));
        std::fs::write(rt.join("version"), "1 proton-9.0-4\n").unwrap();
        let init = PrefixInit::Proton { steam_client: scratch("pm-steam"), app_id: None };
        assert!(matches!(ensure_with(&root, &rt, "s", &init), Err(PrefixError::NotGe(_))));
        assert!(calls(&root, "s").is_empty(), "proton never ran");
    }
}

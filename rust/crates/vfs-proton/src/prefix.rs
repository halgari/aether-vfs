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
            | PrefixError::BadLocation(_) => None,
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
    let mut child = cmd.spawn()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(false);
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
}

//! Disk directory provider — maps a host folder under a mount.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::ops::{
    bad_request, exists, map_io_err, not_a_dir, not_found, Access, Capabilities, CaseMatch,
    DirEntry, Handle, Provider, SetAttr, Stat, VPath, KIND_DIR, KIND_FILE, OPEN_CREATE,
    OPEN_EXCL, OPEN_TRUNC, OPEN_WRITE,
};

pub struct DiskProvider {
    root: PathBuf,
    next: AtomicU64,
    opens: Mutex<HashMap<u64, File>>,
}

impl DiskProvider {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        DiskProvider {
            root: root.into(),
            next: AtomicU64::new(1),
            opens: Mutex::new(HashMap::new()),
        }
    }

    /// Map a `VPath::rel` onto a real path under `root`.
    ///
    /// `rel` is documented as arriving normalized (no `..`), but that is a
    /// contract on the caller, not a guarantee this provider can see
    /// enforced elsewhere — `vfs-source`'s gRPC boundary rejects a `..`
    /// component from a network client, but this is a different crate, and
    /// `open`'s new `OPEN_CREATE` handling escalates a containment slip from
    /// unauthorized read to unauthorized directory creation. Reject a bare
    /// `..` component here too, so containment does not depend solely on a
    /// caller in another crate getting it right. A filename that merely
    /// starts with `..` (e.g. `..foo`) is a normal path segment and passes
    /// through untouched.
    fn resolve(&self, path: &str) -> Result<PathBuf, i32> {
        if path.is_empty() {
            return Ok(self.root.clone());
        }
        let mut p = self.root.clone();
        for part in path.split('/') {
            if part.is_empty() {
                continue;
            }
            if part == ".." {
                return Err(bad_request());
            }
            p.push(part);
        }
        Ok(p)
    }

    /// Case-fold-aware variant of [`DiskProvider::resolve`], for targets
    /// where the host filesystem does not fold on its own.
    ///
    /// On Windows this costs nothing: NTFS already matches `rel`'s spelling
    /// case-insensitively, so the exact path from `resolve` is already the
    /// right one to hand to the OS.
    #[cfg(windows)]
    fn resolve_case_aware(&self, path: &str) -> Result<PathBuf, i32> {
        self.resolve(path)
    }

    /// See the Windows arm above. Here the filesystem is byte-exact, so a
    /// fold-equal spelling that misses byte-exactly needs
    /// [`resolve_fold_equal`] to find the real on-disk entry before any
    /// filesystem call is made — `resolve`'s exact path is used only as a
    /// fallback when no such entry exists (i.e. this is a create).
    #[cfg(not(windows))]
    fn resolve_case_aware(&self, path: &str) -> Result<PathBuf, i32> {
        let exact = self.resolve(path)?;
        Ok(resolve_fold_equal(&self.root, path).unwrap_or(exact))
    }

    /// Fold-aware fallback for a path whose byte-exact spelling (`exact`,
    /// from [`DiskProvider::resolve`]) the filesystem itself just reported
    /// missing. Used by callers whose target is expected to already exist
    /// (`getattr`, `readdir`, a non-creating `open`, `set_attr`): they try
    /// `exact` first and pay for this — the same fold scan
    /// `resolve_case_aware` used to run on every call, hit or miss — only
    /// on that miss. A create must not use this: see
    /// [`DiskProvider::resolve_case_aware`]'s doc and
    /// `open_create_under_a_fold_equal_directory_does_not_fork_it`.
    ///
    /// A no-op on Windows: NTFS already folded `exact` for free, so the
    /// caller's retry against the same path is a second, harmless attempt
    /// rather than a wrong assumption that something new was found.
    #[cfg(windows)]
    fn resolve_fold_fallback(&self, _path: &str, exact: PathBuf) -> PathBuf {
        exact
    }

    #[cfg(not(windows))]
    fn resolve_fold_fallback(&self, path: &str, exact: PathBuf) -> PathBuf {
        resolve_fold_equal(&self.root, path).unwrap_or(exact)
    }
}

/// Resolve `rel` against `base` when the host filesystem is case-sensitive.
///
/// Not a cheap function: calling it at all costs at least one `stat` and one
/// `PathBuf` allocation, and a genuine fold miss costs one of each per path
/// component. [`DiskProvider::resolve_case_aware`] (a create, which must
/// resolve eagerly) and [`DiskProvider::resolve_fold_fallback`] (everything
/// else, which resolves lazily and only reaches this function after the
/// byte-exact spelling has already been tried and missed) both exist so this
/// is called only when that cost is actually needed — never on a hit. This
/// is what Wine does, and what `ciopfs` exists to avoid doing repeatedly.
///
/// Compares with [`vfs_core::fold`], never `to_ascii_lowercase`, and never
/// hands a folded spelling to the filesystem: `casefold.rs` warns the fold is
/// not NTFS-case-equivalence (`İ` folds to a genuinely different name), so the
/// resolved *original* entry name is what gets opened.
///
/// `rel` is assumed already validated by the caller (`resolve_case_aware`
/// calls `resolve` first, which rejects a bare `..` component) — but a `..`
/// encountered here is still treated as an unmatched component rather than
/// walked, so this function can never become an escape hatch around that
/// check on its own.
///
/// Two or more siblings can be fold-equal on a case-sensitive filesystem
/// (`A.esp` and `a.esp` coexisting is legal on ext4, impossible on NTFS).
/// When that happens the lexicographically smallest byte spelling wins, so
/// the choice is deterministic rather than dependent on `read_dir`'s
/// unspecified order — not a claim that it is the "right" one, since there
/// isn't a right one once the source directory itself is ambiguous.
#[cfg(not(windows))]
fn resolve_fold_equal(base: &std::path::Path, rel: &str) -> Option<PathBuf> {
    // Exact spelling first: this still costs a `PathBuf` allocation and a
    // `stat`, same as any other attempt at this path, but when it hits, the
    // entry it names is unambiguously the right one and nothing below runs.
    // A bare `..` component is rejected here exactly as the miss loop below
    // rejects it (rather than being pushed and walked), so the doc comment
    // above about this function never being an escape hatch around
    // `resolve`'s `..` check is true of this branch too, not just the miss
    // loop.
    let mut exact = base.to_path_buf();
    for part in rel.split('/') {
        if part.is_empty() {
            continue;
        }
        if part == ".." {
            return None;
        }
        exact.push(part);
    }
    if std::fs::symlink_metadata(&exact).is_ok() {
        return Some(exact);
    }

    // Miss: walk components one at a time. `cur` accumulates the real
    // on-disk path resolved so far. The moment a component cannot be
    // resolved against the filesystem — it does not exist under any
    // spelling, or `cur` turned out not to be a directory — stop resolving
    // and keep that component, and everything after it, exactly as given.
    // That is the common "this is a create, the target does not exist yet"
    // case, and the point of stopping rather than giving up entirely is
    // that the ancestry already resolved (by exact or fold match) must not
    // be thrown away: a caller creating `data/new.txt` under an existing
    // `Data/` must land inside `Data/`, not spawn a second, divergent
    // `data/` directory beside it.
    let mut cur = base.to_path_buf();
    let mut parts = rel.split('/').filter(|p| !p.is_empty());
    for part in parts.by_ref() {
        if part == ".." {
            return None;
        }
        let candidate = cur.join(part);
        if std::fs::symlink_metadata(&candidate).is_ok() {
            cur = candidate;
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&cur) else {
            cur.push(part);
            break;
        };
        let folded_part = vfs_core::fold(part);
        let mut best: Option<std::ffi::OsString> = None;
        for entry in entries.flatten() {
            let name = entry.file_name();
            // Skip, don't `to_string_lossy()`: `part` came from a UTF-8
            // vpath, so a non-UTF-8 on-disk name can never be the fold-equal
            // match we're looking for, and lossily replacing its invalid
            // bytes with U+FFFD would be a bug here specifically — two
            // different non-UTF-8 names can both replace to the same
            // sequence of replacement characters, which then fold-match
            // each other (and possibly `folded_part`) even though the real
            // names never did.
            let Some(name_str) = name.to_str() else {
                continue;
            };
            if vfs_core::fold(name_str) != folded_part {
                continue;
            }
            best = match best {
                Some(prev) if prev <= name => Some(prev),
                _ => Some(name),
            };
        }
        match best {
            Some(name) => cur.push(name),
            None => {
                cur.push(part);
                break;
            }
        }
    }
    // Anything left unconsumed (because the loop above broke out early)
    // carries over verbatim: no more of it has a real on-disk entry to
    // resolve against.
    for rest in parts {
        cur.push(rest);
    }
    Some(cur)
}

impl Provider for DiskProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            access: Access::ReadWrite,
            immutable: false, // a real directory can change underneath us
            slow: false,
            preferred_block: None,
            // True for free on Windows (NTFS folds); true by construction
            // elsewhere via `resolve_case_aware`'s fold-scan.
            case: CaseMatch::Insensitive,
        }
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        let path = p.rel;
        // Lazy resolution: `getattr` expects its target to already exist
        // (or genuinely not to), so it tries the byte-exact path first and
        // only pays for the fold-aware fallback on an actual miss — a hit
        // costs exactly the one `stat` this call was always going to make.
        let exact = self.resolve(path)?;
        let meta = match std::fs::metadata(&exact) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let folded = self.resolve_fold_fallback(path, exact);
                match std::fs::metadata(&folded) {
                    Ok(m) => m,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(_) => return Err(map_io_err()),
                }
            }
            Err(_) => return Err(map_io_err()),
        };
        if meta.is_dir() {
            Ok(Some(Stat {
                kind: KIND_DIR,
                size: 0,
                mtime: 0,
            }))
        } else if meta.is_file() {
            Ok(Some(Stat {
                kind: KIND_FILE,
                size: meta.len(),
                mtime: 0,
            }))
        } else {
            Ok(None)
        }
    }

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        let path = p.rel;
        // Lazy resolution, same as `getattr`: the directory being listed is
        // expected to already exist, so the fold-aware fallback is paid for
        // only when the byte-exact spelling actually misses.
        let exact = self.resolve(path)?;
        let rd = match std::fs::read_dir(&exact) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let folded = self.resolve_fold_fallback(path, exact);
                std::fs::read_dir(&folded).map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        not_found()
                    } else if e.kind() == std::io::ErrorKind::NotADirectory {
                        not_a_dir()
                    } else {
                        map_io_err()
                    }
                })?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => return Err(not_a_dir()),
            Err(_) => return Err(map_io_err()),
        };
        let mut out = Vec::new();
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            let meta = ent.metadata().ok();
            let (kind, size) = match meta {
                Some(m) if m.is_dir() => (KIND_DIR, 0),
                Some(m) => (KIND_FILE, m.len()),
                None => continue,
            };
            out.push(DirEntry {
                name,
                stat: Stat {
                    kind,
                    size,
                    mtime: 0,
                },
            });
        }
        // Folded, not ASCII-lowercased: every other listing in this stack
        // orders by `vfs_core::fold`, and two providers whose entries are
        // merged must agree on the ordering they were sorted by.
        out.sort_by_key(|a| vfs_core::fold(&a.name));
        Ok(out)
    }

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        let path = p.rel;
        let creating = flags & OPEN_CREATE != 0;

        if flags & OPEN_WRITE == 0 {
            // Read-only open: the target is expected to already exist, so
            // resolve lazily — byte-exact first, fold-aware fallback only
            // on an actual miss.
            let exact = self.resolve(path)?;
            let (p, meta) = match std::fs::metadata(&exact) {
                Ok(m) => (exact, m),
                Err(_) => {
                    let folded = self.resolve_fold_fallback(path, exact);
                    let m = std::fs::metadata(&folded).map_err(|_| not_found())?;
                    (folded, m)
                }
            };
            if meta.is_dir() {
                let bh = self.next.fetch_add(1, Ordering::Relaxed);
                return Ok((bh, 0, true));
            }
            let f = File::open(&p).map_err(|_| map_io_err())?;
            let size = meta.len();
            let bh = self.next.fetch_add(1, Ordering::Relaxed);
            self.opens.lock().map_err(|_| map_io_err())?.insert(bh, f);
            return Ok((bh, size, false));
        }

        // `OPEN_WRITE` from here on. A create must resolve eagerly: the
        // eager fold-aware resolution is what lands a create inside an
        // existing, differently-cased directory instead of forking a
        // divergent sibling (see
        // `open_create_under_a_fold_equal_directory_does_not_fork_it`).
        // Trying the byte-exact path first and creating there on a "miss"
        // would never see a miss to fall back from — `OpenOptions::create`
        // succeeds by creating exactly that path. A plain write-open of an
        // already-existing file has no such hazard and resolves lazily,
        // same as the read branch above.
        let mut p = if creating { self.resolve_case_aware(path)? } else { self.resolve(path)? };

        if creating {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).map_err(|_| map_io_err())?;
            }
        }

        let open_at = |target: &PathBuf| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(creating)
                .create_new(flags & OPEN_EXCL != 0)
                .truncate(flags & OPEN_TRUNC != 0)
                .open(target)
        };

        let mut opened = open_at(&p);
        if !creating && matches!(&opened, Err(e) if e.kind() == std::io::ErrorKind::NotFound) {
            p = self.resolve_fold_fallback(path, p);
            opened = open_at(&p);
        }
        let f = opened.map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => not_found(),
            // `OPEN_EXCL` (create_new) against an existing path. Without
            // this arm it fell into the generic `map_io_err()` below,
            // indistinguishable from a real I/O failure — and the shim
            // then treated *any* write-open error as "director refused,
            // fall through to the overlay", so an exclusive create
            // against an existing file silently created it in the
            // overlay and reported success instead of failing.
            std::io::ErrorKind::AlreadyExists => exists(),
            _ => map_io_err(),
        })?;
        let size = f.metadata().map_err(|_| map_io_err())?.len();
        let bh = self.next.fetch_add(1, Ordering::Relaxed);
        self.opens.lock().map_err(|_| map_io_err())?.insert(bh, f);
        Ok((bh, size, false))
    }

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let mut g = self.opens.lock().map_err(|_| map_io_err())?;
        let f = g.get_mut(&h).ok_or_else(crate::ops::bad_fh)?;
        f.seek(SeekFrom::Start(offset)).map_err(|_| map_io_err())?;
        f.read(buf).map_err(|_| map_io_err())
    }

    fn write_at(&self, h: Handle, offset: u64, buf: &[u8]) -> Result<usize, i32> {
        let mut g = self.opens.lock().map_err(|_| map_io_err())?;
        let f = g.get_mut(&h).ok_or_else(crate::ops::bad_fh)?;
        f.seek(SeekFrom::Start(offset)).map_err(|_| map_io_err())?;
        f.write(buf).map_err(|_| map_io_err())
    }

    fn set_len(&self, h: Handle, len: u64) -> Result<(), i32> {
        let g = self.opens.lock().map_err(|_| map_io_err())?;
        let f = g.get(&h).ok_or_else(crate::ops::bad_fh)?;
        f.set_len(len).map_err(|_| map_io_err())
    }

    fn flush(&self, h: Handle) -> Result<(), i32> {
        let g = self.opens.lock().map_err(|_| map_io_err())?;
        let f = g.get(&h).ok_or_else(crate::ops::bad_fh)?;
        f.sync_all().map_err(|_| map_io_err())
    }

    fn mkdir(&self, p: VPath) -> Result<(), i32> {
        let path = self.resolve_case_aware(p.rel)?;
        std::fs::create_dir_all(&path).map_err(|_| map_io_err())
    }

    fn remove(&self, p: VPath) -> Result<(), i32> {
        let path = self.resolve_case_aware(p.rel)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(not_found()),
            Err(_) => {
                // Windows reports "remove_file'd a directory" as plain
                // PermissionDenied — indistinguishable by ErrorKind from a
                // genuinely locked or permission-denied file. Consult
                // metadata to confirm this is actually a directory before
                // falling back, so a real file-removal failure is reported
                // as its own status instead of being replaced by whatever
                // remove_dir happens to return for a path that isn't one.
                if !std::fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false) {
                    return Err(map_io_err());
                }
                std::fs::remove_dir(&path).map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        not_found()
                    } else {
                        map_io_err()
                    }
                })
            }
        }
    }

    fn rename(&self, from: VPath, to: VPath) -> Result<(), i32> {
        if from.root != to.root {
            return Err(bad_request());
        }
        let from_path = self.resolve_case_aware(from.rel)?;
        let to_path = self.resolve_case_aware(to.rel)?;
        std::fs::rename(&from_path, &to_path).map_err(|_| map_io_err())
    }

    fn set_attr(&self, p: VPath, attr: SetAttr) -> Result<(), i32> {
        if attr.size.is_none() && attr.mtime.is_none() {
            return Ok(());
        }
        // Lazy resolution, same as `getattr`: `set_attr`'s target is
        // expected to already exist.
        let exact = self.resolve(p.rel)?;
        let f = match File::options().write(true).open(&exact) {
            Ok(f) => f,
            Err(_) => {
                let folded = self.resolve_fold_fallback(p.rel, exact);
                File::options().write(true).open(&folded).map_err(|_| map_io_err())?
            }
        };
        if let Some(size) = attr.size {
            f.set_len(size).map_err(|_| map_io_err())?;
        }
        if let Some(mtime) = attr.mtime {
            let time = std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(mtime.max(0) as u64);
            let times = std::fs::FileTimes::new().set_modified(time);
            f.set_times(times).map_err(|_| map_io_err())?;
        }
        Ok(())
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        let mut g = self.opens.lock().map_err(|_| map_io_err())?;
        // Dir opens may not be in the map.
        let _ = g.remove(&h);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_provider_declares_read_write() {
        use vfs_provider::{Access, Provider};
        let dir = std::env::temp_dir().join(format!("vfs-diskrw-{}", std::process::id()));
        vfs_provider::write_fixture_tree(&dir);
        let p = DiskProvider::new(&dir);
        let caps = p.capabilities();
        assert_eq!(caps.access, Access::ReadWrite);
        assert!(!caps.immutable, "a real directory can change underneath us");
        caps.validate().expect("ReadWrite must not claim immutable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_provider_passes_write_conformance() {
        let dir = std::env::temp_dir().join(format!("vfs-diskwconf-{}", std::process::id()));
        vfs_provider::write_fixture_tree(&dir);
        let p: std::sync::Arc<dyn vfs_provider::Provider> = std::sync::Arc::new(DiskProvider::new(&dir));
        vfs_provider::assert_conformance(p);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_rejects_a_dotdot_component() {
        let dir = std::env::temp_dir().join(format!("vfs-diskdotdot-{}", std::process::id()));
        vfs_provider::write_fixture_tree(&dir);

        assert_eq!(
            DiskProvider::new(&dir).resolve("../escape.txt"),
            Err(bad_request()),
            "a leading .. component must be refused, not walked"
        );
        assert_eq!(
            DiskProvider::new(&dir).resolve("sub/../../escape.txt"),
            Err(bad_request()),
            "a .. component buried mid-path must be refused too"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_accepts_a_filename_that_merely_starts_with_dotdot() {
        let dir = std::env::temp_dir().join(format!("vfs-diskdotdotfoo-{}", std::process::id()));
        vfs_provider::write_fixture_tree(&dir);

        let resolved = DiskProvider::new(&dir)
            .resolve("..foo")
            .expect("..foo is a legitimate filename, not a .. component");
        assert_eq!(resolved, dir.join("..foo"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dotdot_escaping_open_is_refused_and_creates_nothing() {
        // Guards the OPEN_CREATE escalation directly: before this fix, an
        // OPEN_CREATE with a .. component would create_dir_all a directory
        // outside root. Confirm the parent of `dir` gains nothing.
        let parent = std::env::temp_dir();
        let dir = parent.join(format!("vfs-diskdotdotopen-{}", std::process::id()));
        vfs_provider::write_fixture_tree(&dir);
        let sibling = parent.join(format!("vfs-diskdotdotopen-{}-escaped", std::process::id()));
        let _ = std::fs::remove_dir_all(&sibling);

        use vfs_provider::{Provider, OPEN_CREATE, OPEN_WRITE};
        let p = DiskProvider::new(&dir);
        let escaping = format!(
            "../{}/pwned.txt",
            sibling.file_name().unwrap().to_string_lossy()
        );
        let result = p.open(VPath::at_default(&escaping), OPEN_WRITE | OPEN_CREATE);
        assert_eq!(result, Err(bad_request()));
        assert!(
            !sibling.exists(),
            "a .. escaping OPEN_CREATE must not create anything outside root"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&sibling);
    }

    /// Fold-equal resolution must not depend on the host filesystem. On Windows
    /// NTFS satisfies this for free; on Linux over ext4 nothing does, and a
    /// FUSE mount serving a Windows program needs it either way.
    #[test]
    fn fold_equal_spellings_resolve_on_any_filesystem() {
        let dir = std::env::temp_dir().join(format!("vfs-disk-fold-{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("Data"));
        std::fs::write(dir.join("Data").join("A.esp"), b"body").unwrap();

        let p = DiskProvider::new(&dir);
        for spelling in ["Data/A.esp", "data/a.esp", "DATA/A.ESP"] {
            let st = p
                .getattr(VPath::at_default(spelling))
                .unwrap()
                .unwrap_or_else(|| panic!("{spelling} did not resolve"));
            assert_eq!(st.size, 4, "{spelling} resolved to the wrong entry");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Creating through a fold-equal spelling of an *existing* directory must
    /// land inside that directory, not fork a second, divergently-cased one
    /// beside it. A fold-scan that gives up entirely on a create (because the
    /// leaf being created cannot itself be found) and falls back to a fully
    /// byte-exact path would throw away the already-resolved ancestor and
    /// reintroduce exactly the divergence this task exists to prevent — this
    /// pins that the ancestor resolution survives the leaf being a miss.
    #[test]
    fn open_create_under_a_fold_equal_directory_does_not_fork_it() {
        let dir = std::env::temp_dir().join(format!("vfs-diskcreatefold-{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("Data"));

        use vfs_provider::{Provider, OPEN_CREATE, OPEN_WRITE};
        let p = DiskProvider::new(&dir);
        let (h, _len, _is_dir) = p
            .open(VPath::at_default("data/new.esp"), OPEN_WRITE | OPEN_CREATE)
            .expect("create through a fold-equal directory spelling must succeed");
        p.close(h).expect("close");

        let top: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            top.iter().filter(|n| vfs_core::fold(n) == "data").count(),
            1,
            "creating through the fold-equal spelling `data` must land inside \
             the existing `Data`, not spawn a second, divergently-cased \
             directory: {top:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

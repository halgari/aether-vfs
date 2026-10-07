//! Root links in a prefix's `drive_c` and the manifest that records them.

use std::io;
use std::path::{Path, PathBuf};

use super::{Prefix, PrefixError};

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prefix::scratch;

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
        let p = Prefix {
            dir: scratch("ll-case"),
        };
        std::fs::create_dir_all(p.drive_c().join("users").join("steamuser")).unwrap();
        let target = scratch("ll-case-target");
        let link = p
            .link_location(r"C:\Users\SteamUser\Saves", &target)
            .unwrap();
        assert_eq!(
            link,
            p.drive_c().join("users").join("steamuser").join("Saves")
        );
        assert_eq!(std::fs::read_link(&link).unwrap(), target);
        assert!(
            !p.drive_c().join("Users").exists(),
            "an existing parent must be reused, not shadowed by a sibling spelled differently"
        );
        // Relinking under another spelling finds and replaces our own link.
        let target2 = scratch("ll-case-target2");
        let link2 = p
            .link_location(r"c:\USERS\steamuser\saves", &target2)
            .unwrap();
        let entries: Vec<_> = std::fs::read_dir(p.drive_c().join("users").join("steamuser"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "the old link must be replaced, not joined: {entries:?}"
        );
        assert_eq!(std::fs::read_link(&link2).unwrap(), target2);
    }

    #[cfg(unix)]
    #[test]
    fn link_location_refuses_a_real_directory_spelled_differently() {
        let p = Prefix {
            dir: scratch("ll-case-real"),
        };
        let real = p.drive_c().join("games").join("mine");
        std::fs::create_dir_all(&real).unwrap();
        let err = p
            .link_location(r"C:\Games\Mine", &scratch("ll-case-t"))
            .unwrap_err();
        assert!(matches!(err, PrefixError::BadLocation(_)), "{err}");
        assert!(real.is_dir() && !real.is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn link_location_refuses_to_replace_a_real_directory() {
        let p = Prefix {
            dir: scratch("ll-real"),
        };
        let real = p.drive_c().join("Games").join("Mine");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("keep.txt"), b"keep").unwrap();
        let err = p
            .link_location(r"C:\Games\Mine", &scratch("ll-t"))
            .unwrap_err();
        assert!(matches!(err, PrefixError::BadLocation(_)), "{err}");
        assert!(
            real.join("keep.txt").is_file(),
            "a real directory must never be removed"
        );
    }

    /// A user's own symlink at a root location in a persistent prefix is not
    /// ours to replace: refused like a real directory, and it survives.
    #[cfg(unix)]
    #[test]
    fn link_location_refuses_a_symlink_it_did_not_create() {
        let p = Prefix {
            dir: scratch("ll-foreign"),
        };
        let theirs = scratch("ll-foreign-theirs");
        std::fs::create_dir_all(p.drive_c().join("Games")).unwrap();
        let at = p.drive_c().join("Games").join("Skyrim");
        std::os::unix::fs::symlink(&theirs, &at).unwrap();
        let err = p
            .link_location(r"C:\Games\Skyrim", &scratch("ll-foreign-t"))
            .unwrap_err();
        assert!(
            matches!(&err, PrefixError::BadLocation(m) if m.contains("did not create")),
            "{err}"
        );
        assert_eq!(
            std::fs::read_link(&at).unwrap(),
            theirs,
            "their link must survive"
        );
        assert!(p.read_manifest().unwrap().is_empty());
        // Nor does `unlink_location` touch it, even naming its exact target.
        assert!(!p.unlink_location(&at, &theirs).unwrap());
        assert_eq!(std::fs::read_link(&at).unwrap(), theirs);
    }

    #[cfg(unix)]
    #[test]
    fn our_links_are_recorded_and_unlink_removes_them_from_the_manifest() {
        let p = Prefix {
            dir: scratch("ll-manifest"),
        };
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
        assert_eq!(
            parse_location(r"C:\Games\Fixture").unwrap(),
            ["Games", "Fixture"]
        );
        assert_eq!(
            parse_location("c:/Games/./Fixture/").unwrap(),
            ["Games", "Fixture"]
        );
        for bad in [
            r"D:\Games",
            r"C:\a\..\b",
            r"C:\",
            "C:",
            "Games",
            "/tmp/x",
            r"\\srv\share\x",
            "",
        ] {
            match parse_location(bad) {
                Err(PrefixError::BadLocation(m)) => assert!(m.starts_with(bad), "{bad}: {m}"),
                other => panic!("{bad:?} must be refused, got {other:?}"),
            }
        }
    }

    #[test]
    fn link_location_refuses_other_drives_and_dot_dot() {
        let p = Prefix {
            dir: scratch("ll-bad"),
        };
        for bad in [r"D:\Games", r"C:\a\..\b", r"C:\", "Games", r"\\srv\share\x"] {
            assert!(
                matches!(
                    p.link_location(bad, Path::new("/tmp")),
                    Err(PrefixError::BadLocation(_))
                ),
                "{bad} must be refused"
            );
        }
    }
}

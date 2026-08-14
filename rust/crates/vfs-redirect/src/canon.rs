//! Canonicalise every NT spelling of a path to one form, so a device path,
//! long-UNC path, or volume-GUID path cannot slip past the shim's redirect
//! decision by looking unfamiliar.
//!
//! Pure: no filesystem access, no Windows API. Resolving `\Device\...` and
//! `\\?\Volume{...}` prefixes to a drive letter needs the OS (`QueryDosDevice`
//! and friends), so that lookup table is built elsewhere and handed in as a
//! [`VolumeMap`]. An unmapped device prefix is left untouched rather than
//! guessed at: see `an_unmapped_device_does_not_silently_become_a_drive`
//! below — treating an unrecognised device as some drive would make the VFS
//! start intercepting paths that were never under the managed root.

use vfs_core::{normalize_vpath, PathError};

/// NT/DOS long-path prefixes `normalize_vpath` also recognises, in both
/// slash forms. Used here only to find where a drive letter or device name
/// starts, not to strip them permanently — final stripping is
/// `normalize_vpath`'s job.
const NT_PREFIXES: [&str; 4] = [r"\??\", r"\\?\", "/??/", "//?/"];

/// A table from NT device/volume-GUID prefixes to the drive letter they are
/// currently mounted as. Built by resolving the OS's device namespace
/// (`Task 2`); `empty()` is for tests that exercise no device paths.
#[derive(Debug, Clone, Default)]
pub struct VolumeMap {
    entries: Vec<(String, char)>,
}

impl VolumeMap {
    /// A map with no device or volume-GUID prefixes registered.
    pub fn empty() -> Self {
        VolumeMap { entries: Vec::new() }
    }

    /// Register an NT device prefix (e.g. `\Device\HarddiskVolume3`) or a
    /// volume-GUID prefix (e.g. `\\?\Volume{guid}`) as currently mounted on
    /// `drive`.
    pub fn insert(&mut self, prefix: &str, drive: char) {
        self.entries.push((prefix.to_string(), drive));
    }

    /// If `path` starts with a registered prefix at a component boundary
    /// (the match ends the string or is followed by a separator, so
    /// `HarddiskVolume3` cannot match a path actually naming
    /// `HarddiskVolume30`), the drive letter and the byte length of the
    /// matched prefix. Longest match wins if more than one registered
    /// prefix matches.
    fn resolve(&self, path: &str) -> Option<(char, usize)> {
        let mut best: Option<(char, usize)> = None;
        for (prefix, drive) in &self.entries {
            let Some(rest) = path.strip_prefix(prefix.as_str()) else {
                continue;
            };
            if !rest.is_empty() && !rest.starts_with(['\\', '/']) {
                continue;
            }
            let is_longer = match best {
                Some((_, len)) => prefix.len() > len,
                None => true,
            };
            if is_longer {
                best = Some((*drive, prefix.len()));
            }
        }
        best
    }
}

/// The byte length of a recognised NT/DOS prefix at the start of `s`, or 0.
fn nt_prefix_len(s: &str) -> usize {
    NT_PREFIXES.iter().find(|p| s.starts_with(*p)).map_or(0, |p| p.len())
}

/// Split off a trailing alternate-data-stream suffix (`:stream` or
/// `:stream:$DATA`), returning the path before it. Must run before any
/// drive-letter or device-prefix inspection: a colon inside a stream name
/// would otherwise look like (or be confused with) a drive-letter colon.
///
/// A drive-letter colon (`C:`) — right after any NT/DOS prefix that is
/// present — is not itself a stream separator; the first colon *after* it
/// is.
fn strip_stream_suffix(raw: &str) -> &str {
    let prefix_len = nt_prefix_len(raw);
    let rest = &raw[prefix_len..];
    let rest_bytes = rest.as_bytes();
    let has_drive_colon =
        rest_bytes.len() >= 2 && rest_bytes[0].is_ascii_alphabetic() && rest_bytes[1] == b':';
    let search_from = prefix_len + if has_drive_colon { 2 } else { 0 };
    match raw[search_from..].find(':') {
        Some(rel) => &raw[..search_from + rel],
        None => raw,
    }
}

/// Resolve a leading `\Device\...` or `\\?\Volume{...}` prefix to its drive
/// letter via `volumes`. Paths with no such prefix, or with a prefix not
/// present in `volumes`, are returned unchanged — an unmapped device must
/// never be guessed into a drive.
fn resolve_device_prefix(path: &str, volumes: &VolumeMap) -> String {
    match volumes.resolve(path) {
        Some((drive, matched_len)) => format!("{drive}:{}", &path[matched_len..]),
        None => path.to_string(),
    }
}

/// Strip trailing dots and spaces from each path component, the way Win32
/// silently discards them when resolving a name. `.` and `..` are left
/// alone — they are navigation, not a name Win32 would trim — so
/// `normalize_vpath`'s dot-dot handling still sees them intact.
fn strip_trailing_punctuation(path: &str) -> String {
    path.split(['/', '\\'])
        .map(|comp| {
            if comp == "." || comp == ".." {
                comp
            } else {
                comp.trim_end_matches(['.', ' '])
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Canonicalise one NT open path to a single form: strip any alternate-data-
/// stream suffix, resolve a device or volume-GUID prefix to its drive letter
/// via `volumes`, strip trailing per-component dots/spaces, then hand the
/// result to [`normalize_vpath`] for separator folding and `.`/`..`
/// resolution. Every spelling of the same file must come out identical.
///
/// Case is preserved: folding case is the caller's job (`RootMap` already
/// folds for comparison), and `DiskProvider` needs the original case to open
/// the real file on disk.
pub fn canonicalise(raw: &str, volumes: &VolumeMap) -> Result<String, PathError> {
    let no_stream = strip_stream_suffix(raw);
    let resolved = resolve_device_prefix(no_stream, volumes);
    let trimmed = strip_trailing_punctuation(&resolved);
    normalize_vpath(&trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vols() -> VolumeMap {
        let mut v = VolumeMap::empty();
        v.insert(r"\Device\HarddiskVolume3", 'C');
        v.insert(r"\\?\Volume{12345678-1234-1234-1234-123456789abc}", 'C');
        v
    }

    /// Every spelling of the same file must produce one canonical form.
    #[test]
    fn all_spellings_agree() {
        let want = "c:/games/skyrim/data/a.esp";
        for raw in [
            r"C:\Games\Skyrim\Data\a.esp",
            r"\??\C:\Games\Skyrim\Data\a.esp",
            r"\\?\C:\Games\Skyrim\Data\a.esp",
            r"\Device\HarddiskVolume3\Games\Skyrim\Data\a.esp",
            r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\Games\Skyrim\Data\a.esp",
            r"C:\Games\Skyrim\Data\.\a.esp",
            r"C:\Games\Skyrim\Other\..\Data\a.esp",
            r"C:/Games/Skyrim/Data/a.esp",
            r"C:\Games\Skyrim\Data\\a.esp",
            r"C:\GAMES\skyrim\DATA\A.ESP",
        ] {
            assert_eq!(
                canonicalise(raw, &vols()).unwrap().to_ascii_lowercase(),
                want,
                "spelling did not canonicalise: {raw}"
            );
        }
    }

    /// A stream suffix names the same file; the stream is not part of the path.
    #[test]
    fn strips_an_alternate_data_stream_suffix() {
        assert_eq!(
            canonicalise(r"C:\Games\Skyrim\Data\a.esp:evil", &vols()).unwrap().to_ascii_lowercase(),
            "c:/games/skyrim/data/a.esp"
        );
    }

    /// Win32 discards trailing dots and spaces; a path that differs only by
    /// them names the same file and must not escape by looking different.
    #[test]
    fn strips_trailing_dots_and_spaces_per_component() {
        for raw in [
            r"C:\Games\Skyrim\Data.\a.esp",
            r"C:\Games\Skyrim\Data \a.esp",
            r"C:\Games\Skyrim\Data\a.esp.",
        ] {
            assert_eq!(
                canonicalise(raw, &vols()).unwrap().to_ascii_lowercase(),
                "c:/games/skyrim/data/a.esp",
                "trailing punctuation was not stripped: {raw}"
            );
        }
    }

    /// A drive letter must not be confused with a volume it is not mapped to.
    #[test]
    fn an_unmapped_device_does_not_silently_become_a_drive() {
        let raw = r"\Device\HarddiskVolume9\Games\Skyrim\Data\a.esp";
        let got = canonicalise(raw, &vols()).unwrap();
        assert!(
            !got.to_ascii_lowercase().starts_with("c:"),
            "an unmapped device resolved to C: — {got}"
        );
    }

    /// `..` may not climb out of the path entirely.
    #[test]
    fn escaping_dotdot_is_refused() {
        assert!(canonicalise(r"C:\..\..\Windows\System32\evil.dll", &vols()).is_err());
    }
}

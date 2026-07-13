#![forbid(unsafe_code)]

//! The injected shim's pure redirect-decision core: map an incoming NT open path
//! + a published snapshot to pass-through vs redirect-to-backing-file.

use vfs_core::{fold, normalize_vpath, PathError};
use vfs_shared::{SnapResolution, SnapshotReader};

/// The managed VFS install root (mount point), as normalized path components.
pub struct RootMap {
    /// Normalized root components in original case, e.g. `["C:", "Games", "Skyrim"]`.
    root: Vec<String>,
}

impl RootMap {
    /// `root` may be NT (`\??\C:\Games\Skyrim`) or Win32 (`C:\Games\Skyrim`).
    pub fn new(root: &str) -> Result<Self, PathError> {
        let norm = normalize_vpath(root)?;
        let root = if norm.is_empty() {
            Vec::new()
        } else {
            norm.split('/').map(str::to_string).collect()
        };
        Ok(RootMap { root })
    }

    /// The normalized root components (original case). For tests/diagnostics.
    pub fn root_components(&self) -> &[String] {
        &self.root
    }

    /// Decide how to handle an incoming NT open path.
    ///
    /// Fail-safe: any path that is malformed, outside the root, or does not
    /// positively resolve to a virtualized file yields `PassThrough`.
    pub fn decide(&self, nt_path: &str, snap: &SnapshotReader) -> Decision {
        match self.locate(nt_path, snap) {
            Located::Resolved(SnapResolution::File { source, .. }) => {
                Decision::Redirect { target_nt: render_nt(&source) }
            }
            Located::Resolved(SnapResolution::Tombstone) => Decision::Deny,
            Located::Resolved(SnapResolution::Dir)
            | Located::Resolved(SnapResolution::NotFound)
            | Located::Outside => Decision::PassThrough,
        }
    }

    /// Normalize + case-insensitively match the root + resolve the remainder.
    fn locate(&self, nt_path: &str, snap: &SnapshotReader) -> Located {
        let norm = match normalize_vpath(nt_path) {
            Ok(n) => n,
            Err(_) => return Located::Outside,
        };
        let comps: Vec<&str> =
            if norm.is_empty() { Vec::new() } else { norm.split('/').collect() };
        if comps.len() < self.root.len() {
            return Located::Outside;
        }
        for (r, c) in self.root.iter().zip(comps.iter()) {
            if fold(r) != fold(c) {
                return Located::Outside;
            }
        }
        let folded: Vec<String> = comps[self.root.len()..].iter().map(|c| fold(c)).collect();
        let folded_refs: Vec<&str> = folded.iter().map(String::as_str).collect();
        Located::Resolved(snap.resolve(&folded_refs))
    }
}

/// Where an NT path lands relative to the managed root.
enum Located {
    /// Not under the root, or malformed/escaping — never virtualized.
    Outside,
    /// Under the root; here is the snapshot's answer for the remainder.
    Resolved(SnapResolution),
}

/// Render a backing `source` (a UTF-8 absolute Win32 path, per the director's
/// contract) as an NT DOS-device path. A `source` already carrying an NT/DOS
/// long-path prefix is returned unchanged rather than double-prefixed.
fn render_nt(source: &[u8]) -> String {
    let s = String::from_utf8_lossy(source);
    if s.starts_with(r"\??\") || s.starts_with(r"\\?\") {
        s.into_owned()
    } else {
        format!(r"\??\{s}")
    }
}

/// Decode a length-counted UTF-16 buffer (a `UNICODE_STRING` body) to a `String`.
/// Lossy: unpaired surrogates become U+FFFD rather than panicking.
pub fn utf16_to_string(units: &[u16]) -> String {
    String::from_utf16_lossy(units)
}

/// Encode a `&str` as UTF-16 with NO trailing NUL (`UNICODE_STRING` is counted).
pub fn string_to_utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// The outcome of inspecting one NT open path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Let the original NT open proceed unchanged.
    PassThrough,
    /// Reissue the open against this NT path (the mod backing file).
    Redirect { target_nt: String },
    /// The path is tombstoned (mod-deleted); the hook must return
    /// STATUS_OBJECT_NAME_NOT_FOUND rather than open or pass through.
    Deny,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_normalizes_nt_and_win32_roots() {
        // Both forms normalize to the same component vector.
        let nt = RootMap::new(r"\??\C:\Games\Skyrim").unwrap();
        let win32 = RootMap::new(r"C:\Games\Skyrim").unwrap();
        assert_eq!(nt.root_components(), win32.root_components());
        assert_eq!(nt.root_components(), vec!["C:", "Games", "Skyrim"]);
    }

    #[test]
    fn utf16_round_trips() {
        let s = "C:\\Games\\Skyrim\\Data\\foo.esp";
        assert_eq!(utf16_to_string(&string_to_utf16(s)), s);
        // No trailing NUL is appended.
        assert_eq!(*string_to_utf16("ab").last().unwrap(), b'b' as u16);
    }

    #[test]
    fn utf16_lossy_does_not_panic_on_unpaired_surrogate() {
        let units: [u16; 2] = [0xD800, b'x' as u16]; // lone high surrogate
        let _ = utf16_to_string(&units); // must not panic
    }

    use vfs_shared::SnapshotReader;

    // Build a snapshot with two virtual files under `data/`.
    fn snapshot_bytes() -> Vec<u8> {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let file = |vpath: &str, source: &str| InputEntry {
            vpath: vpath.into(),
            kind: EntryKind::File,
            source: source.into(),
            size: 10,
            mtime: 1,
        };
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![
                file("data/foo.esp", r"D:\Mods\Cool\foo.esp"),
                file("data/sub/bar.dds", r"D:\Mods\Cool\bar.dds"),
                InputEntry {
                    vpath: "data/deleted.esp".into(),
                    kind: EntryKind::Tombstone,
                    source: "".into(),
                    size: 0,
                    mtime: 0,
                },
            ],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    }

    fn root() -> RootMap {
        RootMap::new(r"\??\C:\Games\Skyrim").unwrap()
    }

    #[test]
    fn redirects_a_virtual_file() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let d = root().decide(r"\??\C:\Games\Skyrim\Data\foo.esp", &snap);
        assert_eq!(
            d,
            Decision::Redirect { target_nt: r"\??\D:\Mods\Cool\foo.esp".to_string() }
        );
    }

    #[test]
    fn redirect_is_case_insensitive_on_root_and_remainder() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let d = root().decide(r"\??\c:\games\SKYRIM\DATA\Foo.ESP", &snap);
        assert_eq!(
            d,
            Decision::Redirect { target_nt: r"\??\D:\Mods\Cool\foo.esp".to_string() }
        );
    }

    #[test]
    fn passes_through_outside_root() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let d = root().decide(r"\??\C:\Windows\System32\kernel32.dll", &snap);
        assert_eq!(d, Decision::PassThrough);
    }

    #[test]
    fn passes_through_under_root_but_not_virtualized() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let d = root().decide(r"\??\C:\Games\Skyrim\Data\notmod.esp", &snap);
        assert_eq!(d, Decision::PassThrough);
    }

    #[test]
    fn passes_through_a_virtual_directory() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let d = root().decide(r"\??\C:\Games\Skyrim\Data", &snap);
        assert_eq!(d, Decision::PassThrough);
    }

    #[test]
    fn passes_through_escaping_path_without_panic() {
        // Four `..` pop past the drive component, so normalize_vpath returns
        // PathError::EscapesRoot; decide must fail safe to PassThrough, not panic.
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let d = root().decide(r"\??\C:\Games\Skyrim\..\..\..\..\evil", &snap);
        assert_eq!(d, Decision::PassThrough);
    }

    #[test]
    fn win32_form_root_matches_nt_form_open() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let win32_root = RootMap::new(r"C:\Games\Skyrim").unwrap();
        let d = win32_root.decide(r"\??\C:\Games\Skyrim\Data\foo.esp", &snap);
        assert_eq!(
            d,
            Decision::Redirect { target_nt: r"\??\D:\Mods\Cool\foo.esp".to_string() }
        );
    }

    #[test]
    fn source_already_nt_prefixed_is_not_double_prefixed() {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "data/foo.esp".into(),
                kind: EntryKind::File,
                source: r"\??\D:\Mods\Cool\foo.esp".into(),
                size: 10,
                mtime: 1,
            }],
        }])
        .unwrap();
        let bytes = vfs_shared::bridge::flatten(&tree);
        let snap = SnapshotReader::open(&bytes).unwrap();
        let d = root().decide(r"\??\C:\Games\Skyrim\Data\foo.esp", &snap);
        assert_eq!(
            d,
            Decision::Redirect { target_nt: r"\??\D:\Mods\Cool\foo.esp".to_string() }
        );
    }

    #[test]
    fn decide_denies_a_tombstoned_path() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        assert_eq!(
            root().decide(r"\??\C:\Games\Skyrim\Data\deleted.esp", &snap),
            Decision::Deny
        );
    }
}

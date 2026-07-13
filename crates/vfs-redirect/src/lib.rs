#![forbid(unsafe_code)]

//! The injected shim's pure redirect-decision core: map an incoming NT open path
//! + a published snapshot to pass-through vs redirect-to-backing-file.

use std::collections::BTreeMap;

use vfs_core::{fold, normalize_vpath, wildcard_match, PathError};
use vfs_shared::{NodeKind, SnapResolution, SnapshotReader};

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

    /// Answer a path-based attribute query against the snapshot.
    pub fn query_attributes(&self, nt_path: &str, snap: &SnapshotReader) -> AttrDecision {
        match self.locate(nt_path, snap) {
            Located::Resolved(SnapResolution::File { size, mtime, .. }) => {
                AttrDecision::Attributes { is_dir: false, size, mtime }
            }
            Located::Resolved(SnapResolution::Dir) => {
                AttrDecision::Attributes { is_dir: true, size: 0, mtime: 0 }
            }
            Located::Resolved(SnapResolution::Tombstone) => AttrDecision::Deny,
            Located::Resolved(SnapResolution::NotFound) | Located::Outside => {
                AttrDecision::PassThrough
            }
        }
    }

    /// Folded remainder components if `nt_path` is under the managed root, else
    /// `None` (out of root, malformed, or escaping).
    fn under_root(&self, nt_path: &str) -> Option<Vec<String>> {
        let norm = normalize_vpath(nt_path).ok()?;
        let comps: Vec<&str> =
            if norm.is_empty() { Vec::new() } else { norm.split('/').collect() };
        if comps.len() < self.root.len() {
            return None;
        }
        for (r, c) in self.root.iter().zip(comps.iter()) {
            if fold(r) != fold(c) {
                return None;
            }
        }
        Some(comps[self.root.len()..].iter().map(|c| fold(c)).collect())
    }

    fn locate(&self, nt_path: &str, snap: &SnapshotReader) -> Located {
        match self.under_root(nt_path) {
            None => Located::Outside,
            Some(folded) => {
                let refs: Vec<&str> = folded.iter().map(String::as_str).collect();
                Located::Resolved(snap.resolve(&refs))
            }
        }
    }

    /// Merge a directory's real on-disk `real` entries with the snapshot's
    /// virtual children: overrides win, tombstones are hidden, `wildcard` filters
    /// the display names, output is case-insensitively ordered by folded name.
    pub fn merge_directory(
        &self,
        dir_nt_path: &str,
        snap: &SnapshotReader,
        real: &[DirItem],
        wildcard: Option<&str>,
    ) -> Vec<DirItem> {
        let mut map: BTreeMap<String, DirItem> = BTreeMap::new();
        for e in real {
            map.insert(fold(&e.name), e.clone());
        }
        if let Some(folded) = self.under_root(dir_nt_path) {
            let refs: Vec<&str> = folded.iter().map(String::as_str).collect();
            if let Ok(virt) = snap.readdir(&refs) {
                for v in virt {
                    let key = fold(&v.name);
                    match v.kind {
                        NodeKind::Tombstone => {
                            map.remove(&key);
                        }
                        NodeKind::Dir => {
                            map.insert(key, DirItem { name: v.name, is_dir: true, size: 0, mtime: 0 });
                        }
                        NodeKind::File => {
                            map.insert(
                                key,
                                DirItem { name: v.name, is_dir: false, size: v.size, mtime: v.mtime },
                            );
                        }
                    }
                }
            }
        }
        map.into_values()
            .filter(|e| match wildcard {
                Some(p) => wildcard_match(p, &e.name),
                None => true,
            })
            .collect()
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

/// One entry in a directory listing — used both for the caller's real on-disk
/// entries and for the merged result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirItem {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
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

/// The outcome of a path-based attribute query (NtQueryAttributesFile).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttrDecision {
    /// Let the original query proceed unchanged.
    PassThrough,
    /// Answer from the snapshot with these attributes.
    Attributes { is_dir: bool, size: u64, mtime: i64 },
    /// Tombstoned: return not-found rather than reveal a hidden real file.
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

    #[test]
    fn attrs_of_a_virtual_file() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        assert_eq!(
            root().query_attributes(r"\??\C:\Games\Skyrim\Data\foo.esp", &snap),
            AttrDecision::Attributes { is_dir: false, size: 10, mtime: 1 }
        );
    }

    #[test]
    fn attrs_of_a_virtual_directory() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        assert_eq!(
            root().query_attributes(r"\??\C:\Games\Skyrim\Data", &snap),
            AttrDecision::Attributes { is_dir: true, size: 0, mtime: 0 }
        );
    }

    #[test]
    fn attrs_of_a_tombstone_deny() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        assert_eq!(
            root().query_attributes(r"\??\C:\Games\Skyrim\Data\deleted.esp", &snap),
            AttrDecision::Deny
        );
    }

    #[test]
    fn attrs_under_root_not_virtualized_passes_through() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        assert_eq!(
            root().query_attributes(r"\??\C:\Games\Skyrim\Data\real.esp", &snap),
            AttrDecision::PassThrough
        );
    }

    #[test]
    fn attrs_outside_root_passes_through() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        assert_eq!(
            root().query_attributes(r"\??\C:\Windows\notepad.exe", &snap),
            AttrDecision::PassThrough
        );
    }

    #[test]
    fn attrs_are_case_insensitive() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        assert_eq!(
            root().query_attributes(r"\??\c:\games\SKYRIM\DATA\Foo.ESP", &snap),
            AttrDecision::Attributes { is_dir: false, size: 10, mtime: 1 }
        );
    }

    // data/ has: Mod.esp (add), Shared.esp (override, size 99), AddedDir (dir),
    // Deleted.esp (tombstone).
    fn merge_snapshot() -> Vec<u8> {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let mk = |vpath: &str, kind: EntryKind, size: u64| InputEntry {
            vpath: vpath.into(),
            kind,
            source: r"D:\Mods\X\f".into(),
            size,
            mtime: 7,
        };
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![
                mk("data/Mod.esp", EntryKind::File, 5),
                mk("data/Shared.esp", EntryKind::File, 99),
                mk("data/AddedDir", EntryKind::Dir, 0),
                mk("data/Deleted.esp", EntryKind::Tombstone, 0),
            ],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    }

    fn item(name: &str, is_dir: bool, size: u64) -> DirItem {
        DirItem { name: name.into(), is_dir, size, mtime: 0 }
    }

    fn names(v: &[DirItem]) -> Vec<String> {
        v.iter().map(|e| e.name.clone()).collect()
    }

    const DATA_NT: &str = r"\??\C:\Games\Skyrim\Data";

    #[test]
    fn merge_overrides_adds_and_hides_tombstones() {
        let bytes = merge_snapshot();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let real = vec![
            item("Shared.esp", false, 1), // overridden by virtual (size 99)
            item("Deleted.esp", false, 1), // tombstoned away
            item("RealOnly.txt", false, 7), // survives
        ];
        let merged = root().merge_directory(DATA_NT, &snap, &real, None);
        // Case-insensitive folded order: addeddir, mod.esp, realonly.txt, shared.esp
        assert_eq!(names(&merged), vec!["AddedDir", "Mod.esp", "RealOnly.txt", "Shared.esp"]);
        let shared = merged.iter().find(|e| e.name == "Shared.esp").unwrap();
        assert_eq!(shared.size, 99); // mod wins
        let added = merged.iter().find(|e| e.name == "AddedDir").unwrap();
        assert!(added.is_dir);
        assert!(!merged.iter().any(|e| e.name == "Deleted.esp"));
    }

    #[test]
    fn merge_is_case_insensitive_override() {
        let bytes = merge_snapshot();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let real = vec![item("SHARED.ESP", false, 1)];
        let merged = root().merge_directory(DATA_NT, &snap, &real, None);
        // One entry, display name from the virtual (mod) side.
        let shared: Vec<&DirItem> = merged.iter().filter(|e| e.name.eq_ignore_ascii_case("shared.esp")).collect();
        assert_eq!(shared.len(), 1);
        assert_eq!(shared[0].name, "Shared.esp");
        assert_eq!(shared[0].size, 99);
    }

    #[test]
    fn merge_wildcard_filters_output() {
        let bytes = merge_snapshot();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let real = vec![item("RealOnly.txt", false, 7)];
        let merged = root().merge_directory(DATA_NT, &snap, &real, Some("*.esp"));
        // AddedDir and RealOnly.txt filtered out; only *.esp remain.
        assert_eq!(names(&merged), vec!["Mod.esp", "Shared.esp"]);
    }

    #[test]
    fn merge_out_of_root_returns_filtered_real() {
        let bytes = merge_snapshot();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let real = vec![item("a.dll", false, 1), item("b.exe", false, 2)];
        let merged = root().merge_directory(r"\??\C:\Windows\System32", &snap, &real, Some("*.dll"));
        assert_eq!(names(&merged), vec!["a.dll"]);
    }

    #[test]
    fn merge_real_only_dir_not_in_snapshot() {
        let bytes = merge_snapshot();
        let snap = SnapshotReader::open(&bytes).unwrap();
        // `data/sub` is under root but not in the snapshot -> no overlay.
        let real = vec![item("z.txt", false, 1), item("a.txt", false, 2)];
        let merged = root().merge_directory(r"\??\C:\Games\Skyrim\Data\sub", &snap, &real, None);
        assert_eq!(names(&merged), vec!["a.txt", "z.txt"]); // ordered, no overlay
    }
}

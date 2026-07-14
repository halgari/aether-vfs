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

    /// Whether `nt_path` lies under the managed root (well-formed, not escaping).
    pub fn contains(&self, nt_path: &str) -> bool {
        self.under_root(nt_path).is_some()
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

/// The directory-info `FILE_INFORMATION_CLASS` values the shim marshals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirInfoClass {
    Directory,       // 1  FILE_DIRECTORY_INFORMATION
    FullDirectory,   // 2  FILE_FULL_DIR_INFORMATION
    BothDirectory,   // 3  FILE_BOTH_DIR_INFORMATION
    Names,           // 12 FILE_NAMES_INFORMATION
    IdBothDirectory, // 37 FILE_ID_BOTH_DIR_INFORMATION
    IdFullDirectory, // 38 FILE_ID_FULL_DIR_INFORMATION
}

impl DirInfoClass {
    /// Map a raw `FILE_INFORMATION_CLASS`; `None` for classes we do not marshal.
    pub fn from_u32(v: u32) -> Option<DirInfoClass> {
        Some(match v {
            1 => DirInfoClass::Directory,
            2 => DirInfoClass::FullDirectory,
            3 => DirInfoClass::BothDirectory,
            12 => DirInfoClass::Names,
            37 => DirInfoClass::IdBothDirectory,
            38 => DirInfoClass::IdFullDirectory,
            _ => return None,
        })
    }

    /// Byte offset of the `FileName` field == the fixed header size.
    fn name_offset(self) -> usize {
        match self {
            DirInfoClass::Names => 12,
            DirInfoClass::Directory => 64,
            DirInfoClass::FullDirectory => 68,
            DirInfoClass::IdFullDirectory => 80,
            DirInfoClass::BothDirectory => 94,
            DirInfoClass::IdBothDirectory => 104,
        }
    }

    /// Byte offset of the `FileNameLength` (u32) field.
    fn name_len_offset(self) -> usize {
        match self {
            DirInfoClass::Names => 8,
            _ => 60,
        }
    }

    /// Whether this class carries `EndOfFile`/`AllocationSize`/`FileAttributes`.
    fn has_metadata(self) -> bool {
        !matches!(self, DirInfoClass::Names)
    }
}

/// The NTSTATUS family a directory write resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirStatus {
    Success,
    NoMoreFiles,
    BufferOverflow,
}

/// Result of marshalling directory entries into a caller buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirWriteResult {
    /// Bytes actually used (end offset of the last record's data) —
    /// the value to report as `IoStatusBlock.Information`.
    pub bytes: usize,
    /// Number of entries written.
    pub count: usize,
    pub status: DirStatus,
}

/// Marshal `items` into `buf` in the layout of `class`, chaining
/// `NextEntryOffset`, 8-byte aligning each record, stopping at `single` (one
/// entry) or when the next record would overflow `buf`. Pure: writes only into
/// `buf`.
pub fn write_dir_info(
    class: DirInfoClass,
    items: &[DirItem],
    buf: &mut [u8],
    single: bool,
) -> DirWriteResult {
    let name_off = class.name_offset();
    let name_len_off = class.name_len_offset();
    let cap = buf.len();
    let mut off = 0usize;
    let mut count = 0usize;
    let mut prev: Option<usize> = None;
    let mut last_end = 0usize;

    for it in items {
        let name16: Vec<u16> = it.name.encode_utf16().collect();
        let namelen = name16.len() * 2;
        let rec = name_off + namelen;
        if off + rec > cap {
            break;
        }
        // Zero the fixed header (EaSize/ShortName/FileId fields left zero).
        for b in &mut buf[off..off + name_off] {
            *b = 0;
        }
        if class.has_metadata() {
            let eof = it.size as i64;
            buf[off + 40..off + 48].copy_from_slice(&eof.to_le_bytes());
            buf[off + 48..off + 56].copy_from_slice(&eof.to_le_bytes());
            let attrs: u32 = if it.is_dir { 0x10 } else { 0x80 };
            buf[off + 56..off + 60].copy_from_slice(&attrs.to_le_bytes());
        }
        buf[off + name_len_off..off + name_len_off + 4]
            .copy_from_slice(&(namelen as u32).to_le_bytes());
        let name_bytes: Vec<u8> = name16.iter().flat_map(|u| u.to_le_bytes()).collect();
        buf[off + name_off..off + name_off + namelen].copy_from_slice(&name_bytes);

        if let Some(p) = prev {
            let delta = (off - p) as u32;
            buf[p..p + 4].copy_from_slice(&delta.to_le_bytes());
        }
        prev = Some(off);
        last_end = off + rec;
        count += 1;
        off += (rec + 7) & !7; // 8-byte align next record
        if single {
            break;
        }
    }

    let status = if count == 0 {
        if items.is_empty() {
            DirStatus::NoMoreFiles
        } else {
            DirStatus::BufferOverflow
        }
    } else {
        DirStatus::Success
    };
    DirWriteResult { bytes: last_end, count, status }
}

/// Parse a `FILE_FULL_DIR_INFORMATION` (class 2) chain into items, skipping `.`
/// and `..`. Bounds-checked: a record that would read past `buf` ends the walk
/// (fail-safe, never panics). The shim always *drains the OS in class 2*, so
/// only this one class needs a parser.
pub fn parse_full_dir_info(buf: &[u8]) -> Vec<DirItem> {
    const HDR: usize = 68;
    let mut out = Vec::new();
    let mut o = 0usize;
    loop {
        if o + HDR > buf.len() {
            break;
        }
        let next = u32::from_le_bytes(buf[o..o + 4].try_into().unwrap()) as usize;
        let size = i64::from_le_bytes(buf[o + 40..o + 48].try_into().unwrap());
        let attrs = u32::from_le_bytes(buf[o + 56..o + 60].try_into().unwrap());
        let namelen = u32::from_le_bytes(buf[o + 60..o + 64].try_into().unwrap()) as usize;
        if o + HDR + namelen > buf.len() {
            break;
        }
        let units: Vec<u16> = buf[o + HDR..o + HDR + namelen]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let name = String::from_utf16_lossy(&units);
        if !name.is_empty() && name != "." && name != ".." {
            out.push(DirItem {
                name,
                is_dir: attrs & 0x10 != 0,
                size: size.max(0) as u64,
                mtime: 0,
            });
        }
        if next == 0 {
            break;
        }
        o += next;
    }
    out
}

/// Strip a `\??\` / `\\?\` prefix and a leading `X:` drive, yielding the
/// volume-relative path (`\...`, no drive) that `FILE_NAME_INFORMATION` carries.
/// Idempotent on already-relative input.
pub fn nt_to_volume_relative(nt_path: &str) -> String {
    let s = nt_path
        .strip_prefix(r"\??\")
        .or_else(|| nt_path.strip_prefix(r"\\?\"))
        .unwrap_or(nt_path);
    let b = s.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        s[2..].to_string()
    } else {
        s.to_string()
    }
}

/// Result of marshalling a `FILE_NAME_INFORMATION` record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NameWriteResult {
    pub bytes: usize,
    pub status: DirStatus,
}

/// Marshal a `FILE_NAME_INFORMATION` / `FILE_NORMALIZED_NAME_INFORMATION`:
/// `FileNameLength` (u32 bytes) @0, UTF-16LE `FileName` (no NUL) @4. On overflow
/// writes only `FileNameLength` (documented behavior).
pub fn write_file_name_info(name: &str, buf: &mut [u8]) -> NameWriteResult {
    let name16: Vec<u16> = name.encode_utf16().collect();
    let namelen = name16.len() * 2;
    if buf.len() < 4 {
        return NameWriteResult { bytes: 0, status: DirStatus::BufferOverflow };
    }
    buf[0..4].copy_from_slice(&(namelen as u32).to_le_bytes());
    if buf.len() < 4 + namelen {
        return NameWriteResult { bytes: 4, status: DirStatus::BufferOverflow };
    }
    let nb: Vec<u8> = name16.iter().flat_map(|u| u.to_le_bytes()).collect();
    buf[4..4 + namelen].copy_from_slice(&nb);
    NameWriteResult { bytes: 4 + namelen, status: DirStatus::Success }
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

    #[test]
    fn contains_reports_under_root() {
        let r = root(); // \??\C:\Games\Skyrim
        assert!(r.contains(r"\??\C:\Games\Skyrim\Data\foo.esp"));
        assert!(r.contains(r"\??\C:\Games\Skyrim")); // the root itself
        assert!(!r.contains(r"\??\C:\Windows\System32"));
        assert!(!r.contains(r"\??\C:\Games\Skyrim\..\..\..\..\evil")); // escaping
    }

    fn ditem(name: &str, is_dir: bool, size: u64) -> DirItem {
        DirItem { name: name.into(), is_dir, size, mtime: 0 }
    }

    fn ru32(buf: &[u8], rec: usize, off: usize) -> u32 {
        u32::from_le_bytes(buf[rec + off..rec + off + 4].try_into().unwrap())
    }
    fn ri64(buf: &[u8], rec: usize, off: usize) -> i64 {
        i64::from_le_bytes(buf[rec + off..rec + off + 8].try_into().unwrap())
    }
    fn rname(buf: &[u8], rec: usize, name_off: usize, namelen: usize) -> String {
        let units: Vec<u16> = buf[rec + name_off..rec + name_off + namelen]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    }

    #[test]
    fn write_full_dir_two_entries_chained() {
        let items = [ditem("a.esp", false, 5), ditem("sub", true, 0)];
        let mut buf = vec![0u8; 1024];
        let r = write_dir_info(DirInfoClass::FullDirectory, &items, &mut buf, false);
        assert_eq!(r.status, DirStatus::Success);
        assert_eq!(r.count, 2);
        assert_eq!(ru32(&buf, 0, 60), 10); // "a.esp" = 5 chars * 2 bytes
        assert_eq!(ri64(&buf, 0, 40), 5); // EndOfFile
        assert_eq!(ru32(&buf, 0, 56), 0x80); // FILE_ATTRIBUTE_NORMAL
        assert_eq!(rname(&buf, 0, 68, 10), "a.esp");
        let next = ru32(&buf, 0, 0) as usize;
        assert_eq!(next, 80); // (68+10)=78 -> 8-align -> 80
        assert_eq!(ru32(&buf, next, 56), 0x10); // second is a directory
        assert_eq!(rname(&buf, next, 68, 6), "sub");
        assert_eq!(ru32(&buf, next, 0), 0); // last record: NextEntryOffset 0
        assert_eq!(r.bytes, 80 + 68 + 6);
    }

    #[test]
    fn write_both_dir_uses_class3_header() {
        let items = [ditem("x", false, 1)];
        let mut buf = vec![0u8; 512];
        let r = write_dir_info(DirInfoClass::BothDirectory, &items, &mut buf, false);
        assert_eq!(r.status, DirStatus::Success);
        assert_eq!(ru32(&buf, 0, 60), 2);
        assert_eq!(ru32(&buf, 0, 56), 0x80);
        assert_eq!(rname(&buf, 0, 94, 2), "x");
    }

    #[test]
    fn write_names_class_is_name_only() {
        let items = [ditem("only.txt", false, 999)];
        let mut buf = vec![0u8; 256];
        let r = write_dir_info(DirInfoClass::Names, &items, &mut buf, false);
        assert_eq!(r.status, DirStatus::Success);
        assert_eq!(ru32(&buf, 0, 8), 16); // "only.txt" = 8*2
        assert_eq!(rname(&buf, 0, 12, 16), "only.txt");
    }

    #[test]
    fn write_single_entry_stops_after_one() {
        let items = [ditem("a", false, 1), ditem("b", false, 1)];
        let mut buf = vec![0u8; 512];
        let r = write_dir_info(DirInfoClass::FullDirectory, &items, &mut buf, true);
        assert_eq!(r.count, 1);
        assert_eq!(r.status, DirStatus::Success);
        assert_eq!(ru32(&buf, 0, 0), 0); // single -> no chain
    }

    #[test]
    fn write_empty_is_no_more_files() {
        let mut buf = vec![0u8; 128];
        let r = write_dir_info(DirInfoClass::FullDirectory, &[], &mut buf, false);
        assert_eq!(r.count, 0);
        assert_eq!(r.status, DirStatus::NoMoreFiles);
        assert_eq!(r.bytes, 0);
    }

    #[test]
    fn write_too_small_for_first_is_buffer_overflow() {
        let items = [ditem("longname.esp", false, 1)];
        let mut buf = vec![0u8; 8]; // smaller than one class-2 record
        let r = write_dir_info(DirInfoClass::FullDirectory, &items, &mut buf, false);
        assert_eq!(r.count, 0);
        assert_eq!(r.status, DirStatus::BufferOverflow);
    }

    #[test]
    fn dir_info_class_from_u32() {
        assert_eq!(DirInfoClass::from_u32(2), Some(DirInfoClass::FullDirectory));
        assert_eq!(DirInfoClass::from_u32(3), Some(DirInfoClass::BothDirectory));
        assert_eq!(DirInfoClass::from_u32(12), Some(DirInfoClass::Names));
        assert_eq!(DirInfoClass::from_u32(99), None);
    }

    #[test]
    fn parse_full_dir_round_trips_and_skips_dots() {
        let items = [
            ditem(".", true, 0),
            ditem("..", true, 0),
            ditem("keep.esp", false, 42),
            ditem("kids", true, 0),
        ];
        let mut buf = vec![0u8; 4096];
        let w = write_dir_info(DirInfoClass::FullDirectory, &items, &mut buf, false);
        assert_eq!(w.status, DirStatus::Success);
        let parsed = parse_full_dir_info(&buf);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0], DirItem { name: "keep.esp".into(), is_dir: false, size: 42, mtime: 0 });
        assert_eq!(parsed[1], DirItem { name: "kids".into(), is_dir: true, size: 0, mtime: 0 });
    }

    #[test]
    fn parse_full_dir_empty_buffer_is_empty() {
        let buf = vec![0u8; 68];
        let _ = parse_full_dir_info(&buf); // must not panic
    }

    #[test]
    fn volume_relative_strips_prefix_and_drive() {
        assert_eq!(
            nt_to_volume_relative(r"\??\C:\Games\Skyrim\Data\foo.esp"),
            r"\Games\Skyrim\Data\foo.esp"
        );
        assert_eq!(nt_to_volume_relative(r"\\?\D:\Mods\x.esp"), r"\Mods\x.esp");
        assert_eq!(nt_to_volume_relative(r"\Games\already.esp"), r"\Games\already.esp");
    }

    #[test]
    fn write_file_name_info_round_trips() {
        let mut buf = vec![0u8; 128];
        let r = write_file_name_info(r"\Games\Skyrim\Data\foo.esp", &mut buf);
        assert_eq!(r.status, DirStatus::Success);
        let namelen = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        assert_eq!(namelen, r"\Games\Skyrim\Data\foo.esp".encode_utf16().count() * 2);
        let units: Vec<u16> =
            buf[4..4 + namelen].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(String::from_utf16_lossy(&units), r"\Games\Skyrim\Data\foo.esp");
        assert_eq!(r.bytes, 4 + namelen);
    }

    #[test]
    fn write_file_name_info_overflow_writes_length_only() {
        let mut buf = vec![0u8; 6]; // room for u32 len but not the name
        let r = write_file_name_info("abcdef", &mut buf);
        assert_eq!(r.status, DirStatus::BufferOverflow);
        assert_eq!(u32::from_le_bytes(buf[0..4].try_into().unwrap()), 12);
    }
}

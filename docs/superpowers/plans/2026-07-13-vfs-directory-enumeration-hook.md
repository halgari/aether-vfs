# VFS Directory Enumeration Hook (Slice F) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Hook `ntdll!NtQueryDirectoryFileEx` so a virtualized directory
enumerates as the merged view of its real on-disk entries and the snapshot's
virtual children — mod adds appear, overrides win, tombstones vanish — visible
through `std::fs::read_dir`.

**Architecture:** Pure, exhaustively-unit-tested marshalling
(`write_dir_info`/`parse_full_dir_info`) plus a `RootMap::contains` predicate in
`vfs-redirect`; two engine wrappers in `vfs-shim`; and a thin `unsafe` detour in
`vfs-shim/hook.rs` that tracks directory handles (tagged at `NtCreateFile`,
reclaimed at `NtClose`), drains the real directory through the trampoline in
class 2, merges, and re-marshals into the caller's requested info class.

**Tech Stack:** Rust stable 1.97, `windows-sys` 0.59, `retour::RawDetour`.

## Global Constraints

- Rust stable (1.97); `retour::RawDetour` only (no `static_detour!`).
- `windows-sys` 0.59: `HANDLE`/`HMODULE` are `*mut c_void` (`.is_null()`,
  `core::ptr::null_mut()`).
- `vfs-redirect` stays `#![forbid(unsafe_code)]`. All `unsafe` in `vfs-shim`
  lives only in `hook.rs`.
- Fail-safe: any decode failure, unknown info class, untracked handle, or
  snapshot error → unmodified pass-through. Never make a real directory less
  visible than the un-hooked OS would.
- **Derive rule (recurring bug):** every type used with `assert_eq!`/`.unwrap()`
  in a test must derive `Debug` (and `PartialEq` for `assert_eq!`). All new
  pure types below derive `Debug, Clone, PartialEq, Eq` — they hold only
  `usize`/enums, so this is free. Do NOT use `.unwrap_err()` on a `Result` whose
  `Ok` type is not `Debug`.

---

### Task 1: `write_dir_info` — multi-class directory-info marshaller (pure)

**Files:**
- Modify: `crates/vfs-redirect/src/lib.rs`
- Test: `crates/vfs-redirect/src/lib.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: existing `DirItem { name: String, is_dir: bool, size: u64, mtime: i64 }`.
- Produces: `DirInfoClass` (+ `from_u32`), `DirStatus`, `DirWriteResult`, and
  `write_dir_info(class: DirInfoClass, items: &[DirItem], buf: &mut [u8], single:
  bool) -> DirWriteResult`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `crates/vfs-redirect/src/lib.rs`:

```rust
    fn ditem(name: &str, is_dir: bool, size: u64) -> DirItem {
        DirItem { name: name.into(), is_dir, size, mtime: 0 }
    }

    // Read a u32 field at `off` in a written record starting at `rec`.
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
        // First record at 0: class 2 header = 68, name @68, FileNameLength @60,
        // attrs @56, EndOfFile @40.
        assert_eq!(ru32(&buf, 0, 60), 10); // "a.esp" = 5 chars * 2 bytes
        assert_eq!(ri64(&buf, 0, 40), 5);  // EndOfFile
        assert_eq!(ru32(&buf, 0, 56), 0x80); // FILE_ATTRIBUTE_NORMAL
        assert_eq!(rname(&buf, 0, 68, 10), "a.esp");
        // NextEntryOffset chains to an 8-aligned second record: (68+10)=78 -> 80.
        let next = ru32(&buf, 0, 0) as usize;
        assert_eq!(next, 80);
        assert_eq!(ru32(&buf, next, 56), 0x10); // second is a directory
        assert_eq!(rname(&buf, next, 68, 6), "sub");
        assert_eq!(ru32(&buf, next, 0), 0); // last record: NextEntryOffset 0
        // bytes = end of last record's data = 80 + 68 + 6.
        assert_eq!(r.bytes, 80 + 68 + 6);
    }

    #[test]
    fn write_both_dir_uses_class3_header() {
        let items = [ditem("x", false, 1)];
        let mut buf = vec![0u8; 512];
        let r = write_dir_info(DirInfoClass::BothDirectory, &items, &mut buf, false);
        assert_eq!(r.status, DirStatus::Success);
        // Class 3 name offset is 94; FileNameLength still @60, attrs @56.
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
        // Class 12: FileNameLength @8, name @12, no size/attrs fields.
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
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p vfs-redirect write_ 2>&1 | tail -20`
Expected: FAIL — `DirInfoClass`, `write_dir_info`, etc. not found.

- [ ] **Step 3: Implement**

Add to `crates/vfs-redirect/src/lib.rs` (near the other public types, before the
`#[cfg(test)]` module):

```rust
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
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p vfs-redirect write_ dir_info_class_from_u32 2>&1 | tail -20`
Expected: PASS (all 7 new tests).

- [ ] **Step 5: Commit**

```bash
git add crates/vfs-redirect/src/lib.rs
git commit -m "feat(vfs-redirect): multi-class directory-info marshaller (write_dir_info)"
```

---

### Task 2: `parse_full_dir_info` — class-2 directory parser (pure)

**Files:**
- Modify: `crates/vfs-redirect/src/lib.rs`
- Test: `crates/vfs-redirect/src/lib.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: `write_dir_info`, `DirInfoClass::FullDirectory`, `DirItem`.
- Produces: `parse_full_dir_info(buf: &[u8]) -> Vec<DirItem>`.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module:

```rust
    #[test]
    fn parse_full_dir_round_trips_and_skips_dots() {
        // Marshal ".", "..", a file, and a dir in class 2, then parse them back.
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
        // "." and ".." dropped; file and dir survive with attrs/size.
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0], DirItem { name: "keep.esp".into(), is_dir: false, size: 42, mtime: 0 });
        assert_eq!(parsed[1], DirItem { name: "kids".into(), is_dir: true, size: 0, mtime: 0 });
    }

    #[test]
    fn parse_full_dir_empty_buffer_is_empty() {
        // A zeroed buffer: NextEntryOffset 0, FileNameLength 0 -> one empty name,
        // which is neither "." nor ".." but has an empty name; ensure no panic
        // and the walk terminates.
        let buf = vec![0u8; 68];
        let _ = parse_full_dir_info(&buf); // must not panic
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p vfs-redirect parse_full_dir 2>&1 | tail -20`
Expected: FAIL — `parse_full_dir_info` not found.

- [ ] **Step 3: Implement**

Add to `crates/vfs-redirect/src/lib.rs`:

```rust
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
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p vfs-redirect parse_full_dir 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/vfs-redirect/src/lib.rs
git commit -m "feat(vfs-redirect): class-2 directory parser (parse_full_dir_info)"
```

---

### Task 3: `RootMap::contains` predicate (pure)

**Files:**
- Modify: `crates/vfs-redirect/src/lib.rs`
- Test: `crates/vfs-redirect/src/lib.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: existing private `RootMap::under_root`.
- Produces: `pub fn RootMap::contains(&self, nt_path: &str) -> bool`.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module:

```rust
    #[test]
    fn contains_reports_under_root() {
        let r = root(); // \??\C:\Games\Skyrim
        assert!(r.contains(r"\??\C:\Games\Skyrim\Data\foo.esp"));
        assert!(r.contains(r"\??\C:\Games\Skyrim")); // the root itself
        assert!(!r.contains(r"\??\C:\Windows\System32"));
        assert!(!r.contains(r"\??\C:\Games\Skyrim\..\..\..\..\evil")); // escaping
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p vfs-redirect contains_reports_under_root 2>&1 | tail -20`
Expected: FAIL — no method `contains`.

- [ ] **Step 3: Implement**

Add inside `impl RootMap` (next to `under_root`):

```rust
    /// Whether `nt_path` lies under the managed root (well-formed, not escaping).
    pub fn contains(&self, nt_path: &str) -> bool {
        self.under_root(nt_path).is_some()
    }
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p vfs-redirect contains_reports_under_root 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/vfs-redirect/src/lib.rs
git commit -m "feat(vfs-redirect): RootMap::contains under-root predicate"
```

---

### Task 4: Engine directory wrappers

**Files:**
- Modify: `crates/vfs-shim/src/engine.rs`
- Test: `crates/vfs-shim/src/engine.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: `RootMap::{contains, merge_directory}`, `vfs_redirect::DirItem`.
- Produces: `Engine::is_under_root(&self, nt_path: &str) -> bool`;
  `Engine::merge_directory(&self, dir_nt_path: &str, real: &[DirItem], wildcard:
  Option<&str>) -> Vec<DirItem>`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `crates/vfs-shim/src/engine.rs`:

```rust
    #[test]
    fn is_under_root_predicate() {
        let engine = Engine::new(r"\??\C:\Games\Skyrim", snapshot_bytes()).unwrap();
        assert!(engine.is_under_root(r"\??\C:\Games\Skyrim\Data\foo.esp"));
        assert!(!engine.is_under_root(r"\??\C:\Windows\notepad.exe"));
    }

    #[test]
    fn merge_directory_adds_virtual_children() {
        use vfs_redirect::DirItem;
        let engine = Engine::new(r"\??\C:\Games\Skyrim", snapshot_bytes()).unwrap();
        let real = vec![DirItem { name: "real.txt".into(), is_dir: false, size: 1, mtime: 0 }];
        // snapshot_bytes() puts foo.esp under data/.
        let merged = engine.merge_directory(r"\??\C:\Games\Skyrim\Data", &real, None);
        let names: Vec<&str> = merged.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"real.txt"));
        assert!(names.contains(&"foo.esp"));
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p vfs-shim --lib is_under_root_predicate merge_directory_adds 2>&1 | tail -20`
Expected: FAIL — methods not found.

- [ ] **Step 3: Implement**

In `crates/vfs-shim/src/engine.rs`, change the import line:

```rust
use vfs_redirect::{AttrDecision, Decision, DirItem, RootMap};
```

Add to `impl Engine`:

```rust
    /// Whether `nt_path` lies under the managed root.
    pub fn is_under_root(&self, nt_path: &str) -> bool {
        self.map.contains(nt_path)
    }

    /// Merge a directory's real on-disk entries with the snapshot's virtual
    /// children. Fail-safe: on snapshot re-open failure, returns `real`
    /// unchanged (never hides real files on error).
    pub fn merge_directory(
        &self,
        dir_nt_path: &str,
        real: &[DirItem],
        wildcard: Option<&str>,
    ) -> Vec<DirItem> {
        match SnapshotReader::open(&self.snapshot) {
            Ok(reader) => self.map.merge_directory(dir_nt_path, &reader, real, wildcard),
            Err(_) => real.to_vec(),
        }
    }
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p vfs-shim --lib is_under_root_predicate merge_directory_adds 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/vfs-shim/src/engine.rs
git commit -m "feat(vfs-shim): Engine directory wrappers (is_under_root, merge_directory)"
```

---

### Task 5: ntdef additions for dir enumeration

**Files:**
- Modify: `crates/vfs-shim/src/ntdef.rs`

**Interfaces:**
- Consumes: existing `UnicodeString`, `HANDLE`, `NTSTATUS`, `c_void`.
- Produces: `NtQueryDirectoryFileExFn`, `NtCloseFn` fn types; consts
  `SL_RESTART_SCAN`, `SL_RETURN_SINGLE_ENTRY`, `STATUS_NO_MORE_FILES`,
  `STATUS_BUFFER_OVERFLOW`.

This task adds only type/const definitions (no behavior), so it has no
standalone test; it is verified by compiling and by Task 6's integration test.
Fold it into Task 6 if your workflow requires every task to carry a test.

- [ ] **Step 1: Add the definitions**

Append to `crates/vfs-shim/src/ntdef.rs`:

```rust
/// `ntdll!NtQueryDirectoryFileEx`. `FileName` is a `PUNICODE_STRING` (nullable);
/// `IoStatusBlock` and `FileInformation` are left opaque and touched by the hook
/// via raw offsets. `ApcRoutine`/`ApcContext`/`Event` are unused by our callers.
pub type NtQueryDirectoryFileExFn = unsafe extern "system" fn(
    HANDLE,               // FileHandle
    HANDLE,               // Event
    *const c_void,        // ApcRoutine
    *const c_void,        // ApcContext
    *mut c_void,          // IoStatusBlock
    *mut c_void,          // FileInformation
    u32,                  // Length
    u32,                  // FileInformationClass
    u32,                  // QueryFlags
    *const UnicodeString, // FileName
) -> NTSTATUS;

/// `ntdll!NtClose`.
pub type NtCloseFn = unsafe extern "system" fn(HANDLE) -> NTSTATUS;

/// `NtQueryDirectoryFileEx` QueryFlags.
pub const SL_RESTART_SCAN: u32 = 0x01;
pub const SL_RETURN_SINGLE_ENTRY: u32 = 0x02;

/// `STATUS_NO_MORE_FILES` — enumeration cursor exhausted.
pub const STATUS_NO_MORE_FILES: NTSTATUS = 0x8000_0006u32 as i32;
/// `STATUS_BUFFER_OVERFLOW` — the caller buffer cannot hold even one entry.
pub const STATUS_BUFFER_OVERFLOW: NTSTATUS = 0x8000_0005u32 as i32;
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build -p vfs-shim 2>&1 | tail -20`
Expected: builds (warnings about unused items are fine until Task 6 wires them).

- [ ] **Step 3: Commit**

```bash
git add crates/vfs-shim/src/ntdef.rs
git commit -m "feat(vfs-shim): ntdef types/consts for NtQueryDirectoryFileEx + NtClose"
```

---

### Task 6: `NtQueryDirectoryFileEx` hook + handle tracking (the unsafe integration)

**Files:**
- Modify: `crates/vfs-shim/src/hook.rs`
- Test: `crates/vfs-shim/tests/hook_direnum.rs` (create)

**Interfaces:**
- Consumes: `Engine::{is_under_root, merge_directory}`;
  `vfs_redirect::{parse_full_dir_info, write_dir_info, DirInfoClass, DirItem,
  DirStatus}`; ntdef `NtQueryDirectoryFileExFn`, `NtCloseFn`, `UnicodeString`,
  `SL_RESTART_SCAN`, `SL_RETURN_SINGLE_ENTRY`, `STATUS_NO_MORE_FILES`,
  `STATUS_BUFFER_OVERFLOW`, `STATUS_SUCCESS`, `STATUS_UNSUCCESSFUL`.
- Produces: two additional detours in `install`; a private `DIR_TABLE`; the
  `close_hook`, `qdirex_hook`, and helpers `drain_real`, `wildcard_of`.

- [ ] **Step 1: Write the failing integration test**

Create `crates/vfs-shim/tests/hook_direnum.rs`:

```rust
//! Single-test binary: `std::fs::read_dir` sees the merged VFS view.
use vfs_shim::{install, Engine};

#[test]
fn read_dir_reflects_the_merged_vfs() {
    let pid = std::process::id();
    let root = std::env::temp_dir().join(format!("vfs-shim-direnum-{pid}"));
    // Backing files live OUTSIDE the root so they do not appear in the listing.
    let backing_dir =
        std::env::temp_dir().join(format!("vfs-shim-direnum-backing-{pid}"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&backing_dir).unwrap();

    // Real on-disk contents of the enumerated directory.
    std::fs::write(root.join("real_a.txt"), b"a").unwrap();
    std::fs::write(root.join("real_b.txt"), b"b").unwrap();
    std::fs::write(root.join("over.esp"), vec![0u8; 3]).unwrap(); // overridden
    std::fs::write(root.join("gone.esp"), b"x").unwrap();          // tombstoned
    std::fs::create_dir_all(root.join("realdir")).unwrap();

    // Backing files for the mod override / add.
    let over_backing = backing_dir.join("over.esp");
    std::fs::write(&over_backing, vec![0u8; 4096]).unwrap();
    let add_backing = backing_dir.join("added.esp");
    std::fs::write(&add_backing, vec![0u8; 10]).unwrap();

    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let e = |vpath: &str, kind: EntryKind, source: &str, size: u64| InputEntry {
            vpath: vpath.into(),
            kind,
            source: source.into(),
            size,
            mtime: 0,
        };
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![
                e("added.esp", EntryKind::File, add_backing.to_str().unwrap(), 10),
                e("over.esp", EntryKind::File, over_backing.to_str().unwrap(), 4096),
                e("gone.esp", EntryKind::Tombstone, "", 0),
                e("vdir", EntryKind::Dir, "", 0),
            ],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };
    let engine = Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let _guard = install(engine).expect("install");

    let mut names: Vec<String> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();

    assert!(names.contains(&"added.esp".to_string()), "mod-added missing: {names:?}");
    assert!(names.contains(&"real_a.txt".to_string()), "{names:?}");
    assert!(names.contains(&"real_b.txt".to_string()), "{names:?}");
    assert!(names.contains(&"over.esp".to_string()), "{names:?}");
    assert!(names.contains(&"realdir".to_string()), "{names:?}");
    assert!(names.contains(&"vdir".to_string()), "virtual dir missing: {names:?}");
    assert!(!names.contains(&"gone.esp".to_string()), "tombstone shown: {names:?}");

    // Override wins: over.esp reports the mod size (4096), not the real 3.
    let over = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap())
        .find(|e| e.file_name().to_string_lossy() == "over.esp")
        .unwrap();
    assert_eq!(over.metadata().unwrap().len(), 4096, "override size should win");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p vfs-shim --test hook_direnum 2>&1 | tail -30`
Expected: FAIL — `gone.esp` still listed and/or `added.esp`/`vdir` absent
(no dir-enum hook yet).

- [ ] **Step 3: Extend the imports in `crates/vfs-shim/src/hook.rs`**

Replace the current `use` block additions. Add these near the top (keep existing
imports):

```rust
use std::collections::BTreeMap;
use std::sync::Mutex;

use vfs_redirect::{
    parse_full_dir_info, write_dir_info, AttrDecision, Decision, DirInfoClass, DirItem, DirStatus,
};
```

(Remove the old `use vfs_redirect::{AttrDecision, Decision};` line — it is
superseded by the line above.)

Extend the ntdef import to add the new symbols:

```rust
use crate::ntdef::{
    FileBasicInformation, FileNetworkOpenInformation, NtCloseFn, NtCreateFileFn,
    NtQueryAttributesFileFn, NtQueryDirectoryFileExFn, NtQueryFullAttributesFileFn,
    ObjectAttributes, UnicodeString, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    SL_RESTART_SCAN, SL_RETURN_SINGLE_ENTRY, STATUS_BUFFER_OVERFLOW, STATUS_NO_MORE_FILES,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_SUCCESS, STATUS_UNSUCCESSFUL,
};
```

- [ ] **Step 4: Add the handle table + trampoline statics**

After the existing `static mut TRAMP_QFULL` line, add:

```rust
static mut TRAMP_QDIREX: Option<NtQueryDirectoryFileExFn> = None;
static mut TRAMP_CLOSE: Option<NtCloseFn> = None;

/// Per-handle enumeration cursor over a merged directory listing.
struct EnumState {
    merged: Vec<DirItem>,
    cursor: usize,
}

/// A tracked directory handle: the NT path it was opened as, and its lazily
/// built enumeration state (rebuilt on `SL_RESTART_SCAN`).
struct DirTracked {
    dir_nt_path: String,
    state: Option<EnumState>,
}

/// Handle value (`isize`) -> tracking. `BTreeMap::new()` is `const`, so this
/// needs no lazy init. Populated by the `NtCreateFile` hook, drained by
/// `NtClose`.
static DIR_TABLE: Mutex<BTreeMap<isize, DirTracked>> = Mutex::new(BTreeMap::new());
```

- [ ] **Step 5: Wire the two new detours into `install`**

In `install`, after the `d_qfull` trampoline is stored and before the
`d_create.enable()` calls, add:

```rust
        let d_qdirex =
            make_detour(ntdll, b"NtQueryDirectoryFileEx\0", qdirex_hook as *const ())?;
        TRAMP_QDIREX = Some(core::mem::transmute::<*const (), NtQueryDirectoryFileExFn>(
            d_qdirex.trampoline() as *const (),
        ));
        let d_close = make_detour(ntdll, b"NtClose\0", close_hook as *const ())?;
        TRAMP_CLOSE = Some(core::mem::transmute::<*const (), NtCloseFn>(
            d_close.trampoline() as *const (),
        ));
```

Add their `enable()` calls alongside the existing three:

```rust
        d_qdirex.enable().map_err(|_| InstallError::Detour)?;
        d_close.enable().map_err(|_| InstallError::Detour)?;
```

And extend the returned guard's vec:

```rust
        Ok(HookGuard { _detours: vec![d_create, d_qattr, d_qfull, d_qdirex, d_close] })
```

- [ ] **Step 6: Tag directory handles in the `create_hook` PassThrough branch**

Replace the `Some(Decision::PassThrough) | None =>` arm of `create_hook` with:

```rust
        Some(Decision::PassThrough) | None => {
            let status =
                tramp(file_handle, access, oa, iosb, alloc, attrs, share, disp, opts, ea, ealen);
            // NT_SUCCESS is status >= 0. Tag under-root opens so the dir-enum
            // hook can recognize the handle; harmless for file handles (they
            // never receive an NtQueryDirectoryFileEx call) and reclaimed by
            // NtClose regardless.
            if status >= 0 && !file_handle.is_null() {
                if let Some(engine) = ENGINE.get() {
                    if let Some(path) = path_of(oa) {
                        if engine.is_under_root(&path) {
                            if let Ok(mut table) = DIR_TABLE.lock() {
                                table.insert(
                                    *file_handle as isize,
                                    DirTracked { dir_nt_path: path, state: None },
                                );
                            }
                        }
                    }
                }
            }
            status
        }
```

- [ ] **Step 7: Add `close_hook`, `qdirex_hook`, and helpers**

Append to `crates/vfs-shim/src/hook.rs`:

```rust
/// Reclaim any tracking for a closing handle before the OS (possibly) reuses
/// its value.
unsafe extern "system" fn close_hook(handle: HANDLE) -> NTSTATUS {
    let tramp = match TRAMP_CLOSE {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if let Ok(mut table) = DIR_TABLE.lock() {
        table.remove(&(handle as isize));
    }
    tramp(handle)
}

/// Extract a search wildcard from a `PUNICODE_STRING`. Null/empty/`*`/`*.*`
/// mean "match everything" (`None`).
unsafe fn wildcard_of(file_name: *const UnicodeString) -> Option<String> {
    if file_name.is_null() {
        return None;
    }
    let us = &*file_name;
    if us.buffer.is_null() || us.length == 0 {
        return None;
    }
    let units = core::slice::from_raw_parts(us.buffer, us.length as usize / 2);
    let s = String::from_utf16_lossy(units);
    if s.is_empty() || s == "*" || s == "*.*" {
        None
    } else {
        Some(s)
    }
}

/// Drain a real directory's entries by calling the trampoline in class 2
/// (FileFullDirectoryInformation) with SL_RESTART_SCAN, until a negative status
/// (STATUS_NO_MORE_FILES or any error). The trampoline bypasses this detour, so
/// draining does not recurse.
unsafe fn drain_real(handle: HANDLE, tramp: NtQueryDirectoryFileExFn) -> Vec<DirItem> {
    const CLASS_FULL_DIR: u32 = 2;
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut first = true;
    loop {
        let mut local_iosb = [0u8; 16];
        let flags = if first { SL_RESTART_SCAN } else { 0 };
        first = false;
        let st = tramp(
            handle,
            core::ptr::null_mut(),
            core::ptr::null(),
            core::ptr::null(),
            local_iosb.as_mut_ptr() as *mut c_void,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            CLASS_FULL_DIR,
            flags,
            core::ptr::null(),
        );
        if st < 0 {
            break; // STATUS_NO_MORE_FILES or an error ends the drain
        }
        out.extend(parse_full_dir_info(&buf));
    }
    out
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn qdirex_hook(
    handle: HANDLE,
    event: HANDLE,
    apc: *const c_void,
    apc_ctx: *const c_void,
    iosb: *mut c_void,
    info: *mut c_void,
    length: u32,
    class_raw: u32,
    flags: u32,
    file_name: *const UnicodeString,
) -> NTSTATUS {
    let tramp = match TRAMP_QDIREX {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    let passthrough =
        || tramp(handle, event, apc, apc_ctx, iosb, info, length, class_raw, flags, file_name);

    // Unknown info class -> let the OS handle it verbatim.
    let class = match DirInfoClass::from_u32(class_raw) {
        Some(c) => c,
        None => return passthrough(),
    };
    let key = handle as isize;
    let restart = flags & SL_RESTART_SCAN != 0;
    let single = flags & SL_RETURN_SINGLE_ENTRY != 0;

    // Phase 1 (locked): is this a tracked handle, and must we (re)build?
    let (need_build, dir_path) = {
        let table = match DIR_TABLE.lock() {
            Ok(t) => t,
            Err(_) => return passthrough(),
        };
        match table.get(&key) {
            None => return passthrough(),
            Some(t) => (restart || t.state.is_none(), t.dir_nt_path.clone()),
        }
    };

    // Phase 2 (unlocked): drain the real dir + merge. `drain_real` calls the
    // syscall, so the lock must NOT be held here (NtClose also takes it).
    let rebuilt = if need_build {
        let wildcard = wildcard_of(file_name);
        let real = drain_real(handle, tramp);
        Some(match ENGINE.get() {
            Some(engine) => engine.merge_directory(&dir_path, &real, wildcard.as_deref()),
            None => real,
        })
    } else {
        None
    };

    // Phase 3 (locked): store the merged view (if rebuilt) and serve a slice.
    let mut table = match DIR_TABLE.lock() {
        Ok(t) => t,
        Err(_) => return passthrough(),
    };
    let tracked = match table.get_mut(&key) {
        Some(t) => t,
        None => return passthrough(),
    };
    if let Some(merged) = rebuilt {
        tracked.state = Some(EnumState { merged, cursor: 0 });
    }
    let st = match tracked.state.as_mut() {
        Some(s) => s,
        None => return passthrough(),
    };
    let buf = core::slice::from_raw_parts_mut(info as *mut u8, length as usize);
    let result = write_dir_info(class, &st.merged[st.cursor..], buf, single);
    st.cursor += result.count;
    drop(table);

    let status = match result.status {
        DirStatus::Success => STATUS_SUCCESS,
        DirStatus::NoMoreFiles => STATUS_NO_MORE_FILES,
        DirStatus::BufferOverflow => STATUS_BUFFER_OVERFLOW,
    };
    // IO_STATUS_BLOCK: Status (NTSTATUS) @0, Information (ULONG_PTR) @8.
    if !iosb.is_null() {
        let p = iosb as *mut u8;
        core::ptr::write_unaligned(p as *mut u32, status as u32);
        core::ptr::write_unaligned(p.add(8) as *mut usize, result.bytes);
    }
    status
}
```

- [ ] **Step 8: Run the integration test**

Run: `cargo test -p vfs-shim --test hook_direnum 2>&1 | tail -30`
Expected: PASS — merged listing with `added.esp`, `vdir`, real files present;
`gone.esp` hidden; `over.esp` size 4096.

- [ ] **Step 9: Run the full shim + redirect suites**

Run: `cargo test -p vfs-redirect -p vfs-shim 2>&1 | tail -30`
Expected: PASS (existing `hook_redirect`, `hook_deny`, `hook_attrs` still green —
the new `NtClose` detour and handle tagging must not regress them).

- [ ] **Step 10: Commit**

```bash
git add crates/vfs-shim/src/hook.rs crates/vfs-shim/tests/hook_direnum.rs
git commit -m "feat(vfs-shim): NtQueryDirectoryFileEx hook with merged directory enumeration"
```

---

### Task 7: Workspace verification + memory update

**Files:**
- Modify: memory `vfs-hook-surface-plan.md` (mark Slice F done, record dir-enum recipe)

- [ ] **Step 1: Full workspace build + test**

Run: `cargo test --workspace 2>&1 | tail -30`
Expected: all tests pass.

- [ ] **Step 2: Warnings + unsafe audit**

Run: `cargo build --workspace 2>&1 | rg -i "warning" | head` — expect none.
Confirm `unsafe` still appears only in `crates/vfs-shim/src/hook.rs` and the
`vfs-win`/DLL layers; `vfs-redirect` remains `#![forbid(unsafe_code)]`.

- [ ] **Step 3: Commit Cargo.lock if changed**

```bash
git add Cargo.lock
git commit -m "chore: update Cargo.lock for Slice F" || true
```

- [ ] **Step 4: Update the hook-surface memory**

Mark Slice F **DONE** in `vfs-hook-surface-plan.md`, note that
`NtQueryDirectoryFileEx` drains the OS in class 2 and re-marshals to the caller's
class, that virtual-dir *enumeration* (opening a purely-virtual dir) and the
non-Ex `NtQueryDirectoryFile` remain follow-ups, and that G/H (NtOpenFile,
NtQueryInformationFile identity) are next.

---

## Self-Review

**Spec coverage:** write_dir_info (Task 1) + parse_full_dir_info (Task 2) cover
marshalling both directions; contains/engine wrappers (Tasks 3–4) feed the hook;
ntdef (Task 5) + hook (Task 6) implement the detour, handle tracking, drain,
merge, cursor, flags, wildcard, and IoStatusBlock; the integration test covers
add/override/tombstone/virtual-dir/real passthrough end-to-end. Out-of-scope
items (virtual-dir enumeration, non-Ex function) are documented, not silently
dropped.

**Placeholder scan:** none — every code step carries complete code.

**Type consistency:** `DirInfoClass`, `DirStatus`, `DirWriteResult`,
`DirWriteResult { bytes, count, status }`, `DirItem { name, is_dir, size, mtime
}`, and the fn signatures `write_dir_info(class, items, buf, single)` /
`parse_full_dir_info(buf)` / `Engine::merge_directory(dir_nt_path, real,
wildcard)` are used identically across tasks. The `NtQueryDirectoryFileExFn`
arity (10 params) matches the hook and the `drain_real` call sites.

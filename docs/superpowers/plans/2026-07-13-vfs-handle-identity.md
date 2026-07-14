# VFS Handle Identity (Slice G) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development or superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Hook `NtQueryInformationFile` so a redirected virtual file reports its
*virtual* path to `GetFinalPathNameByHandleW` (class 48,
`FileNormalizedNameInformation`), while reads still hit the backing file.

**Architecture:** Pure `nt_to_volume_relative` + `write_file_name_info` in
`vfs-redirect`; an `IDENTITY_TABLE` (handle → virtual volume-relative path)
populated in the redirect branches of the existing open hooks; a new
`NtQueryInformationFile` detour that spoofs class 48 only.

## Global Constraints

- Rust stable 1.97; `retour::RawDetour`; `windows-sys` 0.59.
- `vfs-redirect` stays `#![forbid(unsafe_code)]`; all `unsafe` in `hook.rs`.
- Fail-safe pass-through on any decode failure / untracked handle / non-48 class.
- Reuse `DirStatus` for the name-write result (do not introduce a parallel enum).

---

### Task 1: `nt_to_volume_relative` + `write_file_name_info` (pure)

**Files:** Modify `crates/vfs-redirect/src/lib.rs` (+ tests there).

**Produces:** `nt_to_volume_relative(&str) -> String`; `NameWriteResult { bytes,
status }`; `write_file_name_info(name: &str, buf: &mut [u8]) -> NameWriteResult`.

- [ ] **Step 1: Failing tests**

```rust
    #[test]
    fn volume_relative_strips_prefix_and_drive() {
        assert_eq!(nt_to_volume_relative(r"\??\C:\Games\Skyrim\Data\foo.esp"), r"\Games\Skyrim\Data\foo.esp");
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
        let units: Vec<u16> = buf[4..4 + namelen].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
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
```

- [ ] **Step 2: Run — expect FAIL** (`cargo test -p vfs-redirect volume_relative write_file_name_info`).

- [ ] **Step 3: Implement** (add near the dir-info functions, before `#[cfg(test)]`):

```rust
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
```

- [ ] **Step 4: Run — expect PASS.**
- [ ] **Step 5: Commit** (`feat(vfs-redirect): volume-relative path + FILE_NAME_INFORMATION marshaller`).

---

### Task 2: ntdef `NtQueryInformationFile` type + class const

**Files:** Modify `crates/vfs-shim/src/ntdef.rs`.

- [ ] **Step 1: Add**

```rust
/// `ntdll!NtQueryInformationFile`.
pub type NtQueryInformationFileFn = unsafe extern "system" fn(
    HANDLE,      // FileHandle
    *mut c_void, // IoStatusBlock
    *mut c_void, // FileInformation
    u32,         // Length
    u32,         // FileInformationClass
) -> NTSTATUS;

/// `FileNormalizedNameInformation` — the class `GetFinalPathNameByHandleW` uses
/// as the authoritative path (spoof this; NOT class 9, which would break it).
pub const FILE_NORMALIZED_NAME_INFORMATION: u32 = 48;
```

- [ ] **Step 2: `cargo build -p vfs-shim`** (warnings until Task 3 are fine).
- [ ] **Step 3: Commit** (`feat(vfs-shim): ntdef NtQueryInformationFile type + class 48 const`).

---

### Task 3: `NtQueryInformationFile` identity hook

**Files:** Modify `crates/vfs-shim/src/hook.rs`; create
`crates/vfs-shim/tests/hook_identity.rs`.

- [ ] **Step 1: Failing integration test** — `crates/vfs-shim/tests/hook_identity.rs`:

```rust
//! Single-test binary: a redirected virtual file reports its VIRTUAL path.
use std::os::windows::io::AsRawHandle;
use vfs_shim::{install, Engine};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;

#[test]
fn redirected_file_reports_virtual_path() {
    let pid = std::process::id();
    let root = std::env::temp_dir().join(format!("vfs-shim-ident-{pid}"));
    let backing_dir = std::env::temp_dir().join(format!("vfs-shim-ident-backing-{pid}"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&backing_dir).unwrap();

    // Backing file with a DISTINCT name so we can tell paths apart.
    let backing = backing_dir.join("backing_blob.dat");
    std::fs::write(&backing, b"the-real-bytes").unwrap();

    // Virtual file, absent on disk, redirects to the backing blob.
    let vfile = root.join("mod.esp");

    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "mod.esp".into(),
                kind: EntryKind::File,
                source: backing.to_str().unwrap().into(),
                size: 14,
                mtime: 0,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };
    let engine = Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let _guard = install(engine).expect("install");

    // Open the virtual path -> redirected; content comes from the backing file.
    let f = std::fs::File::open(&vfile).expect("open redirected virtual file");
    let content = std::fs::read(&vfile).unwrap();
    assert_eq!(content, b"the-real-bytes");

    let h = f.as_raw_handle() as HANDLE;
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetFinalPathNameByHandleW(h, buf.as_mut_ptr(), buf.len() as u32, 0) };
    assert!(n > 0, "GetFinalPathNameByHandleW failed");
    let final_path = String::from_utf16_lossy(&buf[..n as usize]).to_lowercase();

    assert!(final_path.contains("mod.esp"), "should report virtual name: {final_path}");
    assert!(!final_path.contains("backing_blob"), "must NOT leak backing name: {final_path}");
}
```

- [ ] **Step 2: Run — expect FAIL** (`cargo test -p vfs-shim --test hook_identity`): final path shows `backing_blob.dat`.

- [ ] **Step 3: Extend imports** in `hook.rs`:
  - `use vfs_redirect::{..., nt_to_volume_relative, write_file_name_info};`
  - ntdef import: add `NtQueryInformationFileFn`, `FILE_NORMALIZED_NAME_INFORMATION`.

- [ ] **Step 4: Add table + trampoline static** (next to `DIR_TABLE`):

```rust
static mut TRAMP_QIF: Option<NtQueryInformationFileFn> = None;

/// Redirected-file handle -> virtual volume-relative path (identity spoof).
static IDENTITY_TABLE: Mutex<BTreeMap<isize, String>> = Mutex::new(BTreeMap::new());
```

- [ ] **Step 5: Capture at redirect.** In `create_hook`'s `Decision::Redirect`
  arm, compute the original path first and record the handle after success:

```rust
        Some(Decision::Redirect { target_nt }) => {
            let vpath = path_of(oa); // original virtual path, pre-rewrite
            let mut wbuf: Vec<u16> = target_nt.encode_utf16().collect();
            // ... existing new_us / new_oa construction ...
            let status = tramp(
                file_handle, access, &new_oa, iosb, alloc, attrs, share, disp, opts, ea, ealen,
            );
            drop(wbuf);
            if status >= 0 && !file_handle.is_null() {
                if let Some(p) = vpath {
                    if let Ok(mut t) = IDENTITY_TABLE.lock() {
                        t.insert(*file_handle as isize, nt_to_volume_relative(&p));
                    }
                }
            }
            status
        }
```

  Apply the identical capture in `open_hook`'s `Decision::Redirect` arm (its
  trampoline call uses the `NtOpenFile` argument list).

- [ ] **Step 6: Wire the detour** in `install` (alongside the others):

```rust
        let d_qif =
            make_detour(ntdll, b"NtQueryInformationFile\0", qif_hook as *const ())?;
        TRAMP_QIF = Some(core::mem::transmute::<*const (), NtQueryInformationFileFn>(
            d_qif.trampoline() as *const (),
        ));
```
  Add `d_qif.enable()...` and push `d_qif` into the `HookGuard` vec.

- [ ] **Step 7: Extend `close_hook`** to also drop `IDENTITY_TABLE`:

```rust
    if let Ok(mut t) = IDENTITY_TABLE.lock() {
        t.remove(&(handle as isize));
    }
```

- [ ] **Step 8: Add `qif_hook`:**

```rust
unsafe extern "system" fn qif_hook(
    handle: HANDLE,
    iosb: *mut c_void,
    info: *mut c_void,
    length: u32,
    class: u32,
) -> NTSTATUS {
    let tramp = match TRAMP_QIF {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    // Only FileNormalizedNameInformation (48) on a tracked handle is spoofed;
    // class 9 must pass through or GetFinalPathNameByHandleW breaks.
    if class == FILE_NORMALIZED_NAME_INFORMATION && !info.is_null() {
        let vpath = {
            match IDENTITY_TABLE.lock() {
                Ok(t) => t.get(&(handle as isize)).cloned(),
                Err(_) => None,
            }
        };
        if let Some(vpath) = vpath {
            let buf = core::slice::from_raw_parts_mut(info as *mut u8, length as usize);
            let r = write_file_name_info(&vpath, buf);
            let status = match r.status {
                DirStatus::Success => STATUS_SUCCESS,
                _ => STATUS_BUFFER_OVERFLOW,
            };
            if !iosb.is_null() {
                let p = iosb as *mut u8;
                core::ptr::write_unaligned(p as *mut u32, status as u32);
                core::ptr::write_unaligned(p.add(8) as *mut usize, r.bytes);
            }
            return status;
        }
    }
    tramp(handle, iosb, info, length, class)
}
```

- [ ] **Step 9: Run** `cargo test -p vfs-shim --test hook_identity` — expect PASS.
- [ ] **Step 10: Regression** `cargo test -p vfs-redirect -p vfs-shim` — all green.
- [ ] **Step 11: Commit** (`feat(vfs-shim): NtQueryInformationFile identity spoof (class 48)`).

---

### Task 4: Workspace verify + memory

- [ ] `cargo test --workspace` green; `cargo build --workspace` no warnings; `unsafe`
  still only in `hook.rs`.
- [ ] Commit `Cargo.lock` if changed.
- [ ] Update `vfs-hook-surface-plan.md`: Slice G DONE; note NtQueryInformationFile
  spoofs class 48 only; next is the real-executable acceptance harness.

## Self-Review

Spec coverage: Task 1 pure helpers, Task 2 ABI, Task 3 hook + capture + cleanup +
integration test through `GetFinalPathNameByHandleW`. Types consistent:
`NameWriteResult { bytes, status }`, `DirStatus` reused, `write_file_name_info`
and `nt_to_volume_relative` signatures match call sites. No placeholders.

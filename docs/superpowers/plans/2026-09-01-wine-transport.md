# Wine-hosted shim, increment 1: the transport — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the ring a shared-memory backing that a shim inside Wine and a
native Linux Director can both map, and make `vfs-embed` compile for Linux.

**Architecture:** Today the ring lives in a page-file-backed *named* Windows
section with named Windows events for wakeups — neither has any identity a Linux
process can open. Replace the backing with a **file**: the shim inside Wine keeps
calling `CreateFileMappingW`, now with a real file handle, and the Director
`mmap`s the same path. Wakeups use `SpinNotifier`, which already exists in
`vfs-ipc` and needs no OS object. Bidirectional coherence of exactly this
arrangement was measured 2026-09-01 (spec §1).

**Tech Stack:** Rust, `windows-sys` (existing, Windows only), `libc` (new, Unix
only), `vfs-ipc`'s existing ring.

**Spec:** `docs/superpowers/specs/2026-09-01-wine-hosted-shim-design.md`

**Scope note — read this before Task 1.** The spec's §9 scope covers the
transport, `vfs-proton`, and an end-to-end run under Wine. This plan is
**increment 1 of three** and delivers the transport only. `vfs-proton` and the
end-to-end run are increments 2 and 3. The spec's statement that the end-to-end
run is "the definition of done" applies to increment 3, not to this plan. Task 5
here is the transport's own end-to-end: our real ring carrying real traffic
between a Wine-hosted client and a Linux-native server.

## Global Constraints

- **No behaviour change on Windows.** The binding constraint. Every task ends
  with `cargo test --no-fail-fast` for the crates it touched, and the tally must
  be read — not `cargo test`, and never piped through `tail`, which truncates the
  log and understates failures.
- **`cargo clippy --all-targets -- -D warnings` must pass.** One clippy error
  masks every downstream crate.
- **`vfs-ipc` gains no dependency.** It has exactly one (`vfs-protocol`) and no
  external crates. `libc` goes in the new `vfs-unix` crate only.
- **`libc` is Unix-only** — `[target.'cfg(unix)'.dependencies]`. It must never
  enter the Windows build graph.
- **Existing named-section constructors keep working and keep their signatures.**
  `SharedMapping::create`/`open` are used by `vfs-director/src/ipc.rs` and
  `vfs-shim/src/fuse_client.rs`. This increment *adds* constructors.
- **`unsafe` blocks carry a `// SAFETY:` comment and `#[allow(unsafe_code)]`,**
  matching `crates/vfs-win/src/mapping.rs` exactly. The crate denies
  `unsafe_code` by default.
- **The protocol descriptor must not drift.** `bin/regen-protocol` then
  `git diff --exit-code resources/` stays clean. No wire-format change here — the
  ring's bytes are identical; only the memory's backing differs.
- **Running Linux tests** requires the Arch WSL box. The repo is cloned at
  `/root/aether-vfs`, whose `origin` **is** this Windows checkout, so:
  `wsl -d archlinux -u root -- bash -s` with the script **piped via stdin** and
  `MSYS_NO_PATHCONV=1` set. Do **not** pass scripts as `bash -c '<script>'`
  arguments — git-bash rewrites `/mnt/...`-shaped arguments and silently empties
  variables. Sync with
  `cd /root/aether-vfs && git fetch origin && git reset --hard origin/<branch>`,
  which only sees **committed** work.

---

### Task 1: File-backed mapping constructors in `vfs-win`

**Files:**
- Modify: `rust/crates/vfs-win/src/mapping.rs`
- Test: same file, `mod tests`

**Interfaces:**
- Consumes: `vfs_ipc::SharedSeg::from_raw`, `vfs_ipc::ring::{init, open}`.
- Produces: `SharedMapping::create_file_backed(path: &Path, size: usize) -> io::Result<Self>`
  and `SharedMapping::open_file_backed(path: &Path, size: usize) -> io::Result<Self>`.
  Task 5 consumes both. `seg()`, `len()`, `as_mut_ptr()` behave identically to the
  named-section constructors.

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `rust/crates/vfs-win/src/mapping.rs`:

```rust
    fn temp_path(tag: &str) -> std::path::PathBuf {
        let pid = std::process::id();
        std::env::temp_dir().join(format!("vfs-win-filemap-{pid}-{tag}.bin"))
    }

    #[test]
    fn file_backed_create_maps_a_writable_section() {
        let p = temp_path("create");
        let _ = std::fs::remove_file(&p);
        let m = SharedMapping::create_file_backed(&p, 64 * 1024).unwrap();
        assert_eq!(m.len(), 64 * 1024);
        // ring::init requires an 8-aligned writable base; success proves both.
        vfs_ipc::ring::init(m.seg(), 4, 256).unwrap();
        drop(m);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn file_backed_create_sizes_the_file_on_disk() {
        // The Linux side mmaps this file by length, so the file must actually be
        // `size` bytes long -- a sparse or zero-length file would give the
        // Director a SIGBUS on first touch rather than a clean error.
        let p = temp_path("sized");
        let _ = std::fs::remove_file(&p);
        let m = SharedMapping::create_file_backed(&p, 64 * 1024).unwrap();
        let meta = std::fs::metadata(&p).unwrap();
        assert_eq!(meta.len(), 64 * 1024, "backing file must be fully sized");
        drop(m);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn file_backed_open_aliases_the_same_bytes() {
        let p = temp_path("alias");
        let _ = std::fs::remove_file(&p);
        let creator = SharedMapping::create_file_backed(&p, 64 * 1024).unwrap();
        let geom_created = vfs_ipc::ring::init(creator.seg(), 4, 256).unwrap();
        let opener = SharedMapping::open_file_backed(&p, 64 * 1024).unwrap();
        let geom_opened = vfs_ipc::ring::open(opener.seg()).unwrap();
        assert_eq!(geom_created, geom_opened);
        drop(opener);
        drop(creator);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn file_backed_writes_are_visible_through_a_second_mapping() {
        // Coherence, not just aliasing: a byte written through one view must be
        // readable through the other. This is the property the Wine/Linux split
        // depends on.
        let p = temp_path("coherent");
        let _ = std::fs::remove_file(&p);
        let a = SharedMapping::create_file_backed(&p, 64 * 1024).unwrap();
        let b = SharedMapping::open_file_backed(&p, 64 * 1024).unwrap();
        // SAFETY: both views map the same 64 KiB file; writing one byte at a
        // fixed offset inside it, with no concurrent reader but `b` below.
        #[allow(unsafe_code)]
        unsafe {
            a.as_mut_ptr().add(4096).write_volatile(0xAB);
            assert_eq!(b.as_mut_ptr().add(4096).read_volatile(), 0xAB);
        }
        drop(b);
        drop(a);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn file_backed_open_missing_file_errors() {
        let p = temp_path("absent-xyz");
        let _ = std::fs::remove_file(&p);
        assert!(SharedMapping::open_file_backed(&p, 64 * 1024).is_err());
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p vfs-win --no-fail-fast` (from `rust/`)
Expected: FAIL — `no function or associated item named 'create_file_backed'`.

- [ ] **Step 3: Implement**

Add these imports to the existing `use` blocks in `mapping.rs`:

```rust
use std::path::Path;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
```

`vfs-win`'s manifest already enables `Win32_Storage_FileSystem`, so no manifest
change is needed. Add to `impl SharedMapping`:

```rust
    /// Create (or truncate) `path` at exactly `size` bytes, then map a
    /// read/write view of a **file-backed** section over it.
    ///
    /// The difference from [`Self::create`] is the first argument to
    /// `CreateFileMappingW`: a real file handle instead of
    /// `INVALID_HANDLE_VALUE`. That is the whole point — a page-file-backed
    /// section exists only inside one Windows (or Wine) session and has no
    /// identity a native Linux process can open, whereas a file-backed one is
    /// coherent with an `mmap` of the same path. Measured: a Wine process and a
    /// Linux process each saw the other's writes through this arrangement.
    ///
    /// The section is unnamed. Callers coordinate by **path**, not by section
    /// name, which is what lets the two sides agree across the boundary.
    pub fn create_file_backed(path: &Path, size: usize) -> io::Result<Self> {
        let file = open_backing(path, true)?;
        // The file must be exactly `size` bytes: the Linux side maps it by
        // length, and mapping past the end of a short file faults on touch
        // rather than failing at map time. `CreateFileMappingW` with a nonzero
        // size extends the file, but do it explicitly so the postcondition is
        // visible and testable.
        let (size_high, size_low) = split_size(size)?;
        // SAFETY: FFI. `file` is a valid writable handle owned here; passing it
        // as the mapping's backing store. `size` is nonzero for any real ring.
        #[allow(unsafe_code)]
        let handle = unsafe {
            CreateFileMappingW(
                file,
                core::ptr::null(),
                PAGE_READWRITE,
                size_high,
                size_low,
                core::ptr::null(),
            )
        };
        // The mapping holds its own reference to the file object, so the file
        // handle is closed here and the pages stay valid.
        // SAFETY: FFI. `file` is valid and closed exactly once.
        #[allow(unsafe_code)]
        unsafe {
            CloseHandle(file);
        }
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Self::map_view(handle, size)
    }

    /// Map a read/write view of an existing file at `path`, which must already
    /// be at least `size` bytes. See [`Self::create_file_backed`].
    pub fn open_file_backed(path: &Path, size: usize) -> io::Result<Self> {
        let file = open_backing(path, false)?;
        let (size_high, size_low) = split_size(size)?;
        // SAFETY: FFI. As in `create_file_backed`.
        #[allow(unsafe_code)]
        let handle = unsafe {
            CreateFileMappingW(
                file,
                core::ptr::null(),
                PAGE_READWRITE,
                size_high,
                size_low,
                core::ptr::null(),
            )
        };
        // SAFETY: FFI. `file` is valid and closed exactly once.
        #[allow(unsafe_code)]
        unsafe {
            CloseHandle(file);
        }
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Self::map_view(handle, size)
    }
```

And this free function beside `to_wide`:

```rust
/// Open `path` for shared read/write. `create` truncates or creates; otherwise
/// the file must exist. Shared read+write so the other side can map it
/// concurrently — without `FILE_SHARE_*` the second opener gets a sharing
/// violation, which is exactly the case this transport exists to support.
fn open_backing(path: &Path, create: bool) -> io::Result<HANDLE> {
    let wide = to_wide(&path.to_string_lossy());
    let disposition = if create { CREATE_ALWAYS } else { OPEN_EXISTING };
    // SAFETY: FFI. `wide` is a valid NUL-terminated UTF-16 pointer for the call.
    #[allow(unsafe_code)]
    let file = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            core::ptr::null(),
            disposition,
            FILE_ATTRIBUTE_NORMAL,
            core::ptr::null_mut(),
        )
    };
    if file == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}
```

Note on `to_wide(&path.to_string_lossy())`: `to_string_lossy` is acceptable here
because ring paths are chosen by this codebase, not by a user. If a caller ever
passes a non-UTF-8 path this silently substitutes replacement characters and the
open fails with a confusing error — if that becomes a real risk, switch to
`std::os::windows::ffi::OsStrExt::encode_wide`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vfs-win --no-fail-fast`
Expected: PASS, including the three pre-existing named-section tests. Read the
tally; do not accept a truncated log.

- [ ] **Step 5: Clippy**

Run: `cargo clippy -p vfs-win --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add rust/crates/vfs-win/src/mapping.rs
git commit -m "feat(win): file-backed shared mappings, coordinated by path not section name"
```

---

### Task 2: The `vfs-unix` crate — the Linux half of the mapping

**Files:**
- Create: `rust/crates/vfs-unix/Cargo.toml`
- Create: `rust/crates/vfs-unix/src/lib.rs`
- Create: `rust/crates/vfs-unix/src/mapping.rs`
- Modify: `rust/Cargo.toml` (workspace members)

**Interfaces:**
- Consumes: `vfs_ipc::SharedSeg::from_raw`, `vfs_ipc::ring::{init, open}`.
- Produces: `vfs_unix::FileMapping` with
  `create(path: &Path, size: usize) -> io::Result<Self>`,
  `open(path: &Path, size: usize) -> io::Result<Self>`,
  `seg(&self) -> &SharedSeg`, `len(&self) -> usize`,
  `is_empty(&self) -> bool`, `as_mut_ptr(&self) -> *mut u8`.
  Deliberately mirrors `vfs_win::SharedMapping`'s accessors so ring code above it
  reads the same on both platforms. Task 5 consumes this.

- [ ] **Step 1: Create the manifest**

`rust/crates/vfs-unix/Cargo.toml`:

```toml
[package]
name = "vfs-unix"
version = "0.1.0"
edition = "2021"
publish = false
description = "Unix-side OS handles for the ring: file-backed shared memory"

[lib]
name = "vfs_unix"
path = "src/lib.rs"

[dependencies]
vfs-ipc = { path = "../vfs-ipc" }

# The single external dependency, and the reason this crate exists rather than
# the mapping living in `vfs-ipc`: Rust's std has no `mmap`, `vfs-ipc` has no
# external dependencies at all, and `libc` appears nowhere else in this
# workspace. Confining it to a `cfg(unix)` table keeps it out of every existing
# crate's graph and off the Windows build entirely.
[target.'cfg(unix)'.dependencies]
libc = "0.2"
```

- [ ] **Step 2: Add to the workspace**

In `rust/Cargo.toml`, add `"crates/vfs-unix",` to `members`, immediately after
the `"crates/vfs-win",` line, with this comment above it:

```toml
  # The Unix mirror of vfs-win: one crate per OS, each owning that OS's handles,
  # with the portable ring above both. Builds to an empty crate on Windows.
  "crates/vfs-unix",
```

- [ ] **Step 3: Write the failing test**

`rust/crates/vfs-unix/src/lib.rs`:

```rust
//! Unix-side OS handles for the ring.
//!
//! The mirror of `vfs-win`: it owns this platform's shared-memory primitive and
//! exposes it as a [`vfs_ipc::SharedSeg`], so the ring and snapshot code above
//! stay OS-independent. Everything here is `cfg(unix)`; on Windows this crate
//! builds to nothing.
#![deny(unsafe_code)]

#[cfg(unix)]
mod mapping;
#[cfg(unix)]
pub use mapping::FileMapping;
```

`rust/crates/vfs-unix/src/mapping.rs` — tests first, at the bottom of the file:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let pid = std::process::id();
        std::env::temp_dir().join(format!("vfs-unix-filemap-{pid}-{tag}.bin"))
    }

    #[test]
    fn create_maps_a_writable_segment() {
        let p = temp_path("create");
        let _ = std::fs::remove_file(&p);
        let m = FileMapping::create(&p, 64 * 1024).unwrap();
        assert_eq!(m.len(), 64 * 1024);
        vfs_ipc::ring::init(m.seg(), 4, 256).unwrap();
        drop(m);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn create_sizes_the_file_so_a_mapping_cannot_fault_on_touch() {
        // mmap beyond EOF succeeds and then SIGBUSes on access. The file is
        // ftruncate'd to `size` precisely so that cannot happen.
        let p = temp_path("sized");
        let _ = std::fs::remove_file(&p);
        let m = FileMapping::create(&p, 64 * 1024).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 64 * 1024);
        drop(m);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn open_sees_the_ring_the_creator_wrote() {
        let p = temp_path("alias");
        let _ = std::fs::remove_file(&p);
        let creator = FileMapping::create(&p, 64 * 1024).unwrap();
        let geom_created = vfs_ipc::ring::init(creator.seg(), 4, 256).unwrap();
        let opener = FileMapping::open(&p, 64 * 1024).unwrap();
        let geom_opened = vfs_ipc::ring::open(opener.seg()).unwrap();
        assert_eq!(geom_created, geom_opened);
        drop(opener);
        drop(creator);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn writes_through_one_mapping_are_visible_through_another() {
        let p = temp_path("coherent");
        let _ = std::fs::remove_file(&p);
        let a = FileMapping::create(&p, 64 * 1024).unwrap();
        let b = FileMapping::open(&p, 64 * 1024).unwrap();
        // SAFETY: both map the same 64 KiB file; one byte at a fixed offset.
        #[allow(unsafe_code)]
        unsafe {
            a.as_mut_ptr().add(4096).write_volatile(0xAB);
            assert_eq!(b.as_mut_ptr().add(4096).read_volatile(), 0xAB);
        }
        drop(b);
        drop(a);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn open_missing_file_errors() {
        let p = temp_path("absent-xyz");
        let _ = std::fs::remove_file(&p);
        assert!(FileMapping::open(&p, 64 * 1024).is_err());
    }

    #[test]
    fn open_too_short_file_errors_rather_than_mapping_a_fault() {
        // A file shorter than `size` would map and then SIGBUS. Refuse it.
        let p = temp_path("short");
        let _ = std::fs::remove_file(&p);
        std::fs::write(&p, [0u8; 128]).unwrap();
        assert!(FileMapping::open(&p, 64 * 1024).is_err());
        let _ = std::fs::remove_file(&p);
    }
}
```

- [ ] **Step 4: Run the tests to verify they fail**

This must run on Linux. From `rust/`, first confirm the crate is at least known
to cargo on Windows:

Run: `cargo check -p vfs-unix --target x86_64-unknown-linux-gnu`
Expected: FAIL — `mapping.rs` has no `FileMapping` yet.

- [ ] **Step 5: Implement**

Prepend to `rust/crates/vfs-unix/src/mapping.rs` (above the `mod tests`):

```rust
//! File-backed shared memory via `mmap`. All libc FFI is confined here.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

use vfs_ipc::SharedSeg;

/// RAII owner of an `mmap`ed region over a real file, exposed as a [`SharedSeg`].
///
/// The Unix counterpart of `vfs_win::SharedMapping`'s file-backed constructors.
/// Both sides agree by **path**: a Windows `CreateFileMappingW` over the same
/// file and this `mmap` are coherent, which is what lets a shim inside Wine and
/// a native Linux Director share one ring.
pub struct FileMapping {
    ptr: *mut u8,
    len: usize,
    seg: SharedSeg,
}

// SAFETY: the mapped pages are shared memory; all concurrent access is governed
// by the vfs-ipc ring protocol (atomics + seqlock) — the same rationale that
// makes `SharedSeg` itself `Send + Sync`.
#[allow(unsafe_code)]
unsafe impl Send for FileMapping {}
#[allow(unsafe_code)]
unsafe impl Sync for FileMapping {}

impl FileMapping {
    /// Create or truncate `path` to exactly `size` bytes and map it shared.
    pub fn create(path: &Path, size: usize) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        // Size the file before mapping. `mmap` past EOF succeeds and then
        // SIGBUSes on first touch, which would surface as a crash in the
        // Director rather than an error at setup.
        file.set_len(size as u64)?;
        Self::map(&file, size)
    }

    /// Map an existing file at `path`, which must be at least `size` bytes.
    pub fn open(path: &Path, size: usize) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let actual = file.metadata()?.len();
        if actual < size as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("backing file is {actual} bytes, need at least {size}"),
            ));
        }
        Self::map(&file, size)
    }

    fn map(file: &std::fs::File, size: usize) -> io::Result<Self> {
        if size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zero-length mapping",
            ));
        }
        // SAFETY: FFI. `fd` is a valid open read/write descriptor living for the
        // call; MAP_SHARED is required for cross-process coherence, which is the
        // entire purpose here. The kernel chooses the address (null hint).
        #[allow(unsafe_code)]
        let ptr = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let ptr = ptr as *mut u8;
        // SAFETY: `ptr` is valid for `size` bytes until this value is dropped,
        // and `mmap` returns page-aligned memory, satisfying the ring's 8-byte
        // atomics. The file descriptor may be closed now: the mapping keeps its
        // own reference to the underlying file.
        #[allow(unsafe_code)]
        let seg = unsafe { SharedSeg::from_raw(ptr, size) };
        Ok(Self { ptr, len: size, seg })
    }

    /// The mapped region as a `SharedSeg`.
    pub fn seg(&self) -> &SharedSeg {
        &self.seg
    }

    /// The mapped length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the mapping is zero-length (never true for a live mapping).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Raw start of the mapped region (for carving an arena after the ring).
    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.ptr
    }
}

impl Drop for FileMapping {
    fn drop(&mut self) {
        // SAFETY: FFI. `ptr`/`len` came from `mmap` above and are unmapped
        // exactly once here.
        #[allow(unsafe_code)]
        unsafe {
            libc::munmap(self.ptr as *mut core::ffi::c_void, self.len);
        }
    }
}
```

- [ ] **Step 6: Run the tests on Linux**

Commit first — the Arch clone syncs from git, so uncommitted work is invisible:

```bash
git add rust/crates/vfs-unix rust/Cargo.toml
git commit -m "wip: vfs-unix"
```

Then, from a bash shell, with the script piped via **stdin** (not `bash -c`):

```bash
BRANCH=$(git branch --show-current)
cat <<SCRIPT | MSYS_NO_PATHCONV=1 wsl.exe -d archlinux -u root -- bash -s
cd /root/aether-vfs
git fetch origin --quiet
git reset --hard origin/$BRANCH --quiet
cd rust
cargo test -p vfs-unix --no-fail-fast 2>&1 | tail -40
SCRIPT
```

Expected: 6 passed, 0 failed.

- [ ] **Step 7: Verify it is a no-op on Windows**

Run: `cargo build -p vfs-unix` and `cargo clippy -p vfs-unix --all-targets -- -D warnings`
Expected: both clean. The crate compiles to an empty lib on Windows, and `libc`
must not appear:

Run: `cargo tree -p vfs-unix | grep -i libc`
Expected: **no output**. If `libc` appears, the `cfg(unix)` table is wrong.

- [ ] **Step 8: Commit**

```bash
git add rust/crates/vfs-unix rust/Cargo.toml
git commit --amend -m "feat(unix): vfs-unix, the Unix mirror of vfs-win's shared mapping

Rust's std has no mmap, so the Linux side of the ring needs libc. It goes in a
new cfg(unix) crate rather than vfs-ipc, which has exactly one dependency and no
external crates at all and is the portable protocol core the Linux CI job
consumes. Confining libc here keeps it out of every existing crate's graph and
off the Windows build entirely.

Refuses a file shorter than the requested mapping: mmap past EOF succeeds and
then SIGBUSes on first touch, which would surface as a Director crash instead of
an error at setup."
```

---

### Task 3: Claim `vfs-unix` in the Linux CI job

**Files:**
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: the `vfs-unix` crate from Task 2.
- Produces: nothing code-facing.

- [ ] **Step 1: Add the crate to the portable job**

In `.github/workflows/ci.yml`, in job `rust-linux-portable`, step
"Portable Rust crates (compile+test on Linux)", append ` -p vfs-unix` to the
`cargo test` command. It already carries `--no-fail-fast`.

- [ ] **Step 2: Verify the YAML still parses**

Run: `python -c "import yaml; d=yaml.safe_load(open('.github/workflows/ci.yml')); print(list(d['jobs'].keys()))"`
Expected: `['rust-windows', 'node-addon-windows', 'rust-linux-portable']`

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: run vfs-unix's tests on Linux, where its code actually compiles"
```

---

### Task 4: Gate `vfs-embed`'s Windows-only dependencies

**Files:**
- Modify: `rust/crates/vfs-embed/Cargo.toml`
- Modify: `rust/crates/vfs-embed/src/session.rs`

**Interfaces:**
- Consumes: `vfs_provider::layout::overlay_layer_dir` (already portable, moved
  there in the earlier increment 1).
- Produces: a `vfs-embed` that `cargo check --target x86_64-unknown-linux-gnu`
  accepts. **No public API change**: `Session`'s methods keep their signatures.

**Context an implementer cannot infer:** `vfs-embed` currently fails to even
*check* for Linux because `vfs-shim` → `retour` → `libudis86-sys`, whose build
script needs a C cross-compiler. Gating must happen in `Cargo.toml` — a
source-level `cfg` cannot help, because the build script runs before any of our
code is compiled. The three coupling sites are `session.rs:394`
(`vfs_shim::overlay_layer_dir`), `session.rs:925`
(`vfs_shim::encode_config_with_overlay`), and `session.rs:1089`/`1169`/`1176`
(`vfs_inject::*`, the launch path).

- [ ] **Step 1: Move the Windows-only dependencies**

In `rust/crates/vfs-embed/Cargo.toml`, remove the `vfs-inject` and `vfs-shim`
lines from `[dependencies]` and add:

```toml
# Injection and the NT-hook shim are the Windows delivery mechanism. They are
# gated in the manifest, not with cfg in source: `vfs-shim` pulls `retour` ->
# `libudis86-sys`, whose build script is a C cross-compile that fails for a Linux
# target before any of our code is reached.
[target.'cfg(windows)'.dependencies]
vfs-inject = { path = "../vfs-inject" }
vfs-shim = { path = "../vfs-shim" }
```

Leave `[dev-dependencies]` alone for now; Step 3 revisits it if tests fail to
resolve on Linux.

- [ ] **Step 2: Redirect the portable call and gate the rest**

In `session.rs`, change the `overlay_layer_dir` call (near line 394) from
`vfs_shim::overlay_layer_dir(...)` to `vfs_provider::layout::overlay_layer_dir(...)`.
`vfs-shim` only re-exports it; the implementation already lives in
`vfs-provider/src/layout.rs:34`, so this is a re-import, not a move.

Gate the two genuinely-Windows sites with `#[cfg(windows)]` on the methods that
contain them. For each gated method, add a `#[cfg(not(windows))]` counterpart
with the identical signature returning a clear error, e.g.:

```rust
    /// Launch is Windows-only until the Proton path lands (increment 2 of
    /// docs/superpowers/specs/2026-09-01-wine-hosted-shim-design.md). The
    /// signature is identical on both targets so a host compiles unchanged.
    #[cfg(not(windows))]
    pub fn launch(&self, _exe: &str, _opts: LaunchOpts) -> Result<i32, String> {
        Err("launch requires the Proton runtime, which is not implemented yet \
             (see 2026-09-01-wine-hosted-shim-design.md, increment 2)"
            .to_string())
    }
```

Match the real signatures in the file rather than copying the sketch above
verbatim — read them first. Do not change any Windows-side signature.

- [ ] **Step 3: Verify Linux accepts it**

Run: `cargo check -p vfs-embed --target x86_64-unknown-linux-gnu`
Expected: succeeds. If `libudis86-sys` still builds, a Windows-only dependency
is still in the unconditional table — including possibly via `[dev-dependencies]`,
which resolves for `--target` too when checking tests. If `--tests` is needed to
reproduce, gate the dev-dependency the same way.

Then the structural gate, which is the one that actually holds:

Run: `cargo tree -p vfs-embed --target x86_64-unknown-linux-gnu | grep -iE "windows|retour|udis86"`
Expected: **no output**.

**Do not report `cargo check` alone as proof of portability.** It cannot detect a
`windows-sys` dependency at all — `windows-sys` emits extern declarations that
type-check on any target and fail only at link.
`cargo check --target x86_64-unknown-linux-gnu -p vfs-win` succeeds today.

- [ ] **Step 4: Verify Windows is unchanged**

Run: `cargo test -p vfs-embed --no-fail-fast` and
`cargo clippy -p vfs-embed --all-targets -- -D warnings`
Expected: both clean, with the same test count as before the task. Read the
tally.

- [ ] **Step 5: Commit**

```bash
git add rust/crates/vfs-embed/Cargo.toml rust/crates/vfs-embed/src/session.rs
git commit -m "refactor(embed): gate the Windows delivery mechanism in the manifest

vfs-embed could not even `cargo check` for Linux: vfs-shim pulls retour ->
libudis86-sys, whose build script is a C cross-compile. Manifest gating is
required because that build script runs before any cfg in our source matters.

overlay_layer_dir now comes from vfs-provider, where it has lived since the
Director was made OS-agnostic; vfs-shim only re-exported it."
```

---

### Task 5: Prove the real ring crosses the Wine/Linux boundary

**Files:**
- Create: `rust/crates/vfs-ring-harness/src/bin/ring-file-client.rs`
- Create: `rust/crates/vfs-ring-harness/src/bin/ring-file-server.rs`
- Modify: `rust/crates/vfs-ring-harness/Cargo.toml`

**Interfaces:**
- Consumes: `vfs_win::SharedMapping::open_file_backed` (Task 1),
  `vfs_unix::FileMapping::create` (Task 2), `vfs_ipc::{RingServer, RingClient,
  SpinNotifier, ring}`, `vfs_protocol` encoders/decoders.
- Produces: two binaries. `ring-file-server` is the Linux-native end;
  `ring-file-client` is the Windows end, run under Wine.

**Why this task exists:** every probe so far tested one mechanism in isolation,
and the mapping tests in Tasks 1–2 are same-platform. This is the first time
**our** ring carries **our** protocol between a Wine-hosted process and a
native Linux one. It is the definition of done for this increment.

**Read `crates/vfs-server/tests/fuse_e2e.rs` first** — it drives
`RingServer`/`RingClient` with `SpinNotifier` over an `OwnedSeg` in one process,
and is the closest existing model for the two halves here. (Its name predates
this work and refers to the FUSE-style RPC design; it mounts nothing.)

- [ ] **Step 1: Add the two binaries to the harness manifest**

In `rust/crates/vfs-ring-harness/Cargo.toml`, add:

```toml
# Two ends of one ring, deliberately separate binaries: the client is run under
# Wine and the server natively, so they cannot be one process.
[[bin]]
name = "ring-file-server"
path = "src/bin/ring-file-server.rs"

[[bin]]
name = "ring-file-client"
path = "src/bin/ring-file-client.rs"
```

Add `vfs-unix = { path = "../vfs-unix" }` under
`[target.'cfg(unix)'.dependencies]` and `vfs-win = { path = "../vfs-win" }`
under `[target.'cfg(windows)'.dependencies]`, creating either table if absent.
Read the existing manifest before editing; do not disturb the existing harness
binaries.

- [ ] **Step 2: Write the server (Linux end)**

`ring-file-server.rs` takes `<ring-path> <ring-bytes>`, creates the mapping with
`vfs_unix::FileMapping::create`, calls `vfs_ipc::ring::init` with the same
geometry the client will `ring::open`, then serves requests with `SpinNotifier`
until it has answered one `OP_GETATTR` and one `OP_READ`, printing a line per
request handled and exiting 0. Back it with a single in-memory file so the
assertions are about the ring, not about providers. Print
`SERVER: ready` **after** `ring::init` returns and flush stdout — the harness in
Step 4 waits for that line rather than sleeping a fixed interval.

Gate the whole binary `#![cfg(unix)]` with a `fn main() {}` fallback for Windows
so `cargo build --workspace` on Windows still succeeds.

- [ ] **Step 3: Write the client (Windows end, run under Wine)**

`ring-file-client.rs` takes `<ring-path> <ring-bytes>`, opens the mapping with
`vfs_win::SharedMapping::open_file_backed`, `ring::open`s it, sends one
`OP_GETATTR` and one `OP_READ` for the server's file via `RingClient` +
`SpinNotifier`, asserts the responses are `ST_OK` and that the read bytes match
the expected content, prints `CLIENT: OK` and exits 0 — nonzero with a diagnostic
on any mismatch. Gate `#![cfg(windows)]` with a `fn main() {}` fallback.

- [ ] **Step 4: Run both halves across the boundary**

Build the client for Windows and the server for Linux, then run them against one
file inside the Wine prefix so both sides can reach it. Commit first, then:

```bash
cargo build -p vfs-ring-harness --bin ring-file-client   # from rust/
BRANCH=$(git branch --show-current)
cat <<SCRIPT | MSYS_NO_PATHCONV=1 wsl.exe -d archlinux -u root -- bash -s
set -e
cd /root/aether-vfs && git fetch origin --quiet && git reset --hard origin/$BRANCH --quiet
cd rust && cargo build -p vfs-ring-harness --bin ring-file-server 2>&1 | tail -3

R=/root/aether/runtimes/GE-Proton11-6-x86_64
export WINEPREFIX=/root/aether/probe-prefix
export WINEDLLOVERRIDES="mscoree=d;mshtml=d"
export WINEDEBUG=-all
P=\$WINEPREFIX/drive_c/probe
mkdir -p \$P
cp /mnt/c/oss/aether-vfs/rust/target/debug/ring-file-client.exe \$P/
RING=\$P/ring.bin
rm -f \$RING

./target/debug/ring-file-server \$RING 65536 > /tmp/server.log 2>&1 &
SPID=\$!
for i in \$(seq 1 100); do grep -q 'SERVER: ready' /tmp/server.log && break; sleep 0.1; done
grep -q 'SERVER: ready' /tmp/server.log || { echo "SERVER NEVER READY"; cat /tmp/server.log; exit 1; }

cd \$P
timeout 120 \$R/files/bin/wine ring-file-client.exe 'C:\\probe\\ring.bin' 65536 2>&1 \
  | grep -viE 'freetype|equal to 2.0.5|www.freetype|fixme:|err:winediag|wineserver:'
echo "CLIENT_RC=\$?"
wait \$SPID 2>/dev/null || true
echo "--- server log ---"; cat /tmp/server.log
SCRIPT
```

Expected: `CLIENT: OK`, `CLIENT_RC=0`, and the server log showing it handled a
GETATTR and a READ.

**If the client hangs**, `SpinNotifier` on both ends means neither blocks — check
the geometry arguments match exactly between `ring::init` and `ring::open`, and
that `ring-bytes` is identical on both command lines. Bound every run with
`timeout` so a hang cannot wedge the session.

- [ ] **Step 5: Windows regression**

Run: `cargo test --no-fail-fast` (whole workspace, from `rust/`) with
`TMP=C:\vfstmp` and `TEMP=C:\vfstmp` set, and
`cargo clippy --all-targets -- -D warnings`.

Capture the full log to a file and read the tally from it; do **not** pipe
through `tail`, which truncates the log and understates failures. The Windows
suite also has a known intermittent hang in `vfs-directord`'s e2e binary — if the
run times out with `escape_matrix_positive_and_negative_canary` or the
`scenario_toml_*` tests outstanding, that is the known issue and not this task's
regression. Say so explicitly rather than reporting a pass.

- [ ] **Step 6: Commit**

```bash
git add rust/crates/vfs-ring-harness
git commit -m "test(ipc): the real ring, from a Wine-hosted client to a Linux server

First time our ring carries our protocol across the boundary rather than one
mechanism being probed in isolation. Client runs under GE-Proton and maps the
segment with CreateFileMappingW over a real file; server is native Linux and
mmaps the same path. SpinNotifier on both ends, so no OS event object is
involved."
```

---

## Self-Review

**Spec coverage.** §3's transport change → Tasks 1, 2, 5. §4's crate layout →
Tasks 2, 4. §7's gates → every task's verification steps, with the
`cargo tree --target` gate in Task 4 and the end-to-end in Task 5. §5
(`vfs-proton`) and §6 (identity gap) are **deliberately out of this plan** — see
the scope note in the header; they are increments 2 and 3.

**Type consistency.** `vfs_win::SharedMapping::{create_file_backed,
open_file_backed}` take `(&Path, usize)` and are named identically wherever
referenced. `vfs_unix::FileMapping::{create, open}` take `(&Path, usize)` and
expose `seg`/`len`/`is_empty`/`as_mut_ptr` — the same accessor names as
`SharedMapping`, which is what lets Task 5's two binaries read alike.

**Known soft spots, stated rather than hidden.** Task 5's Steps 2–3 describe the
two binaries in prose and name the model to copy (`fuse_e2e.rs`) instead of
giving complete code, because the exact `RingServer`/`RingClient` construction
must be read from that file rather than guessed. That is a real deviation from
this plan format's "no placeholders" rule; the mitigation is the named reference
plus fully specified argv, stdout contract, and exit codes. Task 4's Step 2
likewise says to read the real signatures rather than trusting a sketch, because
`session.rs` is 1332 lines and its `launch` signature was not transcribed here.

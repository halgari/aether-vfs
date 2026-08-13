# Stage 1: Provider Contract Foundations — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce the capability-typed `Provider` contract in a new `vfs-provider` crate and port every existing backend to it, with no change in observable behavior.

**Architecture:** `vfs-payload` leaves the workspace so the main tree can unwind (a prerequisite for the future PyO3 binding). A new `vfs-provider` crate owns `Capabilities`, `VPath`, `Handle`, `SetAttr`, the `Provider` trait, and a capability-parameterized conformance suite. Every existing `Backend` implementor is renamed and re-signatured to `Provider`. Root ids exist in the type system but every call site passes `RootId(0)` — making roots real is Stage 2.

**Tech Stack:** Rust 2021, cargo workspaces, tonic/prost (vfs-source), Windows-first.

## Global Constraints

- Design spec: `docs/superpowers/specs/2026-08-13-pluggable-providers-design.md`. Read §5 and §6 before starting.
- **No behavior change in this stage.** Every existing test must pass unmodified except where a type rename forces an edit. If a test's *assertions* need changing, stop and flag it — that means semantics moved.
- Working directory for all cargo commands is `C:\oss\aether-vfs\rust`.
- Zero clippy warnings: `cargo clippy --all-targets -- -D warnings` must be clean at every commit.
- One word for the concept: **`Provider`**. Not `Backend`, not `Source`.
- Status codes are `i32`, negative for errors. Existing codes run to `-7`; new codes are `-8` and `-9`.
- `RootId` is a newtype over `u32`. `Handle` is a bare `u64`.
- Tests use the existing isolation convention: temp dirs named with `std::process::id()`, cleaned up at the end of the test.
- Commit after every task. Conventional commit prefixes (`feat:`, `refactor:`, `build:`, `test:`).

---

## File Structure

**Created:**

| File | Responsibility |
|---|---|
| `crates/vfs-provider/Cargo.toml` | New crate manifest |
| `crates/vfs-provider/src/lib.rs` | Public re-exports and crate docs |
| `crates/vfs-provider/src/caps.rs` | `Access`, `Capabilities`, capability recomputation helpers |
| `crates/vfs-provider/src/path.rs` | `RootId`, `VPath` |
| `crates/vfs-provider/src/model.rs` | `Handle`, `Stat`, `DirEntry`, `SetAttr`, kind constants |
| `crates/vfs-provider/src/status.rs` | `ST_*` constants and helper constructors |
| `crates/vfs-provider/src/provider.rs` | The `Provider` trait |
| `crates/vfs-provider/src/conformance.rs` | Capability-parameterized conformance suite |

**Modified:** `Cargo.toml` (root), `crates/vfs-payload/Cargo.toml`, `crates/vfs-protocol/src/{lib,ops}.rs`, `crates/vfs-zip/src/backend.rs`, `crates/vfs-director/src/{ops,disk,director,session}.rs`, all of `crates/vfs-compose/src/`, `crates/vfs-cache/src/backend.rs`, `crates/vfs-source/src/{lib,remote,serve,conformance}.rs`, `crates/vfs-directord/src/registry.rs`, `crates/vfs-inject/tests/common/mod.rs`, `crates/vfs-directord/tests/e2e.rs`, `.github/workflows/ci.yml`, `README.md`.

---

### Task 1: Split `vfs-payload` out so the workspace can unwind

**Files:**
- Modify: `Cargo.toml` (root, lines 3-33 members list and the two profile blocks)
- Modify: `crates/vfs-payload/Cargo.toml`
- Modify: `crates/vfs-inject/tests/common/mod.rs:22-40`
- Modify: `crates/vfs-directord/tests/e2e.rs:60-80`
- Modify: `.github/workflows/ci.yml:14`
- Modify: `README.md:27,97`
- Test: `crates/vfs-protocol/tests/unwind.rs` (create)

**Interfaces:**
- Consumes: nothing.
- Produces: a workspace that unwinds on panic. Every later task depends on this only implicitly.

**Why this is first:** `catch_unwind` is a no-op under `panic = "abort"`, so the PyO3 binding in Stage 4 cannot convert Rust panics into Python exceptions. Doing it now means every later task is built and tested under the final profile.

- [ ] **Step 1: Write the failing test**

**A runtime `catch_unwind` check cannot work here.** Cargo always builds `--test` harnesses with `panic = "unwind"` regardless of the profile setting, so a `catch_unwind` test passes under both profiles and proves nothing. Assert the manifests instead — that is what actually catches the regression (someone setting `panic = "abort"` on the main workspace again).

Create `crates/vfs-protocol/tests/unwind.rs`:

```rust
//! The main workspace must unwind so the PyO3 binding can turn a Rust panic
//! into a Python exception instead of aborting the host process.
//!
//! This asserts the manifest rather than calling `catch_unwind`: Cargo always
//! builds `--test` harnesses with `panic = "unwind"` regardless of the profile
//! setting, so a runtime check passes under both and proves nothing.

#[test]
fn main_workspace_profiles_unwind() {
    let manifest =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml"))
            .expect("read the workspace manifest");

    let panic_lines: Vec<&str> = manifest
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("panic"))
        .collect();

    assert!(!panic_lines.is_empty(), "no panic setting found in the workspace manifest");
    for line in panic_lines {
        assert!(line.contains("unwind"), "main workspace must unwind, found: {line}");
    }
}

#[test]
fn vfs_payload_is_excluded_and_still_aborts() {
    let root = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml"))
        .expect("read the workspace manifest");
    assert!(
        root.contains(r#"exclude = ["crates/vfs-payload"]"#),
        "vfs-payload must stay excluded — it is #![no_std] and cannot unwind"
    );

    let payload = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../vfs-payload/Cargo.toml"
    ))
    .expect("read the vfs-payload manifest");
    assert!(
        payload.lines().map(str::trim).any(|l| l == r#"panic = "abort""#),
        "vfs-payload must keep panic = \"abort\""
    );
    assert!(payload.contains("[workspace]"), "vfs-payload must be its own workspace root");
}
```

- [ ] **Step 2: Run it to confirm it fails**

Run: `cargo test -p vfs-protocol --test unwind`

Expected: FAIL — `main workspace must unwind, found: panic = "abort"`, and the exclude assertion fails too. After Steps 3-4, confirm the test has teeth by flipping the root manifest back to `panic = "abort"`, re-running (must FAIL), and restoring it (must PASS).

- [ ] **Step 3: Give `vfs-payload` its own workspace**

In `crates/vfs-payload/Cargo.toml`, add an empty `[workspace]` table (this makes the crate its own workspace root) and pin its profiles to abort:

```toml
[package]
name = "vfs-payload"
version = "0.1.0"
edition = "2021"

# Own workspace root: this crate is `#![no_std]` with a custom #[panic_handler]
# and cannot unwind, while the main workspace must unwind for the PyO3 binding.
[workspace]

[profile.dev]
panic = "abort"

[profile.release]
panic = "abort"

[lib]
crate-type = ["cdylib"]
name = "vfs_payload"
doctest = false
bench = false

[dependencies]
```

- [ ] **Step 4: Exclude it from the root workspace and switch to unwind**

In `rust/Cargo.toml`, remove the `"crates/vfs-payload",` line from `members`, add an `exclude`, and replace the two profile blocks:

```toml
exclude = ["crates/vfs-payload"]

# `vfs-payload` is excluded from this workspace (it is #![no_std] with a custom
# panic_handler and must abort). Everything here unwinds so the library can
# catch panics at FFI boundaries instead of killing its host process.
[profile.dev]
panic = "unwind"

[profile.release]
panic = "unwind"
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p vfs-protocol --test unwind`

Expected: PASS.

- [ ] **Step 6: Fix the nested payload builds**

`vfs-payload` is no longer a workspace member, so `cargo build -p vfs-payload` from the root fails with "package(s) not found". Two call sites build it.

In `crates/vfs-inject/tests/common/mod.rs`, replace the single `cmd` block inside `FIXTURES.call_once` (lines 22-41) with two invocations. The payload build points `CARGO_TARGET_DIR` at the main target directory so every existing artifact-location path keeps working:

```rust
        let target_dir = workspace.join("target");

        // Main-workspace fixtures.
        let mut cmd = Command::new(&cargo);
        cmd.current_dir(&workspace).args([
            "build",
            "-p",
            "vfs-shim-dll",
            "-p",
            "vfs-fixture-vproxy",
            "-p",
            "vfs-fixture-staticimp",
            "--quiet",
        ]);
        if !cfg!(debug_assertions) {
            cmd.arg("--release");
        }
        let status = cmd.status().expect("spawn cargo to build fixtures");
        assert!(status.success(), "fixture cargo build failed: {status}");

        // vfs-payload lives in its own workspace (panic = "abort"). Build it
        // into the same target dir so `locate_artifact` finds it unchanged.
        let mut pay = Command::new(&cargo);
        pay.current_dir(&workspace)
            .env("CARGO_TARGET_DIR", &target_dir)
            .args([
                "build",
                "--manifest-path",
                "crates/vfs-payload/Cargo.toml",
                "--quiet",
            ]);
        if !cfg!(debug_assertions) {
            pay.arg("--release");
        }
        let status = pay.status().expect("spawn cargo to build vfs-payload");
        assert!(status.success(), "vfs-payload cargo build failed: {status}");
```

- [ ] **Step 7: Fix the second nested build**

In `crates/vfs-directord/tests/e2e.rs`, the args array around line 71 lists `"vfs-payload"`. Remove that entry (and its preceding `"-p"`) from the array, then add a second `Command` after the existing one, using the same `--manifest-path` + `CARGO_TARGET_DIR` shape as Step 6. Update the message at line 47 to read `build -p vfs-fixture-read -p vfs-shim-dll and vfs-payload (--manifest-path crates/vfs-payload/Cargo.toml) first`.

- [ ] **Step 8: Update CI and README**

`.github/workflows/ci.yml:14` — remove `-p vfs-payload` from the build line and add a following step:

```yaml
      - name: Build vfs-payload (separate workspace)
        working-directory: rust
        run: cargo build --manifest-path crates/vfs-payload/Cargo.toml
        env:
          CARGO_TARGET_DIR: target
```

`README.md` lines 27 and 97 — remove `-p vfs-payload` from both build commands and add below each. Note the two differ: line 27 is the debug quick-start, line 97 is the release packaging section, whose surrounding text promises artifacts under `target/release/`.

Both need `--target-dir target`. Without it the DLL builds into `crates/vfs-payload/target/` and every artifact-location path — `vfs-inject/src/artifacts.rs`, `Session::launch` — fails to find it at runtime.

Below line 27:

```powershell
cargo build --manifest-path crates/vfs-payload/Cargo.toml --target-dir target   # separate workspace
```

Below line 97:

```powershell
cargo build --release --manifest-path crates/vfs-payload/Cargo.toml --target-dir target   # separate workspace
```

Verify by running the packaging line from `rust/` and confirming `target/release/vfs_payload.dll` exists.

- [ ] **Step 9: Verify the whole tree still builds and tests**

Run: `cargo build --all-targets`
Expected: success.

Run: `cargo build --manifest-path crates/vfs-payload/Cargo.toml`
Expected: success, and `target/debug/vfs_payload.dll` exists (check with `Test-Path target/debug/vfs_payload.dll` — if `CARGO_TARGET_DIR` is unset it lands in `crates/vfs-payload/target/debug/` instead, which is the wrong place).

Run: `cargo test --workspace`
Expected: the same set of passes as before this task. Injection tests that need the payload DLL must still find it.

Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 10: Commit**

```bash
git add rust/Cargo.toml rust/crates/vfs-payload/Cargo.toml \
        rust/crates/vfs-protocol/tests/unwind.rs \
        rust/crates/vfs-inject/tests/common/mod.rs \
        rust/crates/vfs-directord/tests/e2e.rs \
        .github/workflows/ci.yml README.md
git commit -m "build: split vfs-payload into its own workspace so the main tree unwinds

catch_unwind is a no-op under panic=abort, so the PyO3 binding could not
turn a Rust panic into a Python exception — it would abort the host
process instead. vfs-payload is the only crate that needs abort, has no
dependencies, and was already built via nested cargo, so excluding it
costs two --manifest-path invocations."
```

---

### Task 2: `vfs-provider` crate with capability types

**Files:**
- Create: `crates/vfs-provider/Cargo.toml`
- Create: `crates/vfs-provider/src/lib.rs`
- Create: `crates/vfs-provider/src/caps.rs`
- Modify: `Cargo.toml` (root, add member)
- Test: inline `#[cfg(test)]` in `caps.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `Access` (enum: `SeqRead`, `Read`, `ReadWrite`), `Capabilities { access, immutable, slow, preferred_block }`, `Capabilities::read_only()`, `Capabilities::validate() -> Result<(), &'static str>`, and the recomputation helpers `Capabilities::weakest(iter)`, `Capabilities::cached()`, `Capabilities::seekable()`, `Capabilities::read_only_clamp()`.

- [ ] **Step 1: Create the crate manifest and register it**

`crates/vfs-provider/Cargo.toml`:

```toml
[package]
name = "vfs-provider"
version = "0.1.0"
edition = "2021"

[dependencies]
```

In `rust/Cargo.toml`, add `"crates/vfs-provider",` to `members`, immediately after `"crates/vfs-core",`.

- [ ] **Step 2: Write the failing tests**

Create `crates/vfs-provider/src/caps.rs` containing only this test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_write_plus_immutable_is_rejected() {
        let c = Capabilities { access: Access::ReadWrite, immutable: true, ..Capabilities::read_only() };
        assert!(c.validate().is_err());
    }

    #[test]
    fn a_plain_read_only_provider_validates() {
        assert!(Capabilities::read_only().validate().is_ok());
    }

    #[test]
    fn seekable_promotes_sequential_to_positional() {
        let seq = Capabilities { access: Access::SeqRead, ..Capabilities::read_only() };
        assert_eq!(seq.seekable().access, Access::Read);
    }

    #[test]
    fn seekable_leaves_an_already_positional_provider_alone() {
        let rw = Capabilities { access: Access::ReadWrite, ..Capabilities::read_only() };
        assert_eq!(rw.seekable().access, Access::ReadWrite);
    }

    #[test]
    fn caching_clears_the_slow_marker() {
        let slow = Capabilities { slow: true, ..Capabilities::read_only() };
        assert!(!slow.cached().slow);
    }

    #[test]
    fn read_only_clamp_demotes_write_access() {
        let rw = Capabilities { access: Access::ReadWrite, ..Capabilities::read_only() };
        assert_eq!(rw.read_only_clamp().access, Access::Read);
    }

    #[test]
    fn weakest_takes_the_lowest_access_and_ands_immutability() {
        let rw = Capabilities { access: Access::ReadWrite, immutable: false, ..Capabilities::read_only() };
        let ro = Capabilities { access: Access::Read, immutable: true, ..Capabilities::read_only() };
        let w = Capabilities::weakest([rw, ro]);
        assert_eq!(w.access, Access::Read);
        assert!(!w.immutable);
    }

    #[test]
    fn weakest_of_nothing_is_read_only() {
        assert_eq!(Capabilities::weakest([]).access, Access::Read);
    }

    #[test]
    fn weakest_marks_slow_if_any_child_is_slow() {
        let fast = Capabilities::read_only();
        let slow = Capabilities { slow: true, ..Capabilities::read_only() };
        assert!(Capabilities::weakest([fast, slow]).slow);
    }
}
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test -p vfs-provider`
Expected: compile error — `Capabilities` and `Access` are not defined.

- [ ] **Step 4: Implement the types**

Prepend to `crates/vfs-provider/src/caps.rs`:

```rust
//! What a provider can do. Declared, not probed: the composition layer reads
//! these at construction time to validate a stack and to warn about one that
//! will perform badly.

/// Access level. `ReadWrite` implies positional read; there is no
/// write-without-seek tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Access {
    /// Forward-only reads via `read_next`. Must be wrapped in `seekable`.
    SeqRead,
    /// Positional reads via `read_at`.
    Read,
    /// Positional reads and writes.
    ReadWrite,
}

/// A provider's declared capabilities.
///
/// `immutable` and `slow` are orthogonal and the pair is what carries
/// information: `immutable` says caching is *safe*, `slow` says it is
/// *warranted*. Only both together justify persisting blocks to disk across
/// sessions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub access: Access,
    /// Content never changes for the provider's lifetime.
    pub immutable: bool,
    /// Reads are expensive; this provider should sit behind a cache.
    pub slow: bool,
    /// Block-size hint for `cached`. `None` means "caller decides".
    pub preferred_block: Option<u32>,
}

impl Capabilities {
    /// A fast, mutable, positional read-only provider — the common default.
    pub fn read_only() -> Self {
        Capabilities { access: Access::Read, immutable: false, slow: false, preferred_block: None }
    }

    /// Reject self-contradictory declarations. Called at construction.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.access == Access::ReadWrite && self.immutable {
            return Err("a ReadWrite provider cannot be immutable");
        }
        Ok(())
    }

    /// Capabilities of `seekable(self)`: sequential becomes positional.
    pub fn seekable(self) -> Self {
        let access = if self.access == Access::SeqRead { Access::Read } else { self.access };
        Capabilities { access, ..self }
    }

    /// Capabilities of `cached(self)`: access passes through, slow is answered.
    pub fn cached(self) -> Self {
        Capabilities { slow: false, ..self }
    }

    /// Capabilities of `readonly(self)`: write access is demoted.
    pub fn read_only_clamp(self) -> Self {
        let access = if self.access == Access::ReadWrite { Access::Read } else { self.access };
        Capabilities { access, ..self }
    }

    /// Capabilities of a combinator over several children: the weakest access,
    /// immutable only if all are, slow if any is, smallest block hint present.
    pub fn weakest(children: impl IntoIterator<Item = Capabilities>) -> Self {
        let mut out: Option<Capabilities> = None;
        for c in children {
            out = Some(match out {
                None => c,
                Some(acc) => Capabilities {
                    access: acc.access.min(c.access),
                    immutable: acc.immutable && c.immutable,
                    slow: acc.slow || c.slow,
                    preferred_block: match (acc.preferred_block, c.preferred_block) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    },
                },
            });
        }
        out.unwrap_or_else(Capabilities::read_only)
    }
}
```

Note: `Access` derives `Ord` with variants declared weakest-first, so `min` is exactly "weakest access".

- [ ] **Step 5: Create the crate root**

`crates/vfs-provider/src/lib.rs`:

```rust
#![forbid(unsafe_code)]
//! The provider contract: what a filesystem provider can do, how it is
//! addressed, and the conformance suite that holds every implementation —
//! Rust or host-language — to the same standard.

mod caps;

pub use caps::{Access, Capabilities};
```

- [ ] **Step 6: Run to verify it passes**

Run: `cargo test -p vfs-provider`
Expected: 9 passed.

Run: `cargo clippy -p vfs-provider --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add rust/Cargo.toml rust/crates/vfs-provider
git commit -m "feat(provider): capability types and recomputation rules

Access/Capabilities plus the algebra combinators use to derive their own
capabilities from their children."
```

---

### Task 3: Addressing, model types, status codes, and the `Provider` trait

**Files:**
- Create: `crates/vfs-provider/src/path.rs`
- Create: `crates/vfs-provider/src/model.rs`
- Create: `crates/vfs-provider/src/status.rs`
- Create: `crates/vfs-provider/src/provider.rs`
- Modify: `crates/vfs-provider/src/lib.rs`

**Interfaces:**
- Consumes: `Access`, `Capabilities` from Task 2.
- Produces:
  - `RootId(pub u32)` with `RootId::DEFAULT` (= `RootId(0)`).
  - `VPath<'a> { root: RootId, rel: &'a str }` with `VPath::new(root, rel)` and `VPath::at_default(rel)`.
  - `Handle = u64`, `Stat { kind: u8, size: u64, mtime: i64 }`, `DirEntry { name: String, stat: Stat }`, `SetAttr { mtime: Option<i64>, size: Option<u64> }`, `KIND_FILE = 1`, `KIND_DIR = 2`, `KIND_TOMBSTONE = 3`.
  - `ST_OK = 0` … `ST_NO_SPACE = -7`, `ST_NOT_SUPPORTED = -8`, `ST_READ_ONLY = -9`, plus `not_found()`, `bad_fh()`, `is_dir()`, `not_a_dir()`, `bad_request()`, `map_io_err()`, `not_supported()`, `read_only()`.
  - `OPEN_READ = 1`, `OPEN_WRITE = 2`, `OPEN_CREATE = 4`, `OPEN_EXCL = 8`, `OPEN_TRUNC = 16`, `OPEN_APPEND = 32`.
  - `trait Provider` as specified in spec §5.

**Note on `Stat`/`DirEntry`:** these are structurally identical to the ones in `vfs-protocol::ops`. Task 5 deletes the `vfs-protocol` copies and re-exports these, so there is exactly one definition.

- [ ] **Step 1: Write the failing tests**

Create `crates/vfs-provider/src/path.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_relative_path_under_two_roots_differs() {
        assert_ne!(VPath::new(RootId(0), "foo/bar"), VPath::new(RootId(1), "foo/bar"));
    }

    #[test]
    fn at_default_uses_root_zero() {
        assert_eq!(VPath::at_default("a").root, RootId::DEFAULT);
        assert_eq!(RootId::DEFAULT, RootId(0));
    }

    #[test]
    fn the_provider_root_is_the_empty_string() {
        assert_eq!(VPath::at_default("").rel, "");
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p vfs-provider`
Expected: compile error — `VPath` and `RootId` are not defined.

- [ ] **Step 3: Implement addressing**

Prepend to `crates/vfs-provider/src/path.rs`:

```rust
//! Root-scoped addressing. Every path handed to a provider is a `(root,
//! relative path)` pair, so one provider instance can serve several roots and
//! still tell `[1, "foo/bar"]` from `[0, "foo/bar"]`.

/// Identifies one virtualized filesystem location within a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RootId(pub u32);

impl RootId {
    /// The root every single-root session and every Stage-1 call site uses.
    pub const DEFAULT: RootId = RootId(0);
}

/// A path as a provider sees it: normalized, forward-slash separated, no
/// leading slash, provider root is `""`, original case preserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VPath<'a> {
    pub root: RootId,
    pub rel: &'a str,
}

impl<'a> VPath<'a> {
    pub fn new(root: RootId, rel: &'a str) -> Self {
        VPath { root, rel }
    }

    /// Address under [`RootId::DEFAULT`].
    pub fn at_default(rel: &'a str) -> Self {
        VPath { root: RootId::DEFAULT, rel }
    }
}
```

- [ ] **Step 4: Implement the model types**

Create `crates/vfs-provider/src/model.rs`:

```rust
//! Value types crossing the provider boundary.

/// Ops-layer file kind. Not the same encoding as `vfs-shared` snapshot kinds.
pub const KIND_FILE: u8 = 1;
pub const KIND_DIR: u8 = 2;
pub const KIND_TOMBSTONE: u8 = 3;

/// An opaque handle, scoped to the provider that issued it.
pub type Handle = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub kind: u8,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub stat: Stat,
}

/// Attribute change by path. `None` means "leave alone". `size` is present
/// because NT sets end-of-file by path as well as by handle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SetAttr {
    pub mtime: Option<i64>,
    pub size: Option<u64>,
}
```

- [ ] **Step 5: Implement status codes and open flags**

Create `crates/vfs-provider/src/status.rs`:

```rust
//! Status codes crossing the provider boundary, and open-request flags.
//!
//! Values `0` through `-7` are fixed by the existing ring protocol and must
//! not be renumbered.

pub const ST_OK: i32 = 0;
pub const ST_NOT_FOUND: i32 = -1;
pub const ST_NOT_A_DIRECTORY: i32 = -2;
pub const ST_BAD_REQUEST: i32 = -3;
pub const ST_IO_ERROR: i32 = -4;
pub const ST_IS_DIR: i32 = -5;
pub const ST_BAD_FH: i32 = -6;
pub const ST_NO_SPACE: i32 = -7;
/// The provider does not implement this method.
pub const ST_NOT_SUPPORTED: i32 = -8;
/// No `ReadWrite` provider serves this path.
pub const ST_READ_ONLY: i32 = -9;

pub fn ok() -> i32 { ST_OK }
pub fn not_found() -> i32 { ST_NOT_FOUND }
pub fn not_a_dir() -> i32 { ST_NOT_A_DIRECTORY }
pub fn bad_request() -> i32 { ST_BAD_REQUEST }
pub fn map_io_err() -> i32 { ST_IO_ERROR }
pub fn is_dir() -> i32 { ST_IS_DIR }
pub fn bad_fh() -> i32 { ST_BAD_FH }
pub fn not_supported() -> i32 { ST_NOT_SUPPORTED }
pub fn read_only() -> i32 { ST_READ_ONLY }

/// Open wants read access.
pub const OPEN_READ: u32 = 1;
/// Open wants write access.
pub const OPEN_WRITE: u32 = 2;
/// Create if absent (`OPEN_ALWAYS` / `CREATE_ALWAYS`).
pub const OPEN_CREATE: u32 = 4;
/// Fail if present (`CREATE_NEW`).
pub const OPEN_EXCL: u32 = 8;
/// Truncate on open (`TRUNCATE_EXISTING`).
pub const OPEN_TRUNC: u32 = 16;
/// Append-only writes (`FILE_APPEND_DATA`); the director resolves the offset.
pub const OPEN_APPEND: u32 = 32;
```

- [ ] **Step 6: Implement the trait**

Create `crates/vfs-provider/src/provider.rs`:

```rust
//! The provider contract. Everything past the read core defaults to
//! `ST_NOT_SUPPORTED`, so a read-only provider implements five methods.

use crate::caps::Capabilities;
use crate::model::{DirEntry, Handle, SetAttr, Stat};
use crate::path::VPath;
use crate::status::not_supported;

pub trait Provider: Send + Sync {
    /// Constant for the provider's lifetime; read once at construction.
    fn capabilities(&self) -> Capabilities;

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32>;
    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32>;
    /// Returns `(handle, size, is_dir)`.
    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32>;
    fn close(&self, h: Handle) -> Result<(), i32>;

    /// Positional read. Short reads are legal anywhere, not only at EOF.
    fn read_at(&self, _h: Handle, _offset: u64, _buf: &mut [u8]) -> Result<usize, i32> {
        Err(not_supported())
    }

    /// Forward-only read for `Access::SeqRead` providers.
    fn read_next(&self, _h: Handle, _buf: &mut [u8]) -> Result<usize, i32> {
        Err(not_supported())
    }

    fn write_at(&self, _h: Handle, _offset: u64, _buf: &[u8]) -> Result<usize, i32> {
        Err(not_supported())
    }
    fn set_len(&self, _h: Handle, _len: u64) -> Result<(), i32> {
        Err(not_supported())
    }
    fn flush(&self, _h: Handle) -> Result<(), i32> {
        Err(not_supported())
    }
    fn mkdir(&self, _p: VPath) -> Result<(), i32> {
        Err(not_supported())
    }
    fn remove(&self, _p: VPath) -> Result<(), i32> {
        Err(not_supported())
    }
    fn rename(&self, _from: VPath, _to: VPath) -> Result<(), i32> {
        Err(not_supported())
    }
    fn set_attr(&self, _p: VPath, _attr: SetAttr) -> Result<(), i32> {
        Err(not_supported())
    }
}
```

- [ ] **Step 7: Wire up the crate root**

Replace `crates/vfs-provider/src/lib.rs`:

```rust
#![forbid(unsafe_code)]
//! The provider contract: what a filesystem provider can do, how it is
//! addressed, and the conformance suite that holds every implementation —
//! Rust or host-language — to the same standard.

mod caps;
mod model;
mod path;
mod provider;
mod status;

pub use caps::{Access, Capabilities};
pub use model::{DirEntry, Handle, SetAttr, Stat, KIND_DIR, KIND_FILE, KIND_TOMBSTONE};
pub use path::{RootId, VPath};
pub use provider::Provider;
pub use status::{
    bad_fh, bad_request, is_dir, map_io_err, not_a_dir, not_found, not_supported, ok, read_only,
    OPEN_APPEND, OPEN_CREATE, OPEN_EXCL, OPEN_READ, OPEN_TRUNC, OPEN_WRITE, ST_BAD_FH,
    ST_BAD_REQUEST, ST_IO_ERROR, ST_IS_DIR, ST_NOT_A_DIRECTORY, ST_NOT_FOUND, ST_NOT_SUPPORTED,
    ST_NO_SPACE, ST_OK, ST_READ_ONLY,
};
```

- [ ] **Step 8: Add a trait-level test**

Append to `crates/vfs-provider/src/provider.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::Capabilities;
    use crate::status::ST_NOT_SUPPORTED;

    /// The minimum a read-only provider must implement.
    struct Minimal;

    impl Provider for Minimal {
        fn capabilities(&self) -> Capabilities {
            Capabilities::read_only()
        }
        fn getattr(&self, _p: VPath) -> Result<Option<Stat>, i32> {
            Ok(None)
        }
        fn readdir(&self, _p: VPath) -> Result<Vec<DirEntry>, i32> {
            Ok(Vec::new())
        }
        fn open(&self, _p: VPath, _flags: u32) -> Result<(Handle, u64, bool), i32> {
            Err(crate::status::not_found())
        }
        fn close(&self, _h: Handle) -> Result<(), i32> {
            Ok(())
        }
        fn read_at(&self, _h: Handle, _o: u64, _b: &mut [u8]) -> Result<usize, i32> {
            Ok(0)
        }
    }

    #[test]
    fn unimplemented_methods_report_not_supported() {
        let p = Minimal;
        assert_eq!(p.write_at(0, 0, b"x"), Err(ST_NOT_SUPPORTED));
        assert_eq!(p.mkdir(VPath::at_default("d")), Err(ST_NOT_SUPPORTED));
        assert_eq!(p.read_next(0, &mut [0u8; 4]), Err(ST_NOT_SUPPORTED));
        assert_eq!(p.set_attr(VPath::at_default("f"), SetAttr::default()), Err(ST_NOT_SUPPORTED));
    }

    #[test]
    fn a_minimal_provider_is_object_safe() {
        let p: std::sync::Arc<dyn Provider> = std::sync::Arc::new(Minimal);
        assert_eq!(p.capabilities().access, crate::caps::Access::Read);
    }
}
```

- [ ] **Step 9: Run to verify everything passes**

Run: `cargo test -p vfs-provider`
Expected: 14 passed (9 from Task 2, 3 path, 2 provider).

Run: `cargo clippy -p vfs-provider --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 10: Commit**

```bash
git add rust/crates/vfs-provider
git commit -m "feat(provider): addressing, model types, status codes, and the trait

VPath carries (RootId, relative path) so one provider instance can serve
several roots. Everything past the read core defaults to ST_NOT_SUPPORTED,
so a read-only provider implements five methods."
```

---

### Task 4: Capability-parameterized conformance suite (read cases)

**Files:**
- Create: `crates/vfs-provider/src/conformance.rs`
- Modify: `crates/vfs-provider/src/lib.rs`

**Interfaces:**
- Consumes: everything from Tasks 2-3.
- Produces:
  - `assert_conformance(p: Arc<dyn Provider>)` — runs the case subset implied by `p.capabilities()`.
  - `write_fixture_tree(dir: &Path)` — writes the reference tree the suite expects. Moved from `vfs-source::conformance` so there is one definition.
  - `MemFixture` — a minimal in-crate `Access::Read` provider over a fixed tree, used to test the suite itself.
  - `SeqFixture` — an `Access::SeqRead` provider over the same tree, with a per-handle cursor and **no** `read_at` (it inherits the `ST_NOT_SUPPORTED` default). It exists because no crate in Tasks 5-9 ports a sequential-only backend, so without it `assert_sequential` would ship with zero coverage — dead code inside the oracle six ports depend on. Stage 2's `seekable(p)` combinator also needs it as the thing to wrap. It delegates `getattr` and `readdir` to an internal `MemFixture` rather than duplicating the tree walk.

**The reference tree** every provider under test must expose:

```text
a.txt          "hello"
sub/b.txt      "world!"
```

This is **not** the superseded suite's tree (`hello.txt`, `sub/a.bin` = `"abc"`), so each port in Tasks 5-9 must write this tree rather than reusing an old fixture. The superseded suite also compared names with `eq_ignore_ascii_case`; this one compares exactly. Both changes are deliberate: case-insensitive matching is not a universal provider obligation in this design — the `casefold` combinator handles case-sensitive providers — so the suite must not silently accept a provider that returns the wrong case.

**Write cases are deliberately absent.** They arrive in Stage 3 with the write path. This task builds the dispatch structure so adding them later is additive.

- [ ] **Step 1: Read the existing suite before replacing it**

Read `crates/vfs-source/src/conformance.rs` in full. The new suite must assert at least everything it does, or providers could regress silently. Produce a comparison table in your report: every assertion the old suite makes, and where the new suite makes it. Any gap is a finding, not a footnote — six ports are validated by this suite.

- [ ] **Step 2: Write the failing test**

Create `crates/vfs-provider/src/conformance.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reference_fixture_passes_its_own_suite() {
        assert_conformance(std::sync::Arc::new(MemFixture::new()));
    }

    #[test]
    #[should_panic(expected = "getattr")]
    fn a_provider_that_loses_a_file_fails_the_suite() {
        assert_conformance(std::sync::Arc::new(MemFixture::missing("a.txt")));
    }

    /// Serves different content per root, proving `VPath` carries the root id
    /// through. This is not an obligation on real providers — a zip serves one
    /// archive under every root — it verifies the plumbing, not the contract.
    struct PerRootFixture;

    impl Provider for PerRootFixture {
        fn capabilities(&self) -> Capabilities {
            Capabilities::read_only()
        }
        fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
            Ok(Some(Stat { kind: KIND_FILE, size: u64::from(p.root.0), mtime: 0 }))
        }
        fn readdir(&self, _p: VPath) -> Result<Vec<DirEntry>, i32> {
            Ok(Vec::new())
        }
        fn open(&self, _p: VPath, _f: u32) -> Result<(Handle, u64, bool), i32> {
            Err(not_found())
        }
        fn close(&self, _h: Handle) -> Result<(), i32> {
            Ok(())
        }
    }

    #[test]
    fn vpath_carries_the_root_id_to_the_provider() {
        let p = PerRootFixture;
        let at0 = p.getattr(VPath::new(RootId(0), "same")).unwrap().unwrap();
        let at3 = p.getattr(VPath::new(RootId(3), "same")).unwrap().unwrap();
        assert_eq!(at0.size, 0);
        assert_eq!(at3.size, 3, "the provider did not receive the root id");
    }
}
```

**Why there is no "provider ignores the root id" negative fixture:** ignoring the root id is *legal*. A provider over one zip or one directory serves the same tree under every root, exactly as spec §5 permits. Asserting that a relative path resolves under an arbitrary root is passed trivially by such a provider and would wrongly fail a correct multi-root provider that reports not-found for a root it does not serve. `MemFixture` therefore has no `root_aware` flag; `PerRootFixture` above verifies the plumbing instead.

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test -p vfs-provider --lib conformance`
Expected: compile error — `assert_conformance` and `MemFixture` are not defined.

- [ ] **Step 4: Implement the fixture provider**

Prepend to `crates/vfs-provider/src/conformance.rs`:

```rust
//! One conformance suite, run against every provider in every language.
//!
//! Cases are selected by the provider's *declared* capabilities: a provider
//! that declares `Access::Read` is held to the positional-read cases and not
//! to the sequential ones. Bindings expose [`assert_conformance`] so a
//! host-language provider is held to exactly the same standard as a Rust one.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::{
    map_io_err, not_found, Access, Capabilities, DirEntry, Handle, Provider, RootId, Stat, VPath,
    KIND_DIR, KIND_FILE,
};

/// The reference tree every conformance-tested provider must expose.
pub const FIXTURE_FILES: &[(&str, &[u8])] = &[("a.txt", b"hello"), ("sub/b.txt", b"world!")];

/// Write the reference tree to a real directory, for disk-like providers.
///
/// Clears `dir` first. The removal is not swallowed: a caller that passes the
/// wrong path should hear about it rather than silently lose that tree.
pub fn write_fixture_tree(dir: &Path) {
    if dir.exists() {
        std::fs::remove_dir_all(dir).expect("clear the fixture tree");
    }
    std::fs::create_dir_all(dir.join("sub")).expect("create fixture tree");
    for (rel, body) in FIXTURE_FILES {
        let p = dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        std::fs::write(p, body).expect("write fixture file");
    }
}

/// In-memory reference provider, used to test the suite itself. Serves the
/// fixture tree under every root unless built with [`MemFixture::root_blind`].
pub struct MemFixture {
    files: HashMap<String, Vec<u8>>,
    /// When false, only `RootId(0)` resolves — the correct behavior here is
    /// "same tree under every root", so this models a root-blind bug.
    root_aware: bool,
    next: AtomicU64,
    opens: Mutex<HashMap<Handle, Vec<u8>>>,
}

impl MemFixture {
    pub fn new() -> Self {
        Self::build(None, true)
    }

    /// A fixture missing one path, to prove the suite detects a gap.
    pub fn missing(path: &str) -> Self {
        Self::build(Some(path.to_string()), true)
    }

    /// A fixture that serves content only under `RootId(0)`, to prove the
    /// suite detects a provider that ignores the root id.
    pub fn root_blind() -> Self {
        Self::build(None, false)
    }

    fn build(omit: Option<String>, root_aware: bool) -> Self {
        let mut files = HashMap::new();
        for (rel, body) in FIXTURE_FILES {
            if omit.as_deref() == Some(*rel) {
                continue;
            }
            files.insert((*rel).to_string(), body.to_vec());
        }
        MemFixture { files, root_aware, next: AtomicU64::new(1), opens: Mutex::new(HashMap::new()) }
    }

    fn visible(&self, p: VPath) -> bool {
        self.root_aware || p.root == RootId::DEFAULT
    }
}

impl Default for MemFixture {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for MemFixture {
    fn capabilities(&self) -> Capabilities {
        Capabilities::read_only()
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        if !self.visible(p) {
            return Ok(None);
        }
        if p.rel.is_empty() || p.rel == "sub" {
            return Ok(Some(Stat { kind: KIND_DIR, size: 0, mtime: 0 }));
        }
        Ok(self
            .files
            .get(p.rel)
            .map(|b| Stat { kind: KIND_FILE, size: b.len() as u64, mtime: 0 }))
    }

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        if !self.visible(p) {
            return Err(not_found());
        }
        let prefix = if p.rel.is_empty() { String::new() } else { format!("{}/", p.rel) };
        let mut seen: HashMap<String, DirEntry> = HashMap::new();
        for (rel, body) in &self.files {
            let Some(rest) = rel.strip_prefix(&prefix) else { continue };
            match rest.split_once('/') {
                Some((dir, _)) => {
                    seen.entry(dir.to_string()).or_insert(DirEntry {
                        name: dir.to_string(),
                        stat: Stat { kind: KIND_DIR, size: 0, mtime: 0 },
                    });
                }
                None => {
                    seen.insert(
                        rest.to_string(),
                        DirEntry {
                            name: rest.to_string(),
                            stat: Stat { kind: KIND_FILE, size: body.len() as u64, mtime: 0 },
                        },
                    );
                }
            }
        }
        if seen.is_empty() && !p.rel.is_empty() {
            return Err(not_found());
        }
        let mut out: Vec<DirEntry> = seen.into_values().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn open(&self, p: VPath, _flags: u32) -> Result<(Handle, u64, bool), i32> {
        if !self.visible(p) {
            return Err(not_found());
        }
        let body = self.files.get(p.rel).ok_or_else(not_found)?.clone();
        let size = body.len() as u64;
        let h = self.next.fetch_add(1, Ordering::Relaxed);
        self.opens.lock().map_err(|_| map_io_err())?.insert(h, body);
        Ok((h, size, false))
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        self.opens.lock().map_err(|_| map_io_err())?.remove(&h);
        Ok(())
    }

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let g = self.opens.lock().map_err(|_| map_io_err())?;
        let body = g.get(&h).ok_or_else(crate::bad_fh)?;
        let start = (offset as usize).min(body.len());
        let n = (body.len() - start).min(buf.len());
        buf[..n].copy_from_slice(&body[start..start + n]);
        Ok(n)
    }
}
```

- [ ] **Step 5: Implement the suite**

Append to `crates/vfs-provider/src/conformance.rs`, before the test module:

```rust
/// Read every byte of an open handle, looping over short reads.
fn read_all(p: &Arc<dyn Provider>, h: Handle, size: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(size as usize);
    let mut buf = [0u8; 3]; // deliberately small: forces the short-read loop
    let mut off = 0u64;
    loop {
        let n = p.read_at(h, off, &mut buf).expect("read_at");
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        off += n as u64;
        // Bound the loop: a provider that ignores `offset` and keeps returning
        // the same block would otherwise hang here instead of failing.
        assert!(
            out.len() <= size as usize,
            "read_at returned more than the file's {size} bytes — the provider is \
             probably ignoring the offset and re-serving the same block"
        );
    }
    out
}

/// Run the conformance cases implied by `p`'s declared capabilities.
///
/// Panics with a message naming the failing case. `p` must expose
/// [`FIXTURE_FILES`] under every root.
pub fn assert_conformance(p: Arc<dyn Provider>) {
    let caps = p.capabilities();
    caps.validate().expect("capabilities: self-contradictory declaration");

    assert_eq!(
        p.capabilities(),
        caps,
        "capabilities must be constant for the provider's lifetime"
    );

    assert_common(&p);
    match caps.access {
        Access::SeqRead => assert_sequential(&p),
        Access::Read | Access::ReadWrite => assert_positional(&p),
    }
}

fn assert_common(p: &Arc<dyn Provider>) {
    // Root of the provider is the empty string and is a directory.
    let root = p
        .getattr(VPath::at_default(""))
        .expect("getattr: provider root")
        .expect("getattr: provider root must exist");
    assert_eq!(root.kind, KIND_DIR, "the provider root must be a directory");

    // Every fixture file is visible with the right size.
    for (rel, body) in FIXTURE_FILES {
        let st = p
            .getattr(VPath::at_default(rel))
            .unwrap_or_else(|e| panic!("getattr({rel}) failed with status {e}"))
            .unwrap_or_else(|| panic!("getattr({rel}) reported the file missing"));
        assert_eq!(st.kind, KIND_FILE, "getattr({rel}) should report a file");
        assert_eq!(st.size, body.len() as u64, "getattr({rel}) size mismatch");
    }

    // An absent path is Ok(None), not an error.
    assert!(
        p.getattr(VPath::at_default("nope.txt")).expect("getattr: absent path must not error").is_none(),
        "getattr of an absent path must report None"
    );

    // readdir of the root lists both entries.
    let entries = p.readdir(VPath::at_default("")).expect("readdir: provider root");
    let mut names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["a.txt", "sub"], "readdir of the root listed {names:?}");

    // Entry metadata must be right, not just the names: a provider that lists
    // a.txt as a directory, or with size 0, passes a names-only check.
    for (rel, body) in FIXTURE_FILES {
        let Some(name) = rel.split('/').next_back() else { continue };
        if rel.contains('/') {
            continue; // only root-level entries are in this listing
        }
        let e = entries
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("{name} missing from the root listing"));
        assert_eq!(e.stat.kind, KIND_FILE, "readdir reported {name} as kind {}", e.stat.kind);
        assert_eq!(e.stat.size, body.len() as u64, "readdir reported {name} size {}", e.stat.size);
    }
    let sub_entry = entries.iter().find(|e| e.name == "sub").expect("sub in the root listing");
    assert_eq!(sub_entry.stat.kind, KIND_DIR, "readdir reported sub as a file");

    // getattr must agree with readdir about directory-ness.
    let sub_stat = p
        .getattr(VPath::at_default("sub"))
        .expect("getattr(sub)")
        .expect("sub must exist");
    assert_eq!(sub_stat.kind, KIND_DIR, "getattr reported sub as kind {}", sub_stat.kind);

    // readdir of a subdirectory.
    let sub = p.readdir(VPath::at_default("sub")).expect("readdir: sub");
    assert_eq!(sub.len(), 1, "readdir(sub) should list exactly one entry");
    assert_eq!(sub[0].name, "b.txt");

    // A non-default root must be handled coherently. Both answers are legal:
    // a provider over one backing store (a zip, a directory) correctly ignores
    // the root id and returns the same tree, while a multi-root provider may
    // report not-found for a root it does not serve. What is not legal is
    // panicking or returning an unrelated status.
    //
    // Root *distinguishing* is deliberately NOT asserted here — see the note
    // below the test module. It would be passed trivially by a root-ignoring
    // provider and would wrongly fail a correct multi-root one.
    match p.getattr(VPath::new(RootId(7), "a.txt")) {
        Ok(_) => {}
        Err(e) if e == crate::not_found() => {}
        Err(e) => panic!(
            "getattr under a non-default root returned status {e}; expected Ok or ST_NOT_FOUND"
        ),
    }

    // Opening an absent path fails with NOT_FOUND, not some other error and
    // not success. The old vfs-source suite asserted this; six ports depend
    // on it staying true.
    match p.open(VPath::at_default("nope.txt"), crate::OPEN_READ) {
        Err(e) if e == crate::not_found() => {}
        Err(e) => panic!("open of an absent path returned status {e}, expected ST_NOT_FOUND"),
        Ok((h, _, _)) => {
            let _ = p.close(h);
            panic!("open of an absent path succeeded; it must fail with ST_NOT_FOUND");
        }
    }

    // Handles are provider-scoped: two opens are independent.
    let (h1, _, _) = p.open(VPath::at_default("a.txt"), crate::OPEN_READ).expect("open #1");
    let (h2, _, _) = p.open(VPath::at_default("a.txt"), crate::OPEN_READ).expect("open #2");
    assert_ne!(h1, h2, "two concurrent opens must yield distinct handles");
    p.close(h1).expect("close #1");
    p.close(h2).expect("close #2");
}

fn assert_positional(p: &Arc<dyn Provider>) {
    for (rel, body) in FIXTURE_FILES {
        let (h, size, is_dir) = p.open(VPath::at_default(rel), crate::OPEN_READ).expect("open");
        assert!(!is_dir, "open({rel}) reported a directory");
        assert_eq!(size, body.len() as u64, "open({rel}) size mismatch");

        assert_eq!(read_all(p, h, size), *body, "read_at({rel}) content mismatch");

        // Reading at EOF yields zero, not an error.
        assert_eq!(
            p.read_at(h, size, &mut [0u8; 4]).expect("read_at at EOF must not error"),
            0,
            "read_at at EOF must return 0"
        );

        // Reading past EOF yields zero too.
        assert_eq!(
            p.read_at(h, size + 100, &mut [0u8; 4]).expect("read_at past EOF must not error"),
            0,
            "read_at past EOF must return 0"
        );

        // A zero-length buffer reads zero bytes.
        assert_eq!(
            p.read_at(h, 0, &mut []).expect("read_at with an empty buffer must not error"),
            0
        );

        // An unaligned mid-file read returns the right bytes. Assert n > 0
        // first — otherwise a provider returning 0 passes by comparing two
        // empty slices.
        if body.len() >= 3 {
            let mut buf = [0u8; 2];
            let n = p.read_at(h, 1, &mut buf).expect("unaligned read_at");
            assert!(n > 0, "unaligned read_at({rel}) returned 0 bytes mid-file");
            assert_eq!(&buf[..n], &body[1..1 + n], "unaligned read_at({rel}) content mismatch");
        }

        p.close(h).expect("close");
    }

    // A closed handle is no longer valid.
    let (h, _, _) = p.open(VPath::at_default("a.txt"), crate::OPEN_READ).expect("open");
    p.close(h).expect("close");
    assert!(
        p.read_at(h, 0, &mut [0u8; 4]).is_err(),
        "read_at on a closed handle must fail"
    );
}

fn assert_sequential(p: &Arc<dyn Provider>) {
    for (rel, body) in FIXTURE_FILES {
        // A sequential provider must refuse positional reads rather than
        // silently returning something plausible.
        let (probe, _, _) = p.open(VPath::at_default(rel), crate::OPEN_READ).expect("open");
        match p.read_at(probe, 0, &mut [0u8; 4]) {
            Err(e) if e == crate::not_supported() => {}
            Err(e) => panic!(
                "read_at on a SeqRead provider returned status {e}, expected ST_NOT_SUPPORTED"
            ),
            Ok(n) => panic!(
                "read_at on a SeqRead provider succeeded with {n} bytes; it must be refused"
            ),
        }
        p.close(probe).expect("close");

        let (h, _, _) = p.open(VPath::at_default(rel), crate::OPEN_READ).expect("open");
        let mut out = Vec::new();
        let mut buf = [0u8; 3];
        loop {
            let n = p.read_next(h, &mut buf).expect("read_next");
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, *body, "read_next({rel}) content mismatch");
        p.close(h).expect("close");

        // Reopening resets the cursor. Assert the byte count, not just the
        // slice: a shared cursor parked at EOF returns n=0, and comparing two
        // empty slices would pass.
        let (h2, _, _) = p.open(VPath::at_default(rel), crate::OPEN_READ).expect("reopen");
        let mut first = [0u8; 1];
        let n = p.read_next(h2, &mut first).expect("read_next after reopen");
        assert_eq!(n, 1, "reopen did not reset the cursor — read_next returned {n} bytes");
        assert_eq!(&first[..1], &body[..1], "reopen returned the wrong first byte");
        p.close(h2).expect("close");
    }
}
```

- [ ] **Step 6: Export it**

Add to `crates/vfs-provider/src/lib.rs`:

```rust
pub mod conformance;
```

and to the re-export list:

```rust
pub use conformance::{assert_conformance, write_fixture_tree, FIXTURE_FILES};
```

- [ ] **Step 7: Run to verify it passes**

Run: `cargo test -p vfs-provider`
Expected: 17 passed. In particular `a_provider_that_ignores_the_root_id_fails_the_suite` must pass — proving the suite actually catches root blindness rather than merely mentioning it.

Run: `cargo clippy -p vfs-provider --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add rust/crates/vfs-provider
git commit -m "test(provider): capability-parameterized conformance suite

Cases are selected by declared capabilities, so one suite covers
sequential and positional providers. Includes negative fixtures proving
the suite detects a missing file and a provider that ignores the root id."
```

---

### Task 5: Port `vfs-protocol` and `vfs-zip`

**Files:**
- Modify: `crates/vfs-protocol/Cargo.toml`, `crates/vfs-protocol/src/lib.rs`, `crates/vfs-protocol/src/ops.rs`
- Modify: `crates/vfs-zip/Cargo.toml`, `crates/vfs-zip/src/backend.rs`, `crates/vfs-zip/src/lib.rs`

**Interfaces:**
- Consumes: `Provider`, `VPath`, `Capabilities`, `Access`, status codes from Tasks 2-4.
- Produces: `vfs_zip::ZipProvider` (renamed from `ZipBackend`), declaring `Capabilities { access: Access::Read, immutable: true, slow: false, preferred_block: None }`. `vfs-protocol` no longer defines `Backend`, `Stat`, `DirEntry`, or the `ST_*`/`OPEN_*` constants — it re-exports them from `vfs-provider`.

**Why these two together:** `vfs-zip` is the smallest real provider and `vfs-protocol` is its dependency. Porting them as a pair proves the contract works before touching the larger crates.

- [ ] **Step 1: Make `vfs-protocol` depend on `vfs-provider` and delete the duplicate types**

In `crates/vfs-protocol/Cargo.toml`, add under `[dependencies]`:

```toml
vfs-provider = { path = "../vfs-provider" }
```

Replace the entire contents of `crates/vfs-protocol/src/ops.rs`:

```rust
//! Re-export of the provider contract, kept as a path for existing importers.
//!
//! The types live in `vfs-provider`; `vfs-protocol` owns only the ring wire
//! codecs and opcode catalog.

pub use vfs_provider::{
    bad_fh, bad_request, is_dir, map_io_err, not_a_dir, not_found, not_supported, ok, read_only,
    Access, Capabilities, DirEntry, Handle, Provider, RootId, SetAttr, Stat, VPath, KIND_DIR,
    KIND_FILE, KIND_TOMBSTONE,
};
```

In `crates/vfs-protocol/src/lib.rs`, delete the `ST_*` constant block and the `OPEN_READ` / `OPEN_WRITE` definitions (they now live in `vfs-provider`), and replace the top re-export block with:

```rust
pub use ops::{
    bad_fh, bad_request, is_dir, map_io_err, not_a_dir, not_found, not_supported, ok, read_only,
    Access, Capabilities, DirEntry, Handle, Provider, RootId, SetAttr, Stat, VPath, KIND_DIR,
    KIND_FILE, KIND_TOMBSTONE,
};
pub use vfs_provider::{
    OPEN_APPEND, OPEN_CREATE, OPEN_EXCL, OPEN_READ, OPEN_TRUNC, OPEN_WRITE, ST_BAD_FH,
    ST_BAD_REQUEST, ST_IO_ERROR, ST_IS_DIR, ST_NOT_A_DIRECTORY, ST_NOT_FOUND, ST_NOT_SUPPORTED,
    ST_NO_SPACE, ST_OK, ST_READ_ONLY,
};
```

Keep `BackendHandle` alive for one task as a deprecated alias so the port can proceed crate by crate:

```rust
/// Deprecated: use [`Handle`]. Removed at the end of Stage 1.
pub type BackendHandle = Handle;
```

Leave the opcode constants, `AttrResp`, `DirEntryWire`, `OpenResp`, `ReadReq`, and every `encode_*` / `decode_*` function exactly as they are. They are wire concerns and this task does not touch the wire.

- [ ] **Step 2: Verify the workspace still compiles**

Run: `cargo build --all-targets`
Expected: success. Nothing has changed semantically — `vfs_protocol::Backend` is now a re-export of `vfs_provider::Provider`, but no implementor has been ported, so **this will fail** with trait-signature errors in every implementor. That is expected and is the failing state Steps 3-6 fix for `vfs-zip`.

Record the error count for reference: `cargo build --all-targets 2>&1 | Select-String "^error" | Measure-Object -Line`

- [ ] **Step 3: Write the failing test for `vfs-zip`**

Add to the bottom of `crates/vfs-zip/src/backend.rs`, inside the existing `#[cfg(test)] mod tests` block (or create one if absent):

```rust
    #[test]
    fn zip_provider_declares_immutable_read_access() {
        use vfs_provider::{Access, Provider};
        let dir = std::env::temp_dir().join(format!("vfs-zipcaps-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let zip = dir.join("t.zip");
        write_conformance_zip(&zip);

        let p = ZipProvider::open(&zip).expect("open zip");
        let caps = p.capabilities();
        assert_eq!(caps.access, Access::Read);
        assert!(caps.immutable, "a zip container never changes under us");
        assert!(!caps.slow);
        caps.validate().expect("declaration must be self-consistent");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zip_provider_passes_conformance() {
        let dir = std::env::temp_dir().join(format!("vfs-zipconf-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let zip = dir.join("t.zip");
        write_conformance_zip(&zip);

        let p: std::sync::Arc<dyn vfs_provider::Provider> =
            std::sync::Arc::new(ZipProvider::open(&zip).expect("open zip"));
        vfs_provider::assert_conformance(p);

        let _ = std::fs::remove_dir_all(&dir);
    }
```

You also need `write_conformance_zip`, a helper that builds a Stored zip containing `a.txt` = `hello` and `sub/b.txt` = `world!`. The existing test at `crates/vfs-source/src/lib.rs:126-180` builds a single-entry Stored zip by hand; generalize that byte-assembly code into a helper taking `&[(&str, &[u8])]` and place it in `crates/vfs-zip/src/backend.rs` under `#[cfg(test)]`. Do not add a zip-writing dependency.

- [ ] **Step 4: Run to verify it fails**

Run: `cargo test -p vfs-zip`
Expected: compile error — `ZipProvider` is not defined and `ZipBackend` does not implement `Provider`.

- [ ] **Step 5: Port `vfs-zip`**

In `crates/vfs-zip/Cargo.toml`, add `vfs-provider = { path = "../vfs-provider" }`.

In `crates/vfs-zip/src/backend.rs`:

1. Rename the type `ZipBackend` → `ZipProvider` throughout the file.
2. Change the import line to pull from `vfs_provider`:

```rust
use vfs_provider::{
    Access, Capabilities, DirEntry, Handle, Provider, Stat, VPath, KIND_DIR, KIND_FILE, OPEN_WRITE,
};
use vfs_provider::{ST_BAD_FH, ST_BAD_REQUEST, ST_IO_ERROR, ST_NOT_A_DIRECTORY, ST_NOT_FOUND};
```

3. Change `impl Backend for ZipBackend` to `impl Provider for ZipProvider` and add the capabilities method as its first member:

```rust
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            access: Access::Read,
            immutable: true,
            slow: false,
            preferred_block: None,
        }
    }
```

4. Change each path-taking method's signature from `path: &str` to `p: VPath` and, as the first line of each body, bind `let path = p.rel;`. The zip provider serves the same archive under every root, so it ignores `p.root` — which is correct and is what the conformance root-scoping case checks.

5. Rename `BackendHandle` → `Handle` and `bh:` parameters → `h:` in `read`/`release`.

6. Rename the method `read` → `read_at` and `release` → `close` to match the trait.

In `crates/vfs-zip/src/lib.rs`, update the `pub use` to export `ZipProvider`. Add a deprecated alias so the rest of the tree keeps compiling until Task 9:

```rust
pub use backend::ZipProvider;
/// Deprecated: renamed to [`ZipProvider`]. Removed at the end of Stage 1.
pub use backend::ZipProvider as ZipBackend;
```

- [ ] **Step 6: Run to verify it passes**

Run: `cargo test -p vfs-zip`
Expected: all `vfs-zip` tests pass, including the two new ones.

Run: `cargo clippy -p vfs-zip -p vfs-provider -p vfs-protocol --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add rust/crates/vfs-protocol rust/crates/vfs-zip
git commit -m "refactor(zip): port to the Provider contract

vfs-protocol keeps only wire concerns and re-exports the contract from
vfs-provider. ZipProvider declares immutable read access and passes
conformance."
```

**Note for the next task:** the workspace does not build as a whole between Tasks 5 and 9 — each unported implementor is a compile error. Run per-crate `cargo test -p <crate>` until Task 9 restores a whole-workspace build. This is the one place in Stage 1 where a task does not leave the tree fully green, and it is why Tasks 5-9 should land in one sitting.

---

### Task 6: Port `vfs-director` (disk provider and kernel)

**Files:**
- Modify: `crates/vfs-director/Cargo.toml`, `crates/vfs-director/src/ops.rs`, `crates/vfs-director/src/disk.rs`, `crates/vfs-director/src/director.rs`, `crates/vfs-director/src/session.rs`, `crates/vfs-director/src/lib.rs`, `crates/vfs-director/tests/zip_serve_integrity.rs`

**Interfaces:**
- Consumes: `Provider`, `VPath`, `Capabilities` (Tasks 2-4); `ZipProvider` (Task 5).
- Produces: `vfs_director::DiskProvider` (renamed from `DiskBackend`) declaring `Capabilities { access: Access::Read, immutable: false, slow: false, preferred_block: None }` — **still read-only in Stage 1**; write access arrives in Stage 3. `Director::mount(prefix, Arc<dyn Provider>)` unchanged in shape.

**Scope guard:** do **not** remove the layer-ordered mount merge and do **not** implement writes. Those are Stage 2 and Stage 3. This task is a rename and re-signature only.

- [ ] **Step 1: Write the failing test**

Add to `crates/vfs-director/src/disk.rs`, in its `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn disk_provider_declares_mutable_read_access() {
        use vfs_provider::{Access, Provider};
        let dir = std::env::temp_dir().join(format!("vfs-diskcaps-{}", std::process::id()));
        vfs_provider::write_fixture_tree(&dir);

        let p = DiskProvider::new(&dir);
        let caps = p.capabilities();
        assert_eq!(caps.access, Access::Read, "writes arrive in Stage 3");
        assert!(!caps.immutable, "a real directory can change underneath us");
        caps.validate().expect("declaration must be self-consistent");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_provider_passes_conformance() {
        let dir = std::env::temp_dir().join(format!("vfs-diskconf-{}", std::process::id()));
        vfs_provider::write_fixture_tree(&dir);

        let p: std::sync::Arc<dyn vfs_provider::Provider> = std::sync::Arc::new(DiskProvider::new(&dir));
        vfs_provider::assert_conformance(p);

        let _ = std::fs::remove_dir_all(&dir);
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p vfs-director --lib disk`
Expected: compile error — `DiskProvider` is not defined.

- [ ] **Step 3: Port `disk.rs`**

Add `vfs-provider = { path = "../vfs-provider" }` to `crates/vfs-director/Cargo.toml`.

Rename `DiskBackend` → `DiskProvider`. Change `impl Backend` → `impl Provider`, convert `path: &str` parameters to `p: VPath` with `let path = p.rel;` as the first line of each body, and rename `read` → `read_at`, `release` → `close`. Add as the first member of the impl:

```rust
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            access: Access::Read, // writes arrive in Stage 3
            immutable: false,     // a real directory can change underneath us
            slow: false,
            preferred_block: None,
        }
    }
```

- [ ] **Step 4: Port `ops.rs`**

Replace `crates/vfs-director/src/ops.rs`:

```rust
//! Re-export the provider contract from `vfs-protocol` for a single import path.

pub use vfs_protocol::{
    bad_fh, bad_request, is_dir, map_io_err, not_a_dir, not_found, not_supported, ok, read_only,
    Access, Capabilities, DirEntry, Handle, Provider, RootId, SetAttr, Stat, VPath, KIND_DIR,
    KIND_FILE, KIND_TOMBSTONE, OPEN_READ, OPEN_WRITE, ST_BAD_FH, ST_BAD_REQUEST, ST_IO_ERROR,
    ST_IS_DIR, ST_NOT_A_DIRECTORY, ST_NOT_FOUND, ST_NOT_SUPPORTED, ST_OK, ST_READ_ONLY,
};
```

- [ ] **Step 5: Port `director.rs`**

Change `Mount.backend: Arc<dyn Backend>` → `Arc<dyn Provider>` and `OpenRec.backend` likewise. In each of `getattr`, `readdir`, `open`, keep the existing mount-iteration logic verbatim, but change the child call to build a `VPath`:

```rust
            match m.backend.getattr(VPath::at_default(&rel))? {
```

Do the same for `readdir` and `open`. In `read`, rename the call `backend.read(bh, offset, buf)` → `backend.read_at(bh, offset, buf)`. In `close`, `rec.backend.release(rec.bh)` → `rec.backend.close(rec.bh)`.

Every call site uses `VPath::at_default` because roots are not yet real. Stage 2 replaces these.

- [ ] **Step 6: Port `session.rs` and `lib.rs`**

`session.rs` references `Backend` in its `mount` signature and imports; change to `Provider`. `lib.rs` re-exports `DiskBackend`; change to `DiskProvider` and add a deprecated alias:

```rust
pub use disk::DiskProvider;
/// Deprecated: renamed to [`DiskProvider`]. Removed at the end of Stage 1.
pub use disk::DiskProvider as DiskBackend;
```

- [ ] **Step 7: Port the integration test**

`crates/vfs-director/tests/zip_serve_integrity.rs` has 16 `Backend` references. Update the imports to `vfs_provider::{Provider, VPath}`, change `ZipBackend` → `ZipProvider`, wrap bare path arguments in `VPath::at_default(...)`, and rename `.read(` → `.read_at(` and `.release(` → `.close(`. **Do not change any assertion.**

- [ ] **Step 8: Run to verify it passes**

Run: `cargo test -p vfs-director`
Expected: all tests pass, including the two new disk tests. If any *assertion* had to change to make this pass, stop — semantics moved and this task is meant to be behavior-preserving.

Run: `cargo clippy -p vfs-director --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 9: Commit**

```bash
git add rust/crates/vfs-director
git commit -m "refactor(director): port disk provider and kernel to the Provider contract

Rename and re-signature only: mount merging and the OPEN_WRITE rejection
are untouched, and every call site addresses RootId::DEFAULT until
Stage 2 makes roots real."
```

---

### Task 7: Port `vfs-compose` combinators

**Files:**
- Modify: `crates/vfs-compose/Cargo.toml`, and all of `crates/vfs-compose/src/{lib,inline,layered,overlay,router,strip_prefix}.rs`

**Interfaces:**
- Consumes: `Provider`, `VPath`, `Capabilities::{weakest, read_only_clamp}` (Tasks 2-4).
- Produces:
  - `InlineProvider` (was `InlineBackend`) — `Access::Read`, `immutable: true`.
  - `LayeredProvider` (was `LayeredBackend`) — capabilities = `Capabilities::weakest([upper, base])`.
  - `OverlayProvider` (was `OverlayBackend`) — `Access::Read` in Stage 1 (it still rejects `OPEN_WRITE`); Stage 3 promotes it.
  - `RouterProvider` (was `RouterBackend`) — capabilities = `weakest` over default plus every route.
  - `SubdirProvider` (was `StripPrefixBackend`) — capabilities pass through from the child.
  - `stack_layers(Vec<Arc<dyn Provider>>) -> Result<Arc<dyn Provider>, &'static str>` unchanged in shape.

**Scope guard:** `RouterProvider` keeps its current single-dispatch `readdir`. The dispatch-vs-union asymmetry from spec §6 is a **behavior change** and belongs to Stage 2. Do not implement it here.

- [ ] **Step 1: Write the failing tests**

Add to `crates/vfs-compose/src/lib.rs` in the existing `#[cfg(test)] mod stack_tests`:

```rust
    #[test]
    fn a_layered_stack_reports_the_weakest_access_of_its_children() {
        use vfs_provider::{Access, Provider};
        let bottom = Arc::new(InlineProvider::from_files([("f", b"0".as_slice())]));
        let top = Arc::new(InlineProvider::from_files([("f", b"1".as_slice())]));
        let stacked = stack_layers(vec![bottom, top]).unwrap();
        assert_eq!(stacked.capabilities().access, Access::Read);
    }

    #[test]
    fn a_layered_stack_of_immutable_children_is_immutable() {
        use vfs_provider::Provider;
        let bottom = Arc::new(InlineProvider::from_files([("f", b"0".as_slice())]));
        let top = Arc::new(InlineProvider::from_files([("f", b"1".as_slice())]));
        let stacked = stack_layers(vec![bottom, top]).unwrap();
        assert!(stacked.capabilities().immutable, "inline content never changes");
    }

    #[test]
    fn inline_provider_passes_conformance() {
        let p: Arc<dyn vfs_provider::Provider> = Arc::new(InlineProvider::from_files(
            vfs_provider::FIXTURE_FILES.iter().copied(),
        ));
        vfs_provider::assert_conformance(p);
    }

    #[test]
    fn a_layered_stack_passes_conformance() {
        // Bottom holds the full fixture tree, top holds nothing: the stack
        // must still present the reference tree.
        let bottom: Arc<dyn vfs_provider::Provider> = Arc::new(InlineProvider::from_files(
            vfs_provider::FIXTURE_FILES.iter().copied(),
        ));
        let top: Arc<dyn vfs_provider::Provider> =
            Arc::new(InlineProvider::from_files(std::iter::empty::<(&str, &[u8])>()));
        vfs_provider::assert_conformance(stack_layers(vec![bottom, top]).unwrap());
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p vfs-compose`
Expected: compile error — `InlineProvider` is not defined.

- [ ] **Step 3: Port the five combinators**

Add `vfs-provider = { path = "../vfs-provider" }` to `crates/vfs-compose/Cargo.toml`.

For **each** of `inline.rs`, `layered.rs`, `overlay.rs`, `router.rs`, `strip_prefix.rs`:

1. Change imports from `vfs_protocol::{Backend, BackendHandle, ...}` to `vfs_provider::{Provider, Handle, VPath, Capabilities, ...}`.
2. Rename the type per the Interfaces list above (`strip_prefix.rs`'s `StripPrefixBackend` becomes `SubdirProvider`; rename the file to `subdir.rs` and update the `mod` line).
3. Change `impl Backend for X` → `impl Provider for X`.
4. Change every `Arc<dyn Backend>` field and parameter to `Arc<dyn Provider>`.
5. Convert `path: &str` parameters to `p: VPath`. Combinators are **pass-through**: forward `p` unchanged to children. `SubdirProvider` is the one exception — it rewrites, so it builds a new `VPath { root: p.root, rel: &joined }`.
6. Rename `read` → `read_at`, `release` → `close`, `BackendHandle` → `Handle`.
7. Add a `capabilities` method:

```rust
// inline.rs
    fn capabilities(&self) -> Capabilities {
        Capabilities { immutable: true, ..Capabilities::read_only() }
    }

// layered.rs
    fn capabilities(&self) -> Capabilities {
        Capabilities::weakest([self.upper.capabilities(), self.base.capabilities()])
    }

// overlay.rs — Stage 1 keeps this read-only; Stage 3 promotes it to ReadWrite.
    fn capabilities(&self) -> Capabilities {
        Capabilities::read_only()
    }

// router.rs
    fn capabilities(&self) -> Capabilities {
        Capabilities::weakest(
            std::iter::once(self.default.capabilities())
                .chain(self.routes.iter().map(|r| r.provider.capabilities())),
        )
    }

// subdir.rs
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
```

Note `router.rs`'s `Route` field `backend` renames to `provider`.

In `lib.rs`, update the `mod` and `pub use` lines to the new names, and keep deprecated aliases for the old ones until Task 9.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p vfs-compose`
Expected: all existing tests pass unchanged plus the four new ones.

Run: `cargo clippy -p vfs-compose --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add rust/crates/vfs-compose
git commit -m "refactor(compose): port combinators to the Provider contract

Each combinator now derives its own capabilities from its children.
Router keeps single-dispatch readdir — the union asymmetry is a behavior
change and belongs to Stage 2."
```

---

### Task 8: Port `vfs-cache`

**Files:**
- Modify: `crates/vfs-cache/Cargo.toml`, `crates/vfs-cache/src/backend.rs` (rename to `provider.rs`), `crates/vfs-cache/src/lib.rs`

**Interfaces:**
- Consumes: `Provider`, `VPath`, `Capabilities::cached()` (Tasks 2-4).
- Produces: `CachingProvider` (was `CachingBackend`) with `CachingProvider::new(inner, cache, source_id)` unchanged, declaring `inner.capabilities().cached()`.

**Scope guard:** cache keys do **not** gain the root id here — that is Stage 2, and it is a behavior change. `file_id_for` keeps its current inputs.

- [ ] **Step 1: Write the failing test**

Add to `crates/vfs-cache/src/provider.rs` (after the rename) in a `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn caching_answers_the_slow_marker() {
        use std::sync::Arc;
        use vfs_provider::{Access, Capabilities, DirEntry, Handle, Provider, Stat, VPath};

        struct SlowInner;
        impl Provider for SlowInner {
            fn capabilities(&self) -> Capabilities {
                Capabilities { slow: true, preferred_block: Some(1 << 20), ..Capabilities::read_only() }
            }
            fn getattr(&self, _p: VPath) -> Result<Option<Stat>, i32> { Ok(None) }
            fn readdir(&self, _p: VPath) -> Result<Vec<DirEntry>, i32> { Ok(Vec::new()) }
            fn open(&self, _p: VPath, _f: u32) -> Result<(Handle, u64, bool), i32> {
                Err(vfs_provider::not_found())
            }
            fn close(&self, _h: Handle) -> Result<(), i32> { Ok(()) }
            fn read_at(&self, _h: Handle, _o: u64, _b: &mut [u8]) -> Result<usize, i32> { Ok(0) }
        }

        let cache = Arc::new(crate::BlockCache::new(crate::CacheConfig::default()));
        let p = CachingProvider::new(Arc::new(SlowInner), cache, 1);
        let caps = p.capabilities();
        assert!(!caps.slow, "a cached provider is no longer slow");
        assert_eq!(caps.access, Access::Read, "access passes through");
        assert_eq!(caps.preferred_block, Some(1 << 20), "the block hint survives");
    }
```

`BlockCache::new(CacheConfig)` is the real constructor (`crates/vfs-cache/src/store.rs:72`) and `CacheConfig` derives `Default`. Both are already re-exported from `crates/vfs-cache/src/lib.rs`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p vfs-cache`
Expected: compile error — `CachingProvider` is not defined.

- [ ] **Step 3: Port it**

Add `vfs-provider = { path = "../vfs-provider" }` to `crates/vfs-cache/Cargo.toml`. Rename the file `src/backend.rs` → `src/provider.rs` and update `lib.rs`'s `mod` line. Apply the same mechanical port as Task 7 (imports, `impl Provider`, `VPath` parameters, `read_at`/`close` renames, `Handle`), and add:

```rust
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities().cached()
    }
```

Keep the `OPEN_WRITE` rejection in `open` exactly as it is — Stage 3 removes it.

Keep a deprecated alias in `lib.rs`:

```rust
pub use provider::CachingProvider;
/// Deprecated: renamed to [`CachingProvider`]. Removed at the end of Stage 1.
pub use provider::CachingProvider as CachingBackend;
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p vfs-cache`
Expected: all tests pass including the new one.

Run: `cargo clippy -p vfs-cache --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add rust/crates/vfs-cache
git commit -m "refactor(cache): port to the Provider contract

CachingProvider declares inner.capabilities().cached(), so wrapping a
slow provider answers the marker. Cache keying is unchanged; root
scoping arrives in Stage 2."
```

---

### Task 9: Port `vfs-source` and `vfs-directord`, then drop the deprecated aliases

**Files:**
- Modify: `crates/vfs-source/Cargo.toml`, `crates/vfs-source/src/{lib,remote,serve,rt}.rs`, `crates/vfs-source/src/bin/vfs-source-plugin.rs`, `crates/vfs-source/proto/source.proto`
- Delete: `crates/vfs-source/src/conformance.rs`
- Modify: `crates/vfs-directord/src/registry.rs`, `crates/vfs-directord/src/bin/skyrim-live.rs`, `crates/vfs-launch/src/main.rs`
- Modify: every crate's `lib.rs` that carries a deprecated alias (Tasks 5-8)

**Interfaces:**
- Consumes: everything from Tasks 2-8.
- Produces: `RemoteProvider` (was `RemoteBackend`), `ProviderSourceService` (was `BackendSourceService`), `build_provider` (was `build_backend`). `vfs_source::assert_conformance` and `write_fixture_tree` are gone — importers use `vfs_provider::` versions.

**This task restores a green whole-workspace build.**

- [ ] **Step 1: Extend the proto with capabilities**

In `crates/vfs-source/proto/source.proto`, add a version field and a capabilities RPC. Do not add write RPCs — those are Stage 3.

```proto
service Source {
  rpc GetCapabilities(Empty) returns (CapsResp);
  rpc GetAttr(GetAttrReq) returns (GetAttrResp);
  rpc ReadDir(ReadDirReq) returns (ReadDirResp);
  rpc Open(OpenReq) returns (OpenResp);
  rpc Read(ReadReq) returns (ReadResp);
  rpc Release(ReleaseReq) returns (Empty);
}

// 1 = Stage 1 read-only contract. Bumped when the wire shape changes.
message CapsResp {
  uint32 contract_version = 1;
  uint32 access = 2;          // 0 = SeqRead, 1 = Read, 2 = ReadWrite
  bool   immutable = 3;
  bool   slow = 4;
  uint32 preferred_block = 5; // 0 = unset
}
```

Add `root` to every path-carrying request so the wire matches the contract:

```proto
message GetAttrReq { string path = 1; uint32 root = 2; }
message ReadDirReq { string path = 1; uint32 root = 2; }
message OpenReq    { string path = 1; uint32 flags = 2; uint32 root = 3; }
```

- [ ] **Step 2: Write the failing test**

In `crates/vfs-source/src/lib.rs`, replace the existing `disk_conformance` and `remote_backend_conformance` tests with:

```rust
    #[test]
    fn disk_conformance() {
        let dir = std::env::temp_dir().join(format!("vfs-conf-{}", std::process::id()));
        vfs_provider::write_fixture_tree(&dir);
        let p: Arc<dyn Provider> = Arc::new(DiskProvider::new(&dir));
        vfs_provider::assert_conformance(p);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_provider_conformance_and_capabilities() {
        let dir = std::env::temp_dir().join(format!("vfs-rconf-{}", std::process::id()));
        vfs_provider::write_fixture_tree(&dir);
        let p: Arc<dyn Provider> = Arc::new(DiskProvider::new(&dir));
        let svc = ProviderSourceService::new(p);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        let server = tokio::spawn(async move {
            Server::builder()
                .add_service(crate::pb::source_server::SourceServer::new(svc))
                .serve_with_incoming(incoming)
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let remote = RemoteProvider::connect(&format!("{addr}")).await.unwrap();
        let remote: Arc<dyn Provider> = Arc::new(remote);

        // Capabilities must survive the round trip, not be defaulted locally.
        assert_eq!(remote.capabilities().access, vfs_provider::Access::Read);
        assert!(!remote.capabilities().immutable, "disk is mutable, and the wire must say so");

        tokio::task::spawn_blocking(move || {
            vfs_provider::assert_conformance(remote);
        })
        .await
        .unwrap();

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test -p vfs-source`
Expected: compile error — `RemoteProvider`, `ProviderSourceService`, and `vfs_provider::write_fixture_tree` in this context are not yet wired.

- [ ] **Step 4: Port `vfs-source`**

Add `vfs-provider = { path = "../vfs-provider" }` to the manifest.

Delete `crates/vfs-source/src/conformance.rs` and its `mod conformance;` line; the suite now lives in `vfs-provider`. Update `lib.rs`'s re-export to `pub use vfs_provider::{assert_conformance, write_fixture_tree};` so existing importers keep working.

In `remote.rs`: rename `RemoteBackend` → `RemoteProvider`, `impl Backend` → `impl Provider`, convert to `VPath` (send `p.root.0` in the new `root` field), rename `read`/`release` → `read_at`/`close`. Fetch capabilities once during `connect` / `connect_blocking`, store them in a field, and return the stored value from `capabilities()` — the contract requires capabilities to be constant, and re-fetching per call would be a round trip on a hot path. Map `access`: `0` → `SeqRead`, `1` → `Read`, `2` → `ReadWrite`, anything else → error out of `connect` with a clear message naming the contract version.

In `serve.rs`: rename `BackendSourceService` → `ProviderSourceService`, implement `get_capabilities` from the wrapped provider, and read `root` from each request into `VPath::new(RootId(req.root), &req.path)`.

In `lib.rs`: rename `build_backend` → `build_provider`, `SourceSpec` → keep for now (the `ProviderSpec` rename and the registry are Stage 2), and update the match arms to the new type names.

In `src/bin/vfs-source-plugin.rs`: update type names.

- [ ] **Step 5: Port the remaining consumers**

`crates/vfs-directord/src/registry.rs`, `crates/vfs-directord/src/bin/skyrim-live.rs`, and `crates/vfs-launch/src/main.rs` reference `Backend` and the old type names. Update imports and names. No logic changes.

- [ ] **Step 6: Delete every deprecated alias**

Remove the `BackendHandle`, `ZipBackend`, `DiskBackend`, `CachingBackend`, `InlineBackend`, `LayeredBackend`, `OverlayBackend`, `RouterBackend`, and `StripPrefixBackend` aliases added in Tasks 5-8, then fix the resulting compile errors. When this step is done, `grep -rn "Backend" --include=*.rs crates/` must return **zero** hits outside comments describing history.

- [ ] **Step 7: Verify the whole workspace**

Run: `cargo build --all-targets`
Expected: success — first green whole-workspace build since Task 5.

Run: `cargo test --workspace`
Expected: the same set of passes as before Task 5, plus the new conformance and capability tests. No assertion in a pre-existing test should have changed.

Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean.

Run: `grep -rn "Backend" --include=*.rs crates/`
Expected: no hits except historical comments.

- [ ] **Step 8: Commit**

```bash
git add rust/crates
git commit -m "refactor: finish the Provider port and drop the Backend name

vfs-source carries capabilities and the root id on the wire; the
conformance suite moves to vfs-provider so one definition serves every
implementation. Deprecated aliases removed — the tree says Provider."
```

---

### Task 10: Documentation and the stage gate

**Files:**
- Modify: `README.md`, `rust/docs/architecture.md`
- Create: `rust/crates/vfs-provider/README.md`

**Interfaces:**
- Consumes: everything.
- Produces: no code.

- [ ] **Step 1: Write the crate README**

Create `rust/crates/vfs-provider/README.md` covering: what a provider is, the four capability dimensions and what each obliges, the `(RootId, relative path)` addressing rule, the five-method floor for a read-only provider, and how to run `assert_conformance` against your own type. Include a complete, compiling example of a minimal provider — copy the `Minimal` struct from `provider.rs`'s tests so it cannot drift.

- [ ] **Step 2: Update the architecture doc**

In `rust/docs/architecture.md`, replace `Backend` with `Provider` throughout and add a short section describing the capability model and why `immutable` and `slow` are separate flags. Link to the design spec.

- [ ] **Step 3: Update the README crate table**

In `README.md`, add `vfs-provider` to the architecture table with the role "Provider contract, capabilities, conformance suite", and change the `vfs-source` row to "Provider builders, gRPC SourceService".

- [ ] **Step 4: Run the full gate**

All four must be green before Stage 2 begins:

```powershell
cargo build --all-targets
cargo build --manifest-path crates/vfs-payload/Cargo.toml
cargo test --workspace
cargo clippy --all-targets -- -D warnings
```

- [ ] **Step 5: Commit**

```bash
git add README.md rust/docs/architecture.md rust/crates/vfs-provider/README.md
git commit -m "docs: describe the provider contract and capability model"
```

---

## Stage 1 Exit Criteria

- [ ] `vfs-payload` builds in its own workspace; the main workspace unwinds, proven by `crates/vfs-protocol/tests/unwind.rs`.
- [ ] `vfs-provider` exists and owns `Capabilities`, `VPath`, `Provider`, and the conformance suite.
- [ ] `ZipProvider`, `DiskProvider`, `InlineProvider`, `LayeredProvider`, `RemoteProvider` each pass `assert_conformance`.
- [ ] The identifier `Backend` appears nowhere in `crates/**/*.rs` outside historical comments.
- [ ] `cargo test --workspace` passes with no pre-existing assertion modified.
- [ ] `cargo clippy --all-targets -- -D warnings` is clean.

**Not in this stage, by design:** real roots (Stage 2), the router readdir union (Stage 2), root-scoped cache keys (Stage 2), any write path (Stage 3), `vfs-embed` (Stage 4), the Python binding (Stage 4).

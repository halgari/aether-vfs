# Stage 2a-ii Gate 2: Canonicalise and Close the Escapes — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make every spelling of a path under a managed root resolve to one canonical form, so no path can reach the real filesystem merely by being written differently.

**Architecture:** A canonicaliser in `vfs-redirect` resolves alternate NT spellings — device paths, volume GUIDs, 8.3 names, streams, handle-relative opens — to one root-relative form, cached per raw input. `RootMap::under_root` consults it. A fixture executable then attempts a known file via all fourteen spellings and reports a matrix.

**Tech Stack:** Rust 2021, Windows NT API, PowerShell harness.

## Global Constraints

- Design spec: `docs/superpowers/specs/2026-08-13-no-bypass-and-real-roots-design.md` §5 and §7.
- Working directory for all cargo commands is `C:\oss\aether-vfs\rust`.
- Baseline: `cargo test --workspace` = 407 passed, 0 failed, 1 ignored. Never lower it.
- Zero clippy warnings: `cargo clippy --all-targets -- -D warnings`.
- `vfs-payload` is a separate workspace: `cargo build --manifest-path crates/vfs-payload/Cargo.toml --target-dir target`.
- Conventional commit prefixes. Commit after every task.

### What this gate does NOT do — read this before writing any assertion

**The fall-through stays.** Gate 2 removes escapes *via alternate spellings*; it does not remove `NotFound`-under-root → passthrough, `Dir` → passthrough, `Decision::Redirect`, `Decision::Serve`, the DRM exceptions, or the write fall-through. Those are gates 3, 4, and 5, and removing them here would make a failure un-attributable — the whole reason for the split.

**So the negative canary cannot yet assert unreachability.** A real file under the root that no provider serves is *still reachable* after this gate, via the passthrough that gate 3 removes. What gate 2 can assert is that every spelling of it is **classified under-root** — that it lands in a counted outcome class rather than `outside-root`, where no counter sees it.

Concretely, for the negative canary:
- Gate 2 asserts: **classified**, appearing in a counted class.
- Gate 3 asserts: **unreachable**, returning not-found.

An implementer who writes the unreachability assertion here will watch it fail, and the temptation will be to weaken something. Do not. The assertion is correct; it is merely early.

**The positive canary is fully assertable now**, and is the more valuable half at this stage: it proves canonicalisation did not break legitimate access. The cheap way to pass a containment test is to break everything, and the positive canary is what forbids that.

### Where the code goes

`vfs_core::normalize_vpath` already strips `\??\` / `\\?\` prefixes, folds separators, and resolves `.` / `..`. Its own doc comment scopes it deliberately: *"Deeper NT concerns (`\Device\…`, RootDirectory-relative opens, 8.3 short names) are edge/shim concerns and out of scope here."*

Respect that boundary. `vfs-core` is OS-independent and stays that way. The new canonicaliser lives in **`vfs-redirect`**, the shim's Windows-facing decision core, and calls `normalize_vpath` as its final step.

---

## The fourteen vectors

| # | Vector | Handled by |
|---|---|---|
| 1 | 8.3 short name (`SKYRIM~1`) | Task 2 |
| 2 | Extended-length prefix (`\\?\C:\…`) | already in `normalize_vpath` — Task 1 asserts it |
| 3 | NT device path (`\Device\HarddiskVolume3\…`) | Task 2 |
| 4 | Volume GUID path (`\\?\Volume{…}\…`) | Task 2 |
| 5 | Handle-relative open (`OBJECT_ATTRIBUTES.RootDirectory`) | Task 4 |
| 6 | CWD-relative | already handled in the shim — Task 5 asserts it |
| 7 | Junction / reparse point | Task 2 |
| 8 | Hardlink | Task 2 |
| 9 | UNC / `subst` / mapped drive | Task 2 |
| 10 | Unicode form, trailing dots or spaces | Task 1 |
| 11 | Alternate data stream (`x.esp:s`) | Task 1 |
| 12 | `.` / `..` components, trailing separators | already in `normalize_vpath` — Task 1 asserts it |
| 13 | Handle opened before the root registered | Task 5 reports; closing it is gate 3 |
| 14 | Child process without the shim | Task 5 reports; closing it may not be a shim fix at all |

Vectors 13 and 14 are **reported, not closed**, in this gate. Say so in the matrix rather than leaving a reader to assume a blank means closed.

---

### Task 1: The pure canonicaliser

**Files:** Create `crates/vfs-redirect/src/canon.rs`; modify `crates/vfs-redirect/src/lib.rs`

**Interfaces:**
- Produces: `pub fn canonicalise(raw: &str, volumes: &VolumeMap) -> Result<String, PathError>` and `pub struct VolumeMap` mapping NT device names to drive letters. `VolumeMap::empty()` for tests that exercise no device paths.

Pure and testable: no filesystem access, no Windows API. Everything needing the OS is resolved into `VolumeMap` by Task 2 and passed in. That split is what makes the fourteen-spelling table a unit test rather than an integration test.

Handles here: stream suffixes (vector 11), trailing dots and spaces (vector 10), device and volume-GUID prefixes via the map (vectors 3, 4), then delegation to `normalize_vpath` (vectors 2, 12).

- [ ] **Step 1: Write the failing test**

```rust
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
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p vfs-redirect --lib canon`
Expected: compile error — `canonicalise` and `VolumeMap` are not defined.

- [ ] **Step 3: Implement**

Order matters: split the stream suffix **before** splitting components (otherwise a `:` inside a stream name confuses the drive-letter check), resolve device and volume-GUID prefixes to a drive letter via the map, strip trailing dots and spaces per component, then hand the result to `normalize_vpath`.

**Do not lowercase the output.** Case folding is the caller's job (`RootMap` already folds), and destroying case here would break `DiskProvider`, which needs the original case to open real files. The tests lowercase only for comparison.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p vfs-redirect` then `cargo clippy -p vfs-redirect --all-targets -- -D warnings`

- [ ] **Step 5: Commit**

```bash
git add rust/crates/vfs-redirect
git commit -m "feat(redirect): canonicalise alternate NT path spellings"
```

---

### Task 2: Resolve the volume map from the OS

**Files:** Create `crates/vfs-redirect/src/volumes.rs` (or extend `vfs-win`); modify `crates/vfs-redirect/src/lib.rs`

**Interfaces:**
- Produces: `pub fn resolve_volume_map() -> VolumeMap`, populating device names and volume GUIDs for every drive letter present, plus `pub fn expand_short_name(path: &str) -> Option<String>`.

Called **once at session start**, not per open. `QueryDosDeviceW` gives the device name for a drive letter; `GetVolumeNameForVolumeMountPointW` gives the GUID. `GetLongPathNameW` expands 8.3 names.

8.3 (vector 1), junctions (7), hardlinks (8), and `subst`/mapped drives (9) all resolve the same way: **ask the OS for the final path**. `GetFinalPathNameByHandleW` on an opened handle is the authoritative answer and collapses all four at once. Prefer it where a handle exists; fall back to `GetLongPathNameW` for a path with no handle.

**Note:** 8.3 name generation may be disabled on the volume (`fsutil 8dot3name query`). If it is, vector 1 is unbuildable here — that is a fact to report in Task 6's matrix, not a reason to skip the code path.

- [ ] **Step 1: Write the failing test**

A test asserting `resolve_volume_map()` maps the current drive to a device name beginning `\Device\`, and that `expand_short_name` round-trips a path the test creates with a long name. Guard on Windows; this crate is already Windows-facing.

- [ ] **Step 2: Run to verify it fails**
- [ ] **Step 3: Implement**
- [ ] **Step 4: Run to verify it passes** — `cargo test -p vfs-redirect`, clippy clean
- [ ] **Step 5: Commit**

---

### Task 3: Wire canonicalisation into `RootMap`, with a cache

**Files:** Modify `crates/vfs-redirect/src/lib.rs`

**Interfaces:** `RootMap::new` takes a `VolumeMap`; `under_root` canonicalises before comparing.

**Cache it.** Canonicalisation per open would be a per-I/O `GetFinalPathNameByHandleW`, and this sits on the hot path that the benchmarks in `rust/docs/benchmarks/` measure. Key the cache on the **raw input string** — the existing instrumentation already shows opens repeat heavily during load, so the cache should absorb nearly all of it. Bound it, and evict rather than growing without limit in a long session.

- [ ] **Step 1: Write the failing test**

Assert that `under_root` now recognises a device-path spelling and an 8.3-style spelling of a path under the root, which it previously classified as outside. Assert also that a path genuinely outside the root is still outside — the failure mode of an over-eager canonicaliser is swallowing the whole filesystem.

- [ ] **Step 2: Run to verify it fails**
- [ ] **Step 3: Implement**
- [ ] **Step 4: Run to verify it passes** — plus a benchmark sanity check: `cargo test --workspace` must not slow noticeably. If it does, the cache is not working.
- [ ] **Step 5: Commit**

---

### Task 4: Handle-relative opens

**Files:** Modify `crates/vfs-shim/src/hook.rs`

**Interfaces:** Consumes Task 3's `RootMap`.

Vector 5: `OBJECT_ATTRIBUTES.RootDirectory` is a real directory handle plus a relative name. If the game holds a handle to `C:\Games` and opens `Skyrim\Data\a.esp` relative to it, the name the shim sees is only the relative part.

The shim already has the machinery — `record_path`, `record_identity`, `tag_under_root` track handles it issued. For a handle the shim never saw, ask the OS via `GetFinalPathNameByHandleW` rather than guessing.

- [ ] **Step 1: Write the failing test**

An e2e fixture step that opens a directory handle **outside** the managed root and then opens a file **under** the root relative to it. Assert the open is classified under-root rather than `outside-root`. This is the vector most likely to be silently wrong, because it looks like an ordinary relative path.

- [ ] **Step 2: Run to verify it fails**
- [ ] **Step 3: Implement**
- [ ] **Step 4: Run to verify it passes**
- [ ] **Step 5: Commit**

---

### Task 5: The escape fixture

**Files:** Create `crates/vfs-fixture-escape/`; modify `Cargo.toml` (workspace members) and the fixture-build list in `crates/vfs-directord/tests/e2e.rs`

**Interfaces:** A fixture executable that, given a target file, attempts to open it via each of the fourteen spellings and writes a machine-readable result line per vector: vector id, spelling attempted, outcome (opened / not-found / error code / **unbuildable**).

**`unbuildable` is a first-class outcome, not a silent skip.** Junctions need `mklink /J` (no admin), `subst` needs a free drive letter, hardlinks need the same volume, 8.3 may be disabled, the UNC admin share may need privileges. Any vector that cannot be constructed in the current environment reports `unbuildable` **with the reason**. A blank or missing line must never be readable as a pass — that is precisely how a containment guarantee rots.

Model it on `crates/vfs-fixture-writepath/`. Keep it dependency-free.

- [ ] **Step 1: Write the fixture**
- [ ] **Step 2: Wire it into the workspace and the fixture-build list**
- [ ] **Step 3: Verify it runs standalone** outside a session, against an ordinary file, and reports a full matrix with no panics. A fixture that crashes on vector 7 tells you nothing about vectors 8-14.
- [ ] **Step 4: Commit**

---

### Task 6: The canary matrix

**Files:** Modify `crates/vfs-directord/tests/e2e.rs`; create `rust/docs/escape-matrix.md`

**Interfaces:** An e2e test running the escape fixture under a composed session against two targets.

**Positive canary** — a file the provider serves. **Every buildable spelling must open it, byte-identical.** This is fully assertable now and is the half that forbids passing the containment test by breaking access.

**Negative canary** — a real file on disk under the managed root that no provider serves. **Every buildable spelling must be classified under-root** (appearing in a counted outcome class, not `outside-root`). It will still be *reachable* — gate 3 removes the passthrough that makes it so. Do not assert unreachability here; see the gate's scope note.

Write the matrix to `rust/docs/escape-matrix.md`: fourteen vectors × two canaries, with `unbuildable` rows carrying their reason, and vectors 13-14 marked **reported, not closed in this gate**.

- [ ] **Step 1: Write the failing test**
- [ ] **Step 2: Run to verify it fails**
- [ ] **Step 3: Implement and iterate** until every buildable vector behaves. **A vector that will not close is a finding — report it, do not weaken the assertion.**
- [ ] **Step 4: Full gate**

```powershell
cargo build --all-targets
cargo build --manifest-path crates/vfs-payload/Cargo.toml --target-dir target
cargo test --workspace
cargo clippy --all-targets -- -D warnings
```

- [ ] **Step 5: Commit**

---

## Gate 2 Exit Criteria

- [ ] All fourteen spellings of one path canonicalise to one form, proven by unit test.
- [ ] `RootMap::under_root` recognises every buildable spelling, and still rejects paths genuinely outside the root.
- [ ] Canonicalisation is cached; the workspace suite shows no noticeable slowdown.
- [ ] Positive canary: every buildable spelling opens the provider-served file, byte-identical.
- [ ] Negative canary: every buildable spelling is classified under-root rather than `outside-root`.
- [ ] `rust/docs/escape-matrix.md` records the matrix, with `unbuildable` reasons and vectors 13-14 marked reported-not-closed.
- [ ] Workspace at or above 407; clippy clean; payload workspace builds.
- [ ] **No bypass removed.** `Redirect`, `Serve`, the DRM exceptions, the passthrough, and the write fall-through all still present — gates 3, 4, and 5 own them.

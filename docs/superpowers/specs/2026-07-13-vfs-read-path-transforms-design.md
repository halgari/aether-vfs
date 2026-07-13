# Read-Path Decision Transforms — Design Spec

**Status:** Approved-to-proceed (read-path hook program, Slice B), ready for
planning.
**Date:** 2026-07-13
**Slice:** Read-path hook program, **Slice B** — the pure, exhaustively-tested
decision transforms for path-based operations, now tombstone-aware:
`RootMap::decide` (open) gains a real `Deny`, and a new
`RootMap::query_attributes` answers `NtQueryAttributesFile`/
`NtQueryFullAttributesFile`. Both share one root-resolution helper.
**Parent docs:** hook-surface plan (memory: *vfs-hook-surface-plan*).
**Depends on:** Slice A (first-class tombstones — `SnapResolution::Tombstone`).

---

## 1. Context

`vfs-redirect` holds the shim's pure decision logic. Slice A made tombstones
survive into the snapshot; this slice teaches the transforms to honor them and
adds the attribute-query transform, so the P2 hooks (Slices D–F) are thin ABI
shims over fully-tested logic. All work is OS-independent, `#![forbid(unsafe_code)]`,
and unit-tested against the full behavior matrix.

---

## 2. Scope & boundary

Only `crates/vfs-redirect`. No hooks, no new crates, no `unsafe`.

### In scope

- A `Decision::Deny` variant: the path is tombstoned → the hook returns
  `STATUS_OBJECT_NAME_NOT_FOUND` (do not open, do not pass through).
- `RootMap::decide` maps `SnapResolution::Tombstone → Decision::Deny` (replacing
  the Slice-A interim `PassThrough`).
- A shared private helper `locate(nt_path, snap)` performing the normalize +
  component-wise case-insensitive root match + `snap.resolve`, returning either
  `Outside` (not under root / malformed) or `Resolved(SnapResolution)`. Both
  transforms use it (DRY).
- `RootMap::query_attributes(nt_path, snap) -> AttrDecision` with
  `AttrDecision { PassThrough, Attributes { is_dir: bool, size: u64, mtime: i64 }, Deny }`.

### Out of scope

- Directory *merge* enumeration (Slice C). V-dir-add opening (a virtual-only dir
  with no backing) — `decide` keeps `Dir → PassThrough` for now; merge/virtual-dir
  handling is Slice C/D. Identity/`virtual_name` (Slice F). Any hook (P2). Writes.

---

## 3. Behavior matrix (what the transforms return)

`locate` classifies, then:

| SnapResolution | `decide` (open) | `query_attributes` |
|---|---|---|
| `File { size, mtime, source }` | `Redirect { target_nt }` | `Attributes { is_dir: false, size, mtime }` |
| `Dir` | `PassThrough` | `Attributes { is_dir: true, size: 0, mtime: 0 }` |
| `Tombstone` | `Deny` | `Deny` |
| `NotFound` | `PassThrough` | `PassThrough` |
| `Outside` (not under root / malformed / escaping) | `PassThrough` | `PassThrough` |

Root/remainder matching stays case-insensitive (via `fold`), as in the current
`decide`.

---

## 4. API

```rust
pub enum Decision {
    PassThrough,
    Redirect { target_nt: String },
    Deny,                       // NEW — tombstoned: hook returns STATUS_OBJECT_NAME_NOT_FOUND
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttrDecision {
    PassThrough,
    Attributes { is_dir: bool, size: u64, mtime: i64 },
    Deny,
}

impl RootMap {
    pub fn decide(&self, nt_path: &str, snap: &SnapshotReader) -> Decision;            // extended
    pub fn query_attributes(&self, nt_path: &str, snap: &SnapshotReader) -> AttrDecision; // new
}
```

`Decision` and `AttrDecision` derive `Debug, Clone, PartialEq, Eq` (asserted in
tests via `assert_eq!`).

---

## 5. Error handling

Total, fail-safe: any malformed/out-of-root/`NotFound` input → `PassThrough`
(never `Deny`, never `Redirect` — never hide or reroute what isn't positively a
virtual file/dir/tombstone). No panics.

## 6. Testing (pure, exhaustive)

Reuse the Slice-A snapshot fixtures (build with `vfs-core` + `vfs-shared`
`bridge::flatten`, including a tombstone). For BOTH `decide` and
`query_attributes`, one test per matrix row: file, dir, tombstone→Deny,
under-root-not-found→PassThrough, outside-root→PassThrough, malformed/escaping→
PassThrough, plus a case-insensitive hit. `query_attributes` file test asserts the
exact `size`/`mtime` from the snapshot; dir test asserts `is_dir: true`.

## 7. Dependencies & toolchain

Stable. `vfs-redirect` stays `#![forbid(unsafe_code)]`. No new deps (dev-deps
already include `vfs-core` + `vfs-shared` `bridge` for fixtures).

## 8. Out-of-scope reminders

No directory merge, no virtual-dir open, no identity, no hooks, no writes.

*End of spec.*

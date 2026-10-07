# Hook: Honor Deny (Tombstone Hiding) — Design Spec

**Status:** Approved-to-proceed (read-path hook program, Slice D / first P2 hook slice), ready for planning.
**Date:** 2026-07-13
**Slice:** **Slice D** — make the live `NtCreateFile` hook honor
`Decision::Deny`: a mod-deleted (tombstoned) path returns
`STATUS_OBJECT_NAME_NOT_FOUND` so a real on-disk file the mod deleted is actually
hidden from the target, instead of the current interim pass-through. Also extract
a shared `decision_for(oa)` helper so upcoming hooks (NtOpenFile, etc.) reuse the
ObjectName-decode + decide path.
**Parent docs:** hook-surface plan (memory: *vfs-hook-surface-plan*).
**Depends on:** Slice B (`Decision::Deny`), the existing `vfs-shim` hook.

---

## 1. Context

Slice B made the pure `decide` return `Decision::Deny` for a tombstoned path, but
the shim's hook still uses `if let Decision::Redirect { .. }`, so `Deny` falls
through to a plain trampoline call — the real file gets opened. This slice closes
that gap: the hook now returns `STATUS_OBJECT_NAME_NOT_FOUND` for `Deny`,
completing tombstone semantics end-to-end. Refactoring the ObjectName decode +
decide into one `decision_for` helper keeps the hook readable and gives the next
hooks a single reuse point.

---

## 2. Scope & boundary

Only `crates/vfs-shim`. `unsafe` stays confined to `hook.rs`.

### In scope

- `ntdef`: `STATUS_OBJECT_NAME_NOT_FOUND: NTSTATUS = 0xC000_0034u32 as i32`.
- `hook.rs`: extract `unsafe fn decision_for(oa: *const ObjectAttributes) ->
  Option<Decision>` (returns `None` when ineligible: no engine, null/relative OA,
  null/empty ObjectName). The `NtCreateFile` hook matches its result:
  `Redirect` → rewrite + trampoline (as today), `Deny` → return
  `STATUS_OBJECT_NAME_NOT_FOUND`, `PassThrough`/`None` → trampoline unchanged.
- An integration test proving a tombstoned real file is hidden while a
  non-virtualized real file still passes through.

### Out of scope

- Hooking additional functions (NtOpenFile is Slice E; attrs/dir/identity later).
- `RootDirectory`-relative opens (still `None` → pass through).
- The rewrite-buffer construction stays inline in the hook (self-referential
  locals are awkward to extract; only the decode+decide is shared).

---

## 3. Behavior

`decision_for(oa)`:
- `None` if `ENGINE` unset, `oa` null, `root_directory` non-null (relative),
  `object_name` null, or its `buffer` null.
- else `Some(engine.decide(&utf16_to_string(name_units)))`.

`NtCreateFile` hook:
- `Some(Redirect { target_nt })` → build a local `Vec<u16>` + `UnicodeString` +
  copied `ObjectAttributes` pointing at it, call the trampoline (unchanged).
- `Some(Deny)` → return `STATUS_OBJECT_NAME_NOT_FOUND` (do not call the trampoline
  — the file must appear absent).
- `Some(PassThrough)` or `None` → call the trampoline with the original args.

No panics, no hookable I/O — same discipline as before.

---

## 4. API

No public API change (`install`/`HookGuard`/`Engine` unchanged). `ntdef` gains
one const; `hook.rs` gains one private helper. The existing `hook_redirect`
integration test continues to pass (it exercises a `Redirect`).

## 5. Error handling

The hook returns `STATUS_OBJECT_NAME_NOT_FOUND` for `Deny` and
`STATUS_UNSUCCESSFUL` only if the trampoline is somehow unset (unchanged
invariant). Never panics.

## 6. Testing

New single-test integration binary `tests/hook_deny.rs`:
- Create two real files under a temp root: `hidden.esp` and `visible.esp`.
- Snapshot tombstones `hidden.esp`; says nothing about `visible.esp`.
- `install(Engine::new(root, snapshot))`.
- `std::fs::read(hidden.esp)` → `Err(ErrorKind::NotFound)` (hidden despite
  existing on disk — proves Deny).
- `std::fs::read(visible.esp)` → the real bytes (pass-through unaffected).

(`std::fs::read` → `CreateFileW` → `NtCreateFile`; `STATUS_OBJECT_NAME_NOT_FOUND`
surfaces as `ERROR_FILE_NOT_FOUND` → `ErrorKind::NotFound`.)

## 7. Dependencies & toolchain

Stable. No new deps (dev-deps already include `vfs-core` + `vfs-shared` `bridge`).
`#![deny(unsafe_code)]` crate-level with `unsafe` confined to `hook.rs`.

## 8. Out-of-scope reminders

No new hooked functions, no relative opens, no attrs/dir/identity, no writes.

*End of spec.*

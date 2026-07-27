# Hook: Honor Deny (Tombstone Hiding) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The live `NtCreateFile` hook returns `STATUS_OBJECT_NAME_NOT_FOUND` for a tombstoned path, so a mod-deleted real file is hidden from the target — proven by an integration test where a real on-disk file becomes unreadable while a non-virtualized real file still reads.

**Architecture:** Extract the ObjectName-decode + `engine.decide` into `unsafe fn decision_for(oa) -> Option<Decision>`. The hook matches it: `Redirect` → rewrite+trampoline, `Deny` → `STATUS_OBJECT_NAME_NOT_FOUND`, `PassThrough`/`None` → trampoline.

**Tech Stack:** Rust (stable). `vfs-shim` (`#![deny(unsafe_code)]`, unsafe in `hook.rs`).

## Global Constraints

- Stable; all `unsafe` stays in `hook.rs`; crate root `#![deny(unsafe_code)]`.
- The hook never panics and does no hookable I/O (`match`/`if let` only; no `unwrap`).
- The integration test is its OWN single-`#[test]` binary (process-global hook install must not race other tests).

---

### Task 1: `STATUS_OBJECT_NAME_NOT_FOUND` + `decision_for` + Deny handling

**Files:**
- Modify: `crates/vfs-shim/src/ntdef.rs`
- Modify: `crates/vfs-shim/src/hook.rs`
- Test: `crates/vfs-shim/tests/hook_deny.rs`

**Interfaces:**
- Consumes: `Engine::decide`, `Decision::{Redirect, Deny, PassThrough}`.
- Produces: the hook honoring `Deny`; `ntdef::STATUS_OBJECT_NAME_NOT_FOUND`.

- [ ] **Step 1: Add the status constant**

In `crates/vfs-shim/src/ntdef.rs`, next to `STATUS_UNSUCCESSFUL`:

```rust
/// `STATUS_OBJECT_NAME_NOT_FOUND` — returned for a tombstoned (mod-deleted) path
/// so the real on-disk file appears absent.
pub const STATUS_OBJECT_NAME_NOT_FOUND: NTSTATUS = 0xC000_0034u32 as i32;
```

- [ ] **Step 2: Write the failing integration test**

Create `crates/vfs-shim/tests/hook_deny.rs`:

```rust
//! Single-test binary: a tombstoned real file must be hidden by the hook.
use vfs_shim::{install, Engine};

#[test]
fn tombstone_hides_a_real_file_and_others_pass_through() {
    let pid = std::process::id();
    let root = std::env::temp_dir().join(format!("vfs-shim-deny-{pid}"));
    std::fs::create_dir_all(&root).unwrap();

    // Two REAL files on disk under the managed root.
    let hidden = root.join("hidden.esp");
    let visible = root.join("visible.esp");
    std::fs::write(&hidden, b"SHOULD BE HIDDEN").unwrap();
    std::fs::write(&visible, b"SHOULD BE VISIBLE").unwrap();

    // Snapshot tombstones hidden.esp; says nothing about visible.esp.
    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "hidden.esp".into(),
                kind: EntryKind::Tombstone,
                source: "".into(),
                size: 0,
                mtime: 0,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };

    let engine = Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let _guard = install(engine).expect("install");

    // Tombstoned real file is hidden even though it exists on disk.
    let err = std::fs::read(&hidden).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);

    // A non-virtualized real file still reads (pass-through).
    assert_eq!(std::fs::read(&visible).unwrap(), b"SHOULD BE VISIBLE");
}
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p vfs-shim --test hook_deny`
Expected: FAIL — with the current `if let Decision::Redirect` hook, `hidden.esp`
falls through to the trampoline and reads successfully, so
`std::fs::read(&hidden).unwrap_err()` panics ("called unwrap_err on Ok").

- [ ] **Step 4: Refactor the hook to honor Deny**

In `crates/vfs-shim/src/hook.rs`:

(a) Import the new constant — change the `ntdef` use line to:

```rust
use crate::ntdef::{
    NtCreateFileFn, ObjectAttributes, UnicodeString, STATUS_OBJECT_NAME_NOT_FOUND,
    STATUS_UNSUCCESSFUL,
};
```

(b) Add the shared helper (place it above `hook`):

```rust
/// Decode the ObjectName and ask the engine what to do. Returns `None` when the
/// open is ineligible (no engine, null/relative OA, or empty ObjectName).
unsafe fn decision_for(oa: *const ObjectAttributes) -> Option<Decision> {
    let engine = ENGINE.get()?;
    if oa.is_null() {
        return None;
    }
    let oa_ref = &*oa;
    // MVP: only fully-qualified opens (no RootDirectory-relative).
    if !oa_ref.root_directory.is_null() || oa_ref.object_name.is_null() {
        return None;
    }
    let us = &*oa_ref.object_name;
    if us.buffer.is_null() {
        return None;
    }
    let units = core::slice::from_raw_parts(us.buffer, us.length as usize / 2);
    let path = String::from_utf16_lossy(units);
    Some(engine.decide(&path))
}
```

(c) Replace the body of `hook` (keep its signature) with:

```rust
    // Invariant: TRAMPOLINE is Some once the detour is enabled.
    let tramp = match TRAMPOLINE {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };

    match decision_for(oa) {
        Some(Decision::Redirect { target_nt }) => {
            // Buffers live across the synchronous trampoline call.
            let mut wbuf: Vec<u16> = target_nt.encode_utf16().collect();
            let byte_len = (wbuf.len() * 2) as u16;
            let new_us = UnicodeString {
                length: byte_len,
                maximum_length: byte_len,
                buffer: wbuf.as_mut_ptr(),
            };
            let oa_ref = &*oa;
            let new_oa = ObjectAttributes {
                length: oa_ref.length,
                root_directory: core::ptr::null_mut(),
                object_name: &new_us,
                attributes: oa_ref.attributes,
                security_descriptor: oa_ref.security_descriptor,
                security_qos: oa_ref.security_qos,
            };
            let status = tramp(
                file_handle, access, &new_oa, iosb, alloc, attrs, share, disp, opts, ea, ealen,
            );
            drop(wbuf);
            status
        }
        Some(Decision::Deny) => STATUS_OBJECT_NAME_NOT_FOUND,
        Some(Decision::PassThrough) | None => {
            tramp(file_handle, access, oa, iosb, alloc, attrs, share, disp, opts, ea, ealen)
        }
    }
```

- [ ] **Step 5: Run the deny test and the existing redirect test**

Run: `cargo test -p vfs-shim --test hook_deny`
Expected: PASS.

Run: `cargo test -p vfs-shim --test hook_redirect`
Expected: PASS (redirect still works through the refactored hook).

- [ ] **Step 6: Run all vfs-shim tests**

Run: `cargo test -p vfs-shim`
Expected: engine unit tests + `hook_redirect` + `hook_deny` all pass.

- [ ] **Step 7: Commit**

```bash
git add crates/vfs-shim/src/ntdef.rs crates/vfs-shim/src/hook.rs crates/vfs-shim/tests/hook_deny.rs
git commit -m "vfs-shim: hook honors Decision::Deny (tombstone hides real file)"
```

---

### Task 2: Verification sweep

**Files:** none.

- [ ] **Step 1: Full workspace test**

Run: `cargo test --workspace`
Expected: all green.

- [ ] **Step 2: Warning check**

Run: `cargo build --workspace 2>&1 | rg -i "warning" || echo "no warnings"`
Expected: `no warnings`.

- [ ] **Step 3: Unsafe audit**

Confirm all new `unsafe` is in `hook.rs`; `#![deny(unsafe_code)]` intact at the crate root.

- [ ] **Step 4: Commit Cargo.lock (if changed)**

```bash
git add Cargo.lock
git commit -m "hook-deny: update Cargo.lock" || echo "nothing to commit"
```

---

## Self-Review Notes

- **Spec coverage:** const (Task 1 Step 1), `decision_for` + Deny arm (Step 4), integration proof of hide + pass-through (Step 2/5).
- **No-panic:** hook uses `match`/`if let`; no `unwrap`. The test's `unwrap_err`/`unwrap` are in the TEST (a panic there is a test failure, which is intended signal).
- **Refactor safety:** `decision_for` reproduces the exact eligibility guards the old inline code used; the redirect arm is byte-for-byte the previous behavior, so `hook_redirect` stays green.
- **Isolation:** `hook_deny.rs` is its own test binary (separate process) so its global hook install doesn't collide with `hook_redirect.rs`.

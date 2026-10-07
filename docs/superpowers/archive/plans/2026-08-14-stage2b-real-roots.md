# Stage 2b: Real Roots — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `RootId` mean something. A session virtualizes several filesystem locations, each with exactly one provider, addressed as `(RootId, root-relative path)`.

**Architecture:** Stage 1 put `RootId` in the type system with every call site passing `RootId::DEFAULT`. This stage threads it through for real: the shim's `RootMap` becomes multi-root, `Director` drops its layer-ordered mount merge in favour of one provider per root, and the block cache folds the root into its keys.

**Tech Stack:** Rust 2021, Windows NT API, shared-memory ring IPC, gRPC control plane.

## Global Constraints

- Design spec: `docs/superpowers/specs/2026-08-13-no-bypass-and-real-roots-design.md` §5 (addressing), §7 (why 2b sits here), §9 (acceptance).
- Working directory for all cargo commands is `C:\oss\aether-vfs\rust`.
- Baseline: `cargo test --workspace` = 452 passed, 0 failed. Never lower it.
- Zero clippy warnings: `cargo clippy --all-targets -- -D warnings`.
- `vfs-payload` is a separate workspace: `cargo build --manifest-path crates/vfs-payload/Cargo.toml --target-dir target`.
- **The full suite exceeds ten minutes** and launches real processes. Scope your runs and say what you ran.
- A stray `vfs.exe` daemon can lock the build so `cargo test` yields **no result at all** rather than a failure. Check before concluding a run failed.
- `vfs-shim::writes_land_in_overlay_with_cow` (`crates/vfs-shim/tests/hook_write.rs`) has been flaky under parallel contention; it passes standalone. **Do not extend this allowance to any other test.** An earlier version of this line also named `vfs-inject::injected_shim_passes_full_acceptance_suite` — that test does not exist and never did, and a phantom "known flake" is an invitation to dismiss a real failure. If a test fails, it failed.
- Conventional commit prefixes. Commit after every task.

### Why this stage sits between gates 3 and 4

Gate 1's deep-session baseline established that **Skyrim's saves are invisible to the counters**: they travel through an NTFS junction (`Documents\My Games` → a real directory) outside any managed root, so the shim tags them `outside-root` and neither `Routed` nor any fall-through class sees them.

Gate 4 closes the write fall-through. Without a second root, it would be closing it against a path nothing real exercises. So `Documents\My Games\Skyrim` must become a managed root first.

### What this stage does NOT touch

The write fall-through, `Decision::Redirect`/`Serve`/`Deny`, the legacy `zipserve` path, and the four DRM filename exceptions all stay. Gates 4 and 5 own them, and removing one here makes a later failure un-attributable.

### Known defects this stage must fix, not inherit

Gate 3 found three things that a multi-root design walks straight into:

1. **`Director::readdir` never lists a mount registered below the queried directory.** A non-root mount can be opened by a known path but cannot be discovered by listing. Mounts are how roots compose, so this is load-bearing here.
2. **Non-root mounts only match when spelled all-lowercase** — mount prefixes compare case-sensitively while shim vpaths are lowercased.
3. **The block cache keys on `(path, size, mtime)` with no root**, so the same relative path under two roots collides. Inert while every call site passes `RootId::DEFAULT`; a real bug the moment this stage lands. There is a comment at the call site recording it.

---

### Task 1: Fix mount resolution before building on it

**Files:** `crates/vfs-director/src/director.rs`, `crates/vfs-director/src/path.rs`, `crates/vfs-directord/src/registry.rs`

**Interfaces:** `Director::readdir` lists mounts registered below the queried directory. Mount prefix matching is case-insensitive, consistent with how the shim spells vpaths.

**Why first:** every later task composes providers via mounts. Building multi-root on a mount system that cannot enumerate its own mounts would produce failures that look like root bugs.

- [ ] **Step 1: Write the failing tests**

Two: a mount registered at `data/somemod` must appear when listing `data`; and a mount registered as `Data/SomeMod` must match a lookup for `data/somemod`. Both fail today — the second is the reason gate 3's documented MO2 remedy did not work as written.

- [ ] **Step 2: Run to verify they fail**

- [ ] **Step 3: Implement**

For enumeration: when listing a directory, include the next path component of any mount whose prefix extends below it, as a synthetic directory entry. Watch for duplicates where a provider already supplies that name.

For case: fold both sides at comparison rather than lowercasing stored prefixes, so a mount's configured spelling survives for diagnostics.

- [ ] **Step 4: Run to verify they pass**, then the affected crates and clippy.

- [ ] **Step 5: Commit**

---

### Task 2: Roots become a declared table

**Files:** `crates/vfs-control/src/config.rs`, `crates/vfs-directord/src/registry.rs`, `crates/vfs-director/src/session.rs`

**Interfaces:** A session declares roots with an id, a name, and a host path. `SessionConfig` gains a `[[root]]` table; `SourceEntry` gains a `root` selector and **loses `layer`**.

**The spec's decided shape** (§6): one provider per root, and combining is done by providers that take providers. `layer` disappears because layering becomes an explicit `layered(...)` in the graph rather than an implicit ordering.

**Keep the flat `[[source]]` list working** as documented sugar for "layered of these, mounted at root 0" so existing configs and tests survive. Mark it deprecated in the doc comment.

- [ ] **Step 1: Write the failing test**

A config declaring two roots with a provider each parses, and the resulting session resolves the same relative path differently under each.

- [ ] **Step 2: Run to verify it fails**

- [ ] **Step 3: Implement**

- [ ] **Step 4: Verify** the flat-list sugar still parses and still produces the previous behaviour. **If an existing config test needs its assertions changed, stop and say why** — sugar that silently changes meaning is worse than sugar removed.

- [ ] **Step 5: Commit**

---

### Task 3: Thread `RootId` through the director

**Files:** `crates/vfs-director/src/director.rs`, `crates/vfs-director/src/ring_dispatch.rs`, `crates/vfs-protocol/src/lib.rs`

**Interfaces:** `Director` maps `RootId` → provider. Its public methods carry a root. The ring's path-carrying opcodes carry a root id.

**Delete the mount merge.** `Director::getattr`/`readdir`/`open` currently iterate mounts in reverse and merge. With one provider per root, resolution is a single lookup. Everything that was implicit merging becomes an explicit `layered(...)` in the provider graph, where it is visible.

**The wire is a contract with the injected DLL.** `GetAttrReq`/`ReadDirReq`/`OpenReq` already carry a `root` field from Stage 1 — check before adding anything, and do not renumber.

- [ ] **Step 1: Write the failing test**

`[0, "a.txt"]` and `[1, "a.txt"]` resolve to different content through the director, end to end via `dispatch_director`.

- [ ] **Step 2: Run to verify it fails**

- [ ] **Step 3: Implement**

- [ ] **Step 4: Verify** — the escape-matrix e2e and the write-path e2e must still pass. Name any test whose assertions you changed and why.

- [ ] **Step 5: Commit**

---

### Task 4: The cache must key on root

**Files:** `crates/vfs-cache/src/provider.rs`, `crates/vfs-cache/src/store.rs`

**Interfaces:** `file_id_for` folds the root into the key.

There is a comment at the call site recording this exact deferral. **The bug becomes live the moment Task 3 lands**, because two roots can then legitimately serve the same relative path with the same size and mtime — and the cache would return one root's bytes for the other's file.

- [ ] **Step 1: Write the failing test**

Two roots, same relative path, same size and mtime, different content. Read through a `CachingProvider` and assert each root gets its own bytes. This must fail before the fix — if it passes, the collision is being avoided by something else and you should find out what before changing anything.

- [ ] **Step 2: Run to verify it fails**

- [ ] **Step 3: Implement**, and remove the deferral comment.

- [ ] **Step 4: Verify**, including that `CachingProvider` still passes `assert_conformance` with a writable inner.

- [ ] **Step 5: Commit**

---

### Task 5: The shim becomes multi-root

**Files:** `crates/vfs-redirect/src/lib.rs`, `crates/vfs-shim/src/fuse_client.rs`, `crates/vfs-shim/src/engine.rs`

**Interfaces:** `RootMap` holds several roots and yields `(RootId, relative path)`. `fuse_client`'s under-root predicate agrees with it.

**This is the task that should fix the two-predicate asymmetry**, and it is the right moment because both predicates are being rewritten anyway. Gate 3 found that `fuse_client::vpath_under_root` has no equivalent to `RootMap`'s canonicalisation, so five alternate spellings were classified but never routed — and a name-based attribute query on an unserved file still reaches real disk through the client predicate. There is a test recording that gap; **it should flip when you unify them.**

If unifying turns out to be larger than this task can hold, say so and scope it — but do not leave two predicates that disagree while adding a second root to both.

- [ ] **Step 1: Write the failing tests**

A path under root 1 resolves to `(RootId(1), rel)`; a path under root 0 to `(RootId(0), rel)`; a path under neither is outside. Plus: the recorded metadata-gap test flips.

- [ ] **Step 2: Run to verify they fail**

- [ ] **Step 3: Implement**

- [ ] **Step 4: Verify** — the full escape-matrix must pass **against every root, not just the first**. That is an acceptance criterion, and it is the check most likely to reveal a root-index assumption baked into the canonicaliser.

- [ ] **Step 5: Commit**

---

### Task 6: A two-root Skyrim session

**Files:** `crates/vfs-directord/src/bin/skyrim-live.rs`, `tools/gamectl.ps1`, `rust/docs/bypass-baseline.md`

**Interfaces:** No new API. This is the acceptance evidence.

Declare two roots — the game directory and `Documents\My Games\Skyrim` — with a provider on each, launch, and record what changes.

**The headline question, and the reason this stage sits before gate 4:** does the game's own save now land under a managed root and route through the director? Gate 1's baseline established that saves were invisible because they went through a junction outside any root. If they are now visible, `FellThroughWriteFallback` should become non-zero — which is gate 4's workload appearing for the first time.

**Either answer is informative. Do not tune the run to get the pleasing one.** If saves still bypass, say why — that is a finding gate 4 must have.

- [ ] **Step 1: Extend the harness for a second root**

- [ ] **Step 2: Run a session that saves**, using the plugin-removal approach recorded in the baseline for getting past blocking dialogs rather than automating around them.

- [ ] **Step 3: Record in `bypass-baseline.md`**, alongside the previous runs rather than replacing them: the per-outcome counts, the reconciliation, and specifically whether the save routed.

- [ ] **Step 4: Full gate**

```powershell
cargo build --all-targets
cargo build --manifest-path crates/vfs-payload/Cargo.toml --target-dir target
cargo test --workspace
cargo clippy --all-targets -- -D warnings
```

- [ ] **Step 5: Commit**

---

## Stage 2b Exit Criteria

- [ ] A two-root Skyrim session runs with a different provider on each root.
- [ ] The same relative path under two roots resolves independently, including through the block cache.
- [ ] `Director` has no mount merge; `layer` is gone from config; the flat `[[source]]` sugar still works.
- [ ] Mounts below a queried directory enumerate, and mount matching is case-insensitive.
- [ ] The escape-matrix passes against **every** root, not just the first.
- [ ] The two under-root predicates agree, or the remaining gap is scoped and recorded with a test.
- [ ] Whether the game's save routes through the director is recorded either way.
- [ ] Workspace at or above 452; clippy clean; payload workspace builds.
- [ ] **The write fall-through, `Decision::Redirect`/`Serve`/`Deny`, `zipserve`, and the DRM exceptions all still exist** — gates 4 and 5 own them.

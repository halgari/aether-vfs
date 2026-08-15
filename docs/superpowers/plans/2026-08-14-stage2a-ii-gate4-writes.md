# Gate 4: Close the Write Fall-Through — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the director the only path a write can take. When a write to a path under a managed root cannot be served by the director, it fails — it does not quietly land in a shim-local overlay or on the real filesystem.

**Architecture:** The director already serves writes end to end (all five write opcodes dispatch, every combinator and `DiskProvider` implements the write trait methods). What remains is the shim's fall-through: on a director error, `try_fuse_create` returns `None`, and `Engine::decide_open` then redirects the write into a shim-local overlay. This gate removes that escape, redesigns copy-up to source its bytes from the director, and deletes the decision variants and the zip-window serving code that the redesign strands.

**Tech Stack:** Rust 2021, Windows NT API, shared-memory ring IPC, gRPC control plane.

## Global Constraints

- Design spec: `docs/superpowers/specs/2026-08-13-no-bypass-and-real-roots-design.md` — §3 (the invariant), §4 (what is deleted, **including the two 2026-08-14 corrections**), §8 (acceptance criteria), §8b (the prerequisite this gate must not start without).
- Working directory for all cargo commands is `C:\oss\aether-vfs\rust`.
- Baseline: `cargo test --workspace` = **499 passed, 0 failed, 1 ignored**. Never lower it.
- Zero clippy warnings: `cargo clippy --all-targets -- -D warnings`.
- `vfs-payload` is a separate workspace: `cargo build --manifest-path crates/vfs-payload/Cargo.toml --target-dir target`.
- **The full suite exceeds ten minutes** and launches real processes. Scope your runs and state exactly what you ran.
- A stray `vfs.exe` daemon can lock the build so `cargo test` yields **no result at all** rather than a failure. Check before concluding a run failed.
- `vfs-shim::writes_land_in_overlay_with_cow` (`crates/vfs-shim/tests/hook_write.rs`) has been flaky under parallel contention and passes standalone. **Do not extend that allowance to any other test** — if a test fails, it failed. This task list changes that test's premise; see Task 6.
- **The shim DLL goes stale silently.** `tools/gamectl.ps1 -Action launch` runs the **release** binary, so a fresh debug build proves nothing. Rebuild release (`-p vfs-shim-dll`, the payload workspace, `-p vfs-directord --bin skyrim-live`, each `--release`) and confirm the file size changes before trusting any live result.
- Conventional commit prefixes. Commit after every task.

### What this gate does NOT touch

The four DRM filename exceptions (`hook.rs`, ~1099-1138) stay. **Gate 5 owns them, alone and last.** Removing one here makes a later Steam-interaction failure un-attributable.

### Facts established by survey, so you do not rediscover them

1. **The director already serves writes.** `OP_WRITE`/`OP_SETATTR`/`OP_RENAME`/`OP_DELETE`/`OP_MKDIR` all dispatch in `ring_dispatch.rs` (arms at 190/207/214/221/228). Every combinator, `CachingProvider`, `MountGraph`, and `DiskProvider` implement the write trait methods. This gate builds no new write support.
2. **`Decision::Serve` is already dead in production.** Both its arms (`hook.rs:1518-1528`, `1773-1782`) begin `if crate::fuse_client::global().is_some() { return STATUS_OBJECT_NAME_NOT_FOUND; }`, and bootstrap always installs the FUSE client. `Redirect` and `Deny` are genuinely live.
3. **`zipserve.rs` is two things, and only one is deletable.** See Task 7 — the spec's original "delete zipserve" was wrong and has been corrected.
4. **The overlay has no root component** (`overlay.rs:22-38`), so it collides across roots. That is why Task 2 precedes Task 3.
5. **Two write fixtures are unwired.** `vfs-fixture-writeset` and `vfs-fixture-write` are workspace members that no test harness invokes. Task 8 decides their fate rather than leaving them.

---

### Task 1: Surface the write counters

**Files:** `crates/vfs-director/src/io_stats.rs`

**Interfaces:** `snapshot_report` prints write ops and write bytes alongside the existing open/read counts; `Totals` carries them.

**This is spec §8b, an explicit prerequisite: "Gate 4's entire job is driving write fall-through to zero; it cannot begin without a readable number for the writes that *did* route."** `ops_write` (io_stats.rs:39), `total_write_bytes` (:43), and `PathStats::writes`/`write_bytes` (:23-24) all exist and are updated by `record_write` (:214-228), but `snapshot_report` (:266-347) prints none of them and `Totals`/`totals()` (:67-118) omit them entirely.

Without this you cannot tell "writes routed" from "no writes happened" — which is exactly the ambiguity that made stage 2b's `FellThroughWriteFallback=0` uninformative.

- [ ] **Step 1: Write the failing test**

Record a write through `record_write`, take a `snapshot_report`, and assert the rendered string contains the write op count and byte count. Assert `totals()` reports them too. Both fail today.

- [ ] **Step 2: Run to verify it fails**

- [ ] **Step 3: Implement**

Extend the `"vfs-io t+{}s ops: …"` line (:288-299) and the per-path lines (:311-321). Keep the existing field names and order unchanged — `tools/gamectl.ps1 -Action stats` and `rust/docs/bypass-baseline.md` both parse this text, and reordering silently breaks the comparison against three recorded sessions.

- [ ] **Step 4: Run to verify it passes**, then `-p vfs-director` and clippy.

- [ ] **Step 5: Commit**

---

### Task 2: The overlay becomes root-aware

**Files:** `crates/vfs-shim/src/overlay.rs`

**Interfaces:** `Overlay`'s path-keyed methods take a `RootId` alongside the components. `file_path(root, comps)`, `lookup(root, comps)`, `has_file(root, comps)`, `whiteout(root, comps)`, `clear_whiteout(root, comps)`, `rename(root, from, to)`, `apply_to_listing(root, dir_comps, merged, wildcard)`.

`overlay.rs:22-38` documents the gap: the overlay keys by folded path components **with no root component**, so two roots serving the same relative path collide in one overlay directory. This is the identical collision `CachingProvider` had before stage 2b mixed `RootId` into `file_id_for` (guarded by `two_roots_same_path_size_and_mtime_do_not_collide`) — use that as the precedent for what a good fix and its test look like.

**Do this before Task 3.** Making `Engine` multi-root on a root-blind overlay would make the collision live rather than latent.

- [ ] **Step 1: Write the failing test**

Two roots, the same relative path, different content, written through one `Overlay`. Assert each root reads back its own bytes, and that a whiteout under one root does not hide the other root's file. Must fail before the fix — if it passes, find out why before changing anything.

- [ ] **Step 2: Run to verify it fails**

- [ ] **Step 3: Implement**

Derive the on-disk layout from the root. **State in the report what happens to an existing overlay directory written by the old layout** — if files under the old paths become unreachable, say so plainly; that is a user-visible consequence, not an implementation detail.

- [ ] **Step 4: Run to verify it passes**, then `-p vfs-shim` and clippy.

- [ ] **Step 5: Commit**

---

### Task 3: `Engine` becomes multi-root

**Files:** `crates/vfs-shim/src/engine.rs`, `crates/vfs-shim/src/bootstrap.rs`

**Interfaces:** `Engine` holds a multi-root `RootMap` and its decisions carry the `RootId` the path resolved under.

`engine.rs:88,121,162` build `RootMap::new(&self.root, …)` — a single root, `RootId::DEFAULT` only — and `bootstrap.rs:110,112` pass one root. So a path under root ≥1 gets `Located::Outside` → `Decision::PassThrough`, and **a root-≥1 write that misses the director lands on real disk with no redirect and no deny**, where root 0 would at least be redirected to the overlay.

Stage 2b created this and recorded it deliberately. `RootMap` is already multi-root (stage 2b), and `VFS_VIRTUAL_ROOTS` already tells the shim where the other roots are — this task consumes what is already there.

- [ ] **Step 1: Flip the recorded-gap test**

`a_write_under_a_second_root_passes_through_to_real_disk_today` (`engine.rs:608-663`, doc comment at 585-607) asserts the wrong behaviour on purpose: root 0 gets `Redirect`, an identical write under a second root gets `PassThrough` for both `FILE_OVERWRITE_IF` and `FILE_OPEN_IF`. Its comment says "When it does, this test fails — that is the point."

Rewrite it to assert the *closed* behaviour and rename it accordingly. Keep the contrast against root 0 — a test that only checks root 1 cannot tell "both roots handled" from "both roots broken".

- [ ] **Step 2: Run to verify it fails**

- [ ] **Step 3: Implement**

- [ ] **Step 4: Verify** — run `-p vfs-shim`, `-p vfs-redirect`, `-p vfs-director` and clippy. **Name any test whose assertions you changed and why.**

- [ ] **Step 5: Commit**

---

### Task 4: Copy-up sources its bytes from the director

**Files:** `crates/vfs-shim/src/engine.rs`

**Interfaces:** `Engine::cow_seed(&self, root: RootId, rel: &[String], dest: &Path) -> bool` materialises copy-up content by reading through the director.

`cow_seed` (`engine.rs:253-276`) re-runs the **snapshot-only** decision (`map.decide(nt_path, &reader)`) and copies from whatever it yields: `Decision::Redirect{target_nt}` → `std::fs::copy`; `Decision::Serve{container_nt,offset,length}` → `zipserve::copy_window_to_file`; anything else → `std::fs::copy` off the raw NT path. **None of those consult the director.** Two callers, both in this file: `decide_open` (:244, when `intent.preserves && !ov.has_file(&comps)`) and `rename` (:323, when `!ov.has_file(&from)`).

This is the compile-time blocker for Task 6. It is also a correctness bug in its own right: copy-up currently seeds from the *real filesystem* under a managed root, which is precisely the content the invariant says is unreachable.

- [ ] **Step 1: Write the failing test**

A file that exists **only** in the provider graph (not on real disk under the root) is opened with a preserving disposition; assert the copy-up destination receives the provider's bytes. Then a file that exists **only on real disk** under the root: assert copy-up does **not** seed from it. The second assertion is the one that matters — it is the invariant.

- [ ] **Step 2: Run to verify it fails**

- [ ] **Step 3: Implement**

Read through `fuse_client` (open → read → close). Handle the read spanning multiple ring round-trips for a large file rather than assuming one call returns everything. On a director error, **fail the copy-up** — do not fall back to `std::fs::copy`, which would reintroduce the escape this task removes.

- [ ] **Step 4: Run to verify it passes**, then `-p vfs-shim` and clippy.

- [ ] **Step 5: Commit**

---

### Task 5: Close the write fall-through

**Files:** `crates/vfs-shim/src/hook.rs`

**Interfaces:** A write to a path under a managed root that the director cannot serve returns an NT error status. It does not return `None`.

Two sites inside `try_fuse_create` (signature at :1052-1072):

- **:1199-1213** — `Err(st) if st == ST_NOT_FOUND` and `write`: records `FellThroughWriteFallback`, returns `None`.
- **:1229-1236** — `Err(_) if write` (any other director error): records `FellThroughWriteFallback`, returns `None`.

Returning `None` sends the caller (`create_hook` ~:1461, `open_hook` ~:1721) to `decision_for` → `Engine::decide_open`, which redirects into the shim-local overlay when one is configured and **passes through to real disk when one is not**.

**Keep the counter.** It must stay wired and must read zero — a removed counter cannot prove the class stayed closed, and the reconciliation check in the acceptance criteria depends on it.

**Do not collapse the two sites into one status.** `ST_NOT_FOUND` on a write is a create against a path no provider serves; another error is a provider that failed. They deserve different NT statuses, and merging them would lose the distinction exactly where a live failure needs it.

`ST_EXISTS` (:1226) already returns `STATUS_OBJECT_NAME_COLLISION` and is **not** a fall-through path — leave it alone.

- [ ] **Step 1: Write the failing tests**

A write to a path under a managed root that no provider serves fails with an NT error **and creates no file on the real filesystem under the root**. A write that the director *can* serve still succeeds. Both through the real hook path.

Plus spec §8 criterion 4, which no existing test states in one place: **a write through the director is visible to a subsequent read through the director, and lands where the provider graph says — not on the real filesystem under the root.** Assert all three in one test: write, read back through the director, and check the real path under the root is still absent. The existing writepath e2e proves the round-trip; it does not assert the third clause.

- [ ] **Step 2: Run to verify they fail**

- [ ] **Step 3: Implement**

- [ ] **Step 4: Verify** — the write-path e2e tests (`e2e.rs:430` `scenario_toml_disk_source_fixture_writepath`, `:659` `scenario_toml_two_disk_sources_fixture_writepath`) must still pass. They are the decisive end-to-end proof for the whole write path. If either fails, **stop** — that is a real regression, not a test to update.

- [ ] **Step 5: Commit**

---

### Task 6: Retire the overlay write path, or justify keeping it

**Files:** `crates/vfs-shim/src/engine.rs`, `crates/vfs-shim/tests/hook_write.rs`

**Interfaces:** Determined by this task's own finding — see below.

With Task 5 landed, no write reaches `Engine::decide_open`'s overlay redirect through the fall-through any more. That raises a question this plan deliberately does not pre-answer: **is the shim-local overlay still reachable at all, and if so, by what?**

`Engine`'s overlay surface is consumed by `hook.rs` at :1956/:2027/:2098 (attr hooks), :2268/:2314-2319 (rename/delete), and :3542-3550 (listing merge) — those are metadata and enumeration paths, not the write fall-through.

- [ ] **Step 1: Determine reachability**

For each of those call sites, establish whether it can still be reached now that metadata routes (gate 3) and writes route (Task 5). Report the finding **before** changing anything.

- [ ] **Step 2: Act on the finding**

If the overlay write path is genuinely unreachable, delete it and say what that means for `writes_land_in_overlay_with_cow` (`hook_write.rs`), whose whole premise is that writes land in the overlay. **That test asserts the behaviour this gate removes** — it should be rewritten to assert writes land in the *provider graph*, or deleted with a stated reason. Do not leave it passing against a path nothing uses.

If some of it is still reachable, keep exactly that much and record why, with the call site named.

- [ ] **Step 3: Verify** — `-p vfs-shim` and clippy.

- [ ] **Step 4: Commit**

---

### Task 7: Delete the stranded decision variants and the zip-window half

**Files:** `crates/vfs-redirect/src/lib.rs`, `crates/vfs-shim/src/hook.rs`, `crates/vfs-shim/src/engine.rs`, `crates/vfs-shim/src/zipserve.rs`

**Interfaces:** `Decision` collapses to under-root or not. `zipserve` retains only its synthetic-section half.

**`Decision`.** The enum is at `vfs-redirect/src/lib.rs:685-697`; `RootMap::decide` (:447-466) is the sole producer; `Engine::decide`/`decide_open` (:204-216/:222-249) wrap it. Consumers are two structurally identical match blocks in `hook.rs` (`create_hook` :1487-1577, `open_hook` :1744-1815+).

**`zipserve` is two unrelated things and the spec originally got this wrong.** Delete only the **zip-window serving** half — `open_synth`, `ensure_mapped`/`map_container`, `ZIP_MAPS`/`SynthFile`, `read`/`size`/`position`/`set_position`/`close`, `copy_window_to_file`. **Retain** the **synthetic-section** half — `SynthSection`, `register_mapped_image`, `create_section`, `map_view`, `unmap_view`, `has_view_in`, `is_synth_view` — which backs `fuse_create_section`'s `SEC_IMAGE` path (`hook.rs:2904-2939`) and all of `lazy_section.rs` (:206, 500, 510, 606, 630). That machinery is live.

The `is_synth`-family checks threaded through ordinary handle-lifecycle hooks (close :2137-2143, seek/tell :2293-2300/:2464-2467, read :2852-2864/:3072-3106) are **not** zip-specific. Do not delete them blindly.

- [ ] **Step 1: Delete, compiler-driven**

Remove the variants and let the compiler find every consumer. `Decision::Serve`'s arms are already dead (both early-return when the FUSE client is installed, which bootstrap guarantees) — deleting them should change no behaviour, and if a test fails, that is a finding worth reporting rather than a test to adjust.

- [ ] **Step 2: Verify nothing live was cut**

`cargo build --all-targets` plus `-p vfs-shim`, `-p vfs-redirect`, `-p vfs-director`. Confirm `lazy_section.rs` still compiles and its tests pass — that is the load-bearing consumer of the half you kept.

- [ ] **Step 3: Report the line count removed**, and confirm the retained half has no remaining reference to zip windows.

- [ ] **Step 4: Commit**

---

### Task 8: Write canaries in the escape matrix

**Files:** `crates/vfs-fixture-escape/src/main.rs`, `crates/vfs-directord/tests/e2e.rs`

**Interfaces:** The 14-spelling matrix runs for **write** access, not only read and metadata.

Spec §8 criterion 1: "Canary matrix green for read, write, metadata, and enumeration access: 14 spellings × 2 canaries… **A write to the negative canary must be blocked, and must not create a file on the real filesystem under the root.**"

That second clause is the whole point: a write that is refused but still creates a zero-byte file on real disk has breached containment while reporting success at the API. **Assert the filesystem state, not just the returned status.**

Unbuildable vectors must be reported as unbuildable, **never silently skipped** — a skipped containment test that reads as a pass is how this property rots.

Also decide the fate of the two unwired fixtures, `vfs-fixture-writeset` and `vfs-fixture-write`: no test harness invokes either. Wire one up if it serves this task, delete them otherwise, and say which you did and why. Leaving dead fixtures in the workspace is how the next person concludes coverage exists that does not.

- [ ] **Step 1: Extend the fixture for write access**

- [ ] **Step 2: Run to verify** the negative canary is blocked for all buildable spellings, and check real-disk state after each attempt.

- [ ] **Step 3: Verify the positive canary still succeeds** for every spelling. The cheap way to pass a containment test is to break legitimate access; this is what catches that.

- [ ] **Step 4: Commit**

---

### Task 9: A live session that writes its INI and its save

**Files:** `crates/vfs-directord/src/bin/skyrim-live.rs`, `tools/gamectl.ps1`, `rust/docs/bypass-baseline.md`

**Interfaces:** No new API. This is the acceptance evidence for spec §8 criterion 6.

**The blocker to clear first.** Stage 2b's session never reached gameplay: an Anniversary Edition "Thanks for buying / DOWNLOAD" modal held the main menu and could not resolve (Steam is in offline mode). Input was confirmed reaching the game — the cursor moved — so this is a **content problem, not an input problem**. Fix it by changing what the game loads or what its profile contains; do **not** script input to click past it. The profiles directory is re-seeded each run, so the game re-shows its one-time prompt every session.

**The headline question:** does the game's own save now route through the director? Stage 2b established that the My Games root routes for reads and metadata (4393 routed opens, zero `outside-root`, reconciliation exact) but **no `.ess` was ever written**, so this is unanswered.

**`FellThroughWriteFallback = 0` is not evidence unless a write actually happened.** In stage 2b it read zero because no save occurred. Task 1's counters are what make this distinguishable: a routed-writes number greater than zero alongside a fall-through of zero is the result; both zero means the run proved nothing.

**Either answer is informative. Do not tune the run to get the pleasing one.**

- [ ] **Step 1: Fix the AE modal at the content level**, and state what you changed.

- [ ] **Step 2: Run a session that saves** — reach gameplay, save, quit cleanly.

- [ ] **Step 3: Record in `bypass-baseline.md`**, alongside the previous runs rather than replacing them: per-outcome counts, the reconciliation, the routed **write** ops and bytes from Task 1, and specifically whether the save routed.

Note write throughput if observable. There is **no write benchmark baseline** in `rust/docs/benchmarks/` — every recorded figure is read throughput — so this run is the only observation of what routing writes costs. Do not claim a regression or an improvement against numbers that do not exist.

- [ ] **Step 4: Full gate**

```powershell
cargo build --all-targets
cargo build --manifest-path crates/vfs-payload/Cargo.toml --target-dir target
cargo test --workspace
cargo clippy --all-targets -- -D warnings
```

- [ ] **Step 5: Commit**

---

## Gate 4 Exit Criteria

- [ ] Routed write ops and write bytes are printed by the director's report, and the existing field names and order are unchanged.
- [ ] A write under a managed root that the director cannot serve **fails**, and creates no file on the real filesystem under the root.
- [ ] A write through the director is visible to a subsequent read through the director and lands where the provider graph says, with the real path under the root asserted absent (spec §8 criterion 4).
- [ ] `FellThroughWriteFallback` is still wired, and reads **zero** in a live session in which routed writes are **non-zero**.
- [ ] Copy-up sources its bytes from the director, and does **not** seed from the real filesystem under a managed root.
- [ ] `Engine` is multi-root; the stage-2b recorded-gap test has been flipped to assert the closed behaviour.
- [ ] The overlay is root-aware, or the overlay write path is gone — whichever Task 6 established, with the finding recorded.
- [ ] `Decision::Redirect`/`Serve`/`Deny` are deleted; `zipserve`'s zip-window half is deleted and its synthetic-section half still compiles and passes `lazy_section` tests.
- [ ] The canary matrix is green for **write** access, 14 spellings × 2 canaries, with unbuildable vectors reported as unbuildable.
- [ ] Whether the game's save routes through the director is recorded either way, with routed-write counts that make a zero fall-through meaningful.
- [ ] Workspace at or above 499; clippy clean; payload workspace builds.
- [ ] **The four DRM filename exceptions still exist** — gate 5 owns them.

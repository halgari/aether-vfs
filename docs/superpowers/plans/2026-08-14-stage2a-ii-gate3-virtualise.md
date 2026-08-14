# Stage 2a-ii Gate 3: Virtualise the Roots — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the managed root fully virtual — the provider graph is the sole authority for what exists under it — and delete the legacy decision paths that let the real filesystem answer instead.

**Architecture:** The shim's five policy surfaces collapse to one predicate. `Decision` loses `Redirect`/`Serve`/`Deny`; `AttrDecision`, `query_attributes`, and `merge_directory` are deleted; `NotFound` and `Dir` under a root stop falling through to the real filesystem. Everything under a root is answered by the director or not at all.

**Tech Stack:** Rust 2021, Windows NT API, shared-memory ring IPC.

## Global Constraints

- Design spec: `docs/superpowers/specs/2026-08-13-no-bypass-and-real-roots-design.md` §3, §4, §7.
- Working directory for all cargo commands is `C:\oss\aether-vfs\rust`.
- Baseline: `cargo test --workspace` = 459 passed, 0 failed. Never lower it.
- Zero clippy warnings: `cargo clippy --all-targets -- -D warnings`.
- `vfs-payload` is a separate workspace: `cargo build --manifest-path crates/vfs-payload/Cargo.toml --target-dir target`.
- Conventional commit prefixes. Commit after every task.
- A stray `vfs.exe` daemon can lock the build so `cargo test` yields **no result at all** rather than a failure. Check for one before concluding a run failed.
- Two tests are known flaky under parallel contention: `vfs-inject::injected_shim_passes_full_acceptance_suite`, `vfs-shim::writes_land_in_overlay_with_cow`. Both pass standalone.

### What this gate does and does not remove

**Removes:** standalone (no-director) shim mode, `AttrDecision`, `query_attributes`, `merge_directory`, and `NotFound`/`Dir` → passthrough under a root.

**Does NOT remove, contrary to this plan's first draft:** `Decision::Redirect`, `Serve`, `Deny`, and the legacy `zipserve` path. `Engine::cow_seed` depends on `Redirect` and `Serve` **at compile time** to materialise copy-on-write content for the overlay write path — the write fall-through **gate 4** owns. Their deletion moves to gate 4, after `cow_seed` sources copy-up from the director. The first draft had this backwards.

**Keeps — later gates own these, and removing one here makes a failure un-attributable:**
- The **write fall-through** (gate 4). Writes still fall back; the director cannot yet serve every write.
- The **DRM filename exceptions** (gate 5). `steam_appid.txt`, `SkyrimSELauncher.exe`, `steam_api*.dll`, `SkyrimSE.exe` still trampoline.

### The ordering constraint that governs this plan

Making the root fully virtual means a real file under it that no provider serves **becomes invisible**. The session stages the game executable and its DLLs into the root in order to launch. Delete the passthrough before the provider graph serves those, and **nothing launches** — and the failure will look like the shim broke rather than like a configuration gap.

So Tasks 1 and 2 must land before Task 5. Do not reorder.

---

### Task 1: Serve the staged files from the provider graph

**Files:** `crates/vfs-director/src/session.rs`, `crates/vfs-director/src/stage.rs`, `crates/vfs-directord/src/registry.rs`

**Interfaces:** Produces a session whose provider graph resolves every file the launch path stages into the managed root.

**Why first:** this is the prerequisite that stops Task 5 from breaking the launch.

- [ ] **Step 1: Establish what is staged and whether a provider serves it**

Read `stage.rs` and `session.rs` and enumerate every path the launch writes under the managed root — the staged EXE, `vfs_shim_dll.dll`, `vfs_payload.dll`, anything else. For each, determine whether the current provider graph resolves it. **Write that list into your report before changing anything**; it is the input to the rest of the task and to Task 5's risk.

- [ ] **Step 2: Write the failing test**

A test asserting the director resolves each staged artifact by its under-root path — `getattr` returns `Some`, and `open` succeeds. It must fail today for anything currently reachable only via passthrough.

- [ ] **Step 3: Run to verify it fails**

- [ ] **Step 4: Implement**

Mount a provider covering the staged files. A `disk` provider over the staging directory is the obvious shape and matches the spec's decision ("want the real directory's contents? mount a `disk` provider"). Compose it so the game's own content still wins where both could serve a path — check the existing layering order rather than assuming.

- [ ] **Step 5: Run to verify it passes**, then `cargo test --workspace` and clippy.

- [ ] **Step 6: Commit**

---

### Task 2: A failed FUSE init aborts the launch

**Files:** `crates/vfs-shim/src/hook.rs`, `crates/vfs-shim/src/fuse_client.rs`, `crates/vfs-director/src/session.rs`

**Interfaces:** Produces a launch that fails loudly when the shim's FUSE client cannot initialise.

**Why this is the largest bypass in the system.** `fuse_client::global()` returning `None` currently means every path passes through — the game runs **completely un-virtualized**, and nothing reports it. It is silent, total, and indistinguishable from success. Once the root is fully virtual it also becomes fatal in a confusing way, so it must fail on its own terms first.

- [ ] **Step 1: Write the failing test**

Force FUSE init to fail (an env switch is acceptable if one exists or you add a test-only one) and assert the launch returns an error rather than running the process. Assert on the *outcome*, not a log line.

- [ ] **Step 2: Run to verify it fails** — today the process launches successfully and un-virtualized.

- [ ] **Step 3: Implement**

Report the failure through the launch path so a caller sees it. Do not merely log; a log nobody reads is what made this invisible.

- [ ] **Step 4: Run to verify it passes**, then `cargo test --workspace` and clippy.

- [ ] **Step 5: Commit**

---

### Task 3: Retire standalone mode

**Files:** `crates/vfs-shim/src/bootstrap.rs`, `crates/vfs-shim/src/fuse_client.rs`, and the four standalone tests

**Interfaces:** `FuseInitError::NotConfigured` stops being a legitimate outcome. A launch with no ring section fails exactly as a failed attach does.

**This task replaces the original Task 3, which was wrong.** That task assumed `Decision::Redirect`/`Serve`/`Deny` were unreachable and could be deleted. They are not: with `VFS_RING_SECTION` unset the shim initialises `NotConfigured`, `try_fuse_create` returns `None` unconditionally, and those arms run. Four tests exercise them through real installed hooks — `hook_redirect.rs`, `hook_deny.rs`, `zip_serve_inproc.rs`, `hook_write.rs`.

**And deleting the variants is still not this gate's job**, even after standalone is retired. `Engine::cow_seed` depends on `Redirect` and `Serve` **at compile time** for the overlay write path — gate 4's write fall-through. Their deletion moves to gate 4. Do not attempt it here.

- [ ] **Step 1: Write the failing test**

A launch with no `VFS_RING_SECTION` must return an error rather than running an un-virtualized process. Task 2 built the machinery for the `ConnectFailed` case; this extends it to `NotConfigured`. Assert the outcome, not a log line.

- [ ] **Step 2: Run to verify it fails** — today the launch succeeds and the process runs un-virtualized.

- [ ] **Step 3: Implement**

`bootstrap.rs` matches `NotConfigured` separately and swallows it. Make it fail like any other init failure.

- [ ] **Step 4: Deal with the four standalone tests**

They install hooks with no director and exercise the legacy arms — they test a retired mode.

**Do not simply delete them.** For each, decide and record: does it assert behaviour that still matters under a director, in which case port it; or does it only assert the retired path, in which case remove it and say so. A test deleted without that judgement is coverage lost silently. State the disposition of each in your report.

- [ ] **Step 5: Verify** — `cargo test --workspace`, clippy, and the escape-matrix e2e must still pass. If the test count drops, name every test removed and why.

- [ ] **Step 6: Commit**

---

### Task 4: Delete `AttrDecision`, `query_attributes`, and `merge_directory`

**Files:** `crates/vfs-redirect/src/lib.rs`, `crates/vfs-shim/src/hook.rs`

**Interfaces:** Metadata queries and directory enumeration route to the director instead of being answered locally or merged with real directory contents.

`merge_directory` merges a real directory's entries with the snapshot's. With a fully virtual root there is nothing to merge — the director's `readdir` is authoritative. This is the change most likely to alter what the game *sees*, so verify enumeration parity deliberately.

- [ ] **Step 1: Write the failing test**

Assert that a directory listing under the root contains exactly what the provider graph says and **nothing** that exists only on the real filesystem. Today a real-only file appears via the merge.

- [ ] **Step 2: Run to verify it fails**

- [ ] **Step 3: Implement** — route the attribute-query hooks and the enumeration hooks to the director; delete the local answering paths.

- [ ] **Step 4: Verify** — the existing enumeration tests (`vfs-shim/tests/hook_direnum.rs`, `hook_enum_parity.rs`) must still pass or be updated only where they asserted merged behaviour. **A test asserting real-file-visible-under-root is now asserting the old semantics; changing it is correct, but say so explicitly in your report.**

- [ ] **Step 5: Commit**

---

### Task 5: The root becomes fully virtual

**Files:** `crates/vfs-redirect/src/lib.rs`, `crates/vfs-shim/src/hook.rs`

**Interfaces:** `NotFound` under a root returns not-found. `Dir` under a root returns a director-served handle. Neither falls through.

**This is the behavioural change the gate exists for**, and the point where the negative canary flips from *classified* to *unreachable*.

- [ ] **Step 1: Write the failing test**

A real file on disk under the managed root that no provider serves must be **unreachable** — `open` returns not-found. Today it opens.

- [ ] **Step 2: Run to verify it fails**

- [ ] **Step 3: Implement**

- [ ] **Step 4: Handle the predicted Mod Organizer consequence**

A junction *inside* the managed root pointing at external staging content is a common real-world Skyrim setup. It is currently reachable through the under-root passthrough this task removes, so **removing it will seal that content**. This was predicted in gate 2's review; it is not a surprise and must not be discovered by a user.

Determine what actually happens to such a junction now, document it in `rust/docs/escape-matrix.md`, and state the required configuration — mounting the staging directory as a provider. If it can be made to work without configuration, say how; if it cannot, say that plainly.

- [ ] **Step 5: Verify** — `cargo test --workspace`, clippy, and a real Skyrim launch via `tools/gamectl.ps1` showing the expected load order. **If the launch breaks, stop and report** rather than reinstating the passthrough.

- [ ] **Step 6: Commit**

---

### Task 6: Update the matrix and the baseline

**Files:** `rust/docs/escape-matrix.md`, `rust/docs/bypass-baseline.md`, `crates/vfs-directord/tests/e2e.rs`

- [ ] **Step 1: Flip the negative canary's assertion**

The escape-matrix e2e currently asserts the negative canary is *classified under-root*. It must now assert **unreachable** — every buildable spelling returns not-found. The positive canary must still open byte-identically.

- [ ] **Step 2: Re-run and record**

Update `escape-matrix.md` with the new outcomes, and correct its standing caveat that it establishes classification rather than containment — after this gate it establishes containment for reads, metadata, and enumeration, with writes still open until gate 4.

- [ ] **Step 3: Re-measure the outcome counters**

`FellThroughRedirect`, `FellThroughServe`, and `FellThroughPassthrough` should now be **zero**. `FellThroughWriteFallback` and `FellThroughDrmException` remain non-zero — gates 4 and 5. Record the new numbers in `bypass-baseline.md` alongside the originals, and say which gate closes each remaining non-zero class.

- [ ] **Step 4: Full gate**

```powershell
cargo build --all-targets
cargo build --manifest-path crates/vfs-payload/Cargo.toml --target-dir target
cargo test --workspace
cargo clippy --all-targets -- -D warnings
```

- [ ] **Step 5: Commit**

---

## Gate 3 Exit Criteria

- [ ] Every staged artifact is served by the provider graph; the game launches.
- [ ] A failed FUSE init aborts the launch, with a test asserting it.
- [ ] `Decision::Redirect`, `Serve`, `Deny`, `AttrDecision`, `query_attributes`, `merge_directory`, and the legacy `zipserve` path are gone.
- [ ] A real file under the root that no provider serves is unreachable by every buildable spelling.
- [ ] A directory listing under the root shows exactly the provider graph's contents and nothing real-only.
- [ ] `FellThroughRedirect`, `FellThroughServe`, `FellThroughPassthrough` are zero; the reconciliation invariant still holds.
- [ ] Skyrim launches with the expected load order.
- [ ] The Mod Organizer consequence is documented with its required configuration.
- [ ] Workspace at or above 459; clippy clean; payload workspace builds.
- [ ] **The write fall-through and the DRM exceptions still exist** — gates 4 and 5 own them.

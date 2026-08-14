# Stage 2a-ii Gate 1: Measure the Bypass — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the remaining bypass from invisible into enumerated, changing no behaviour, so gates 2-5 start from data rather than hope.

**Architecture:** The shim classifies every under-root open by which path it actually took — routed to the director, or one of the specific fall-throughs — and reports the counts and the paths. The director exposes its own open counts. A reconciliation compares the two: any drift is a bypass, by definition.

**Tech Stack:** Rust 2021, Windows NT API, shared-memory ring IPC, gRPC control plane.

## Global Constraints

- Design spec: `docs/superpowers/specs/2026-08-13-no-bypass-and-real-roots-design.md` §7, "2a-ii runs in five gates".
- Working directory for all cargo commands is `C:\oss\aether-vfs\rust`.
- **THIS GATE CHANGES NO BEHAVIOUR.** Not one byte of I/O may route differently afterwards. Do not delete `Decision::Redirect`/`Serve`, the DRM filename exceptions, the write fall-through, or any passthrough. Do not "fix" a bypass you discover — **record it**. That is the entire point: gates 2-5 remove them one class at a time so a failure is attributable, and a fix smuggled in here destroys that.
- Zero clippy warnings: `cargo clippy --all-targets -- -D warnings`.
- `vfs-payload` is a separate workspace: `cargo build --manifest-path crates/vfs-payload/Cargo.toml --target-dir target`.
- Baseline: `cargo test --workspace` = 395 passed, 0 failed, 1 ignored. Never lower it.
- Instrumentation must be **free when disabled.** `hookstats::enabled()` gates on `VFS_SHIM_STATS_LOG`; the existing `Timed` guard already avoids reading a clock when disabled, and its test asserts that. New counters must follow the same discipline — a game launch with stats off must not pay for them.
- Opcode numbering and ring payload layout are a contract with the injected DLL. **Do not add a ring opcode in this gate.** The shim already emits a report file; use it.
- Conventional commit prefixes. Commit after every task.

---

## File Structure

| File | Change |
|---|---|
| `crates/vfs-shim/src/hookstats.rs` | Outcome classification counters, per-outcome path lists, report rendering |
| `crates/vfs-shim/src/hook.rs` | Call the classifier at each under-root open outcome site |
| `crates/vfs-director/src/io_stats.rs` | Expose open totals for reconciliation |
| `crates/vfs-directord/src/service.rs` | Surface counts in the gRPC `StatsResp` |
| `crates/vfs-directord/proto/*.proto` | Extend `StatsResp` (control plane, not the ring — safe to extend) |
| `crates/vfs-directord/tests/e2e.rs` | Assert reconciliation in the existing write tests |
| `rust/docs/bypass-baseline.md` | **new** — the recorded before-picture |

---

### Task 1: Classify every under-root open outcome

**Files:** Modify `crates/vfs-shim/src/hookstats.rs`

**Interfaces:**
- Produces: `pub enum OpenOutcome { Routed, FellThroughRedirect, FellThroughServe, FellThroughPassthrough, FellThroughDrmException, FellThroughWriteFallback, Denied }` and `pub fn note_open_outcome(outcome: OpenOutcome, path: &str)`, plus a `render_outcomes()` added to the reporter body.

**Why the outcomes are named this specifically:** gates 2-5 each remove one class. `FellThroughRedirect` and `FellThroughServe` are gate 3's, `FellThroughPassthrough` is gates 2 and 3, `FellThroughWriteFallback` is gate 4, and `FellThroughDrmException` is gate 5. A single "fell through" counter would tell you a gate regressed but not which one — the whole reason for this gate.

- [ ] **Step 1: Write the failing test**

Add to `hookstats.rs`'s test module:

```rust
    #[test]
    fn outcome_counters_are_free_when_disabled() {
        // VFS_SHIM_STATS_LOG is unset under test, so `enabled()` is false and
        // recording must not touch the counters at all.
        let before = outcome_count(OpenOutcome::Routed);
        note_open_outcome(OpenOutcome::Routed, "a.esp");
        assert_eq!(outcome_count(OpenOutcome::Routed), before);
    }

    #[test]
    fn every_outcome_renders_with_a_distinct_label() {
        // A gate that removes one bypass class must be able to see that class
        // alone; identical or missing labels would defeat that.
        let mut labels: Vec<&str> = ALL_OUTCOMES.iter().map(|o| o.label()).collect();
        let n = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), n, "outcome labels must be distinct: {labels:?}");
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p vfs-shim --lib hookstats`
Expected: compile error — `OpenOutcome`, `note_open_outcome`, `outcome_count`, `ALL_OUTCOMES` are not defined.

- [ ] **Step 3: Implement**

Follow the file's existing conventions exactly — `AtomicU64` arrays indexed by a `#[repr(usize)]` enum, an `enabled()` guard at the top of every recorder, and a bounded path map like the one behind `note_passthrough`. Cap the per-outcome path lists the same way the existing path maps are capped, and make the cap visible in the rendered output (`… and N more`) so a truncated list is never mistaken for a complete one.

`render_outcomes()` emits a section with one line per outcome: label, count, and the top paths. Add it to the `format!` chain inside `start_reporter`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p vfs-shim`
Expected: all pass including both new tests.

Run: `cargo clippy -p vfs-shim --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add rust/crates/vfs-shim/src/hookstats.rs
git commit -m "feat(shim): classify under-root open outcomes by which path they took"
```

---

### Task 2: Call the classifier at every outcome site

**Files:** Modify `crates/vfs-shim/src/hook.rs`

**Interfaces:** Consumes Task 1's `note_open_outcome`.

**The work is finding every site, not writing the calls.** Read `create_hook`'s full decision flow. Every path an under-root open can take must record exactly one outcome:
- `try_fuse_create` returning `Some(status)` → `Routed`
- the DRM filename early-returns (`steam_appid.txt`, `SkyrimSELauncher.exe`, `steam_api*.dll`, `SkyrimSE.exe`) → `FellThroughDrmException`
- `Decision::Redirect` → `FellThroughRedirect`
- `Decision::Serve` → `FellThroughServe`
- `Decision::Deny` → `Denied`
- `Decision::PassThrough` where the path **is** under a managed root → `FellThroughPassthrough`
- the write fall-through (`Err(_) if write => None`) → `FellThroughWriteFallback`

A path **outside** every managed root is not an under-root open and must record nothing — counting those would drown the signal in every `kernel32.dll` load the process makes.

**Exactly one outcome per open.** Double-counting inflates the fall-through numbers gates 2-5 are measured against; missing one hides a bypass. State in your report how you convinced yourself each open records exactly once.

- [ ] **Step 1: Write the failing test**

Extend `crates/vfs-directord/tests/e2e.rs`'s existing single-source write test to read the shim report file after the process exits and assert that the `routed` outcome count is greater than zero — proving the classifier is actually wired and firing, not merely compiled.

The harness must set `VFS_SHIM_STATS_LOG` for the child. Check how `LaunchOpts.env` is threaded in the existing tests rather than inventing a mechanism.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p vfs-directord --test e2e`
Expected: FAIL — no outcome section in the report.

- [ ] **Step 3: Implement**

Add the calls. Where a site is ambiguous — an open that could plausibly be classified two ways — pick one, comment the reasoning, and say so in your report rather than guessing silently.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p vfs-directord --test e2e` then `cargo test --workspace`
Expected: all pass; workspace at or above 395.

Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add rust/crates/vfs-shim/src/hook.rs rust/crates/vfs-directord/tests/e2e.rs
git commit -m "feat(shim): record an outcome at every under-root open site"
```

---

### Task 3: Expose director-side open counts

**Files:** Modify `crates/vfs-director/src/io_stats.rs`, `crates/vfs-directord/src/service.rs`, and the `vfs-directord` proto

**Interfaces:**
- Produces: `io_stats::open_totals() -> (u64, u64)` returning `(ok, err)`; `StatsResp` gains `opens_ok`, `opens_err`, and the rejected-write list.

The control-plane proto is **not** the ring protocol — extending `StatsResp` is safe and does not touch the injected DLL's contract. Add fields, do not renumber.

`io_stats::record_open` already exists and is called from `ring_dispatch`. You are exposing what it already counts, plus `rejected_writes()` which Task 4 of the previous phase added.

- [ ] **Step 1: Write the failing test**

Add a `vfs-director` unit test asserting `open_totals()` increments on a successful open and on a failed one, distinguishing the two. Add a `vfs-directord` test asserting the gRPC `stats` response carries a non-zero `opens_ok` after a session has served an open.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p vfs-director -p vfs-directord`
Expected: FAIL — `open_totals` undefined, `StatsResp` has no such field.

- [ ] **Step 3: Implement**

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --workspace` and `cargo clippy --all-targets -- -D warnings`
Expected: green, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add rust/crates/vfs-director/src/io_stats.rs rust/crates/vfs-directord
git commit -m "feat(directord): expose open counts and rejected writes in stats"
```

---

### Task 4: The reconciliation

**Files:** Modify `crates/vfs-directord/tests/e2e.rs`; create a shared helper alongside it

**Interfaces:**
- Produces: `assert_reconciled(shim_report: &Path, opens_ok: u64) -> Reconciliation`, returning the parsed outcome counts plus the drift, and panicking with a message naming the drift when the routed count does not equal the director's open count.

**The invariant:** `routed` (shim) == `opens_ok` (director). Any drift means an open the shim believed it routed never arrived — a bypass by definition.

**What this gate does NOT assert:** that fall-through counts are zero. They are not zero yet, and asserting so would fail. Record them; gates 2-5 drive them down one class at a time.

- [ ] **Step 1: Write the failing test**

Apply `assert_reconciled` to **both** existing e2e write tests (single-source and two-source). Also assert the fall-through counts are *recorded* — that the section exists and parses — rather than that they are zero.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p vfs-directord --test e2e`
Expected: FAIL — helper undefined.

**If it fails because the counts genuinely do not reconcile, STOP and report.** That is a live bypass in a path the e2e tests already exercise, and it is a finding, not something to adjust the assertion around.

- [ ] **Step 3: Implement**

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --workspace` and `cargo clippy --all-targets -- -D warnings`

- [ ] **Step 5: Commit**

```bash
git add rust/crates/vfs-directord/tests
git commit -m "test(e2e): reconcile shim-routed opens against director opens"
```

---

### Task 5: Record the baseline against a real game

**Files:** Create `rust/docs/bypass-baseline.md`; modify `tools/gamectl.ps1`

**Interfaces:** No code interfaces. This task produces **the document gates 2-5 are measured against.**

`tools/gamectl.ps1` is the existing harness for driving the game; it already handles the DPI, foreground-steal, and lock-screen problems that silently break capture and input. Extend it to set `VFS_SHIM_STATS_LOG`, and after the run, dump the outcome section and the director's stats side by side.

- [ ] **Step 1: Extend the harness**

- [ ] **Step 2: Run the game and capture the numbers**

Launch Skyrim under a composed session via `tools/gamectl.ps1`, reach the main menu and load a save if possible, exit, and capture the report.

- [ ] **Step 3: Write the baseline document**

`rust/docs/bypass-baseline.md` records, for that run: total under-root opens, the count per outcome, the top paths per fall-through outcome, the director's `opens_ok`/`opens_err`, the reconciliation drift, and any rejected writes. State the game build, the session config, and the date.

For each fall-through outcome, name **which gate will close it** and what is expected to break — that turns the baseline into gates 2-5's test plan.

- [ ] **Step 4: If the game cannot be driven in this environment**

Say so plainly in the document and in your report — which step failed and why (DPI, foreground, lock screen, missing game, missing save). **Do not fabricate numbers, and do not substitute a fixture run and present it as a game run.** A fixture-derived baseline is still useful; label it as such, prominently, and record what is missing. A baseline nobody can trust is worse than an admitted gap, because gates 2-5 would be measured against fiction.

- [ ] **Step 5: Commit**

```bash
git add rust/docs/bypass-baseline.md tools/gamectl.ps1
git commit -m "docs: record the bypass baseline gates 2-5 are measured against"
```

---

## Gate 1 Exit Criteria

- [ ] Every under-root open records exactly one outcome, and the outcomes distinguish each bypass class that gates 2-5 will remove separately.
- [ ] Instrumentation is free when `VFS_SHIM_STATS_LOG` is unset, with a test asserting it.
- [ ] The reconciliation invariant (`routed` == director `opens_ok`) is asserted in both e2e write tests.
- [ ] `rust/docs/bypass-baseline.md` exists, and is honest about whether it came from a real game run or a fixture.
- [ ] `cargo test --workspace` at or above 395; clippy clean; payload workspace builds.
- [ ] **No behaviour changed.** No bypass removed, no decision path deleted, no DRM exception touched.

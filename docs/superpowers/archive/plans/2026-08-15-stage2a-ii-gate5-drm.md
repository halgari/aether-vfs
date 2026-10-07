# Gate 5: Close the DRM Exceptions — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the last four paths by which a file under a managed root reaches the real filesystem, and close the escapes that survived alongside them. After this gate the invariant holds without exception: *for any path under a managed root, every NT operation on it is answered by the director.*

**Architecture:** Four host-tree filenames (`steam_appid.txt`, `SkyrimSELauncher.exe`, `steam_api{,64}.dll`, `SkyrimSE.exe`) are matched by **basename, at any depth**, and return `None` from `try_fuse_create` before the ring is consulted. That single decision keeps `Decision::Redirect`/`Deny`, `Engine::cow_seed`, and the shim-local overlay write path alive, and it is the enabling condition for an unbounded-recursion hazard. Closing it collapses all of that.

**Tech Stack:** Rust 2021, Windows NT API, shared-memory ring IPC, gRPC control plane.

## Global Constraints

- Design spec: `docs/superpowers/specs/2026-08-13-no-bypass-and-real-roots-design.md` §6 (the exceptions and the risk) **including the three 2026-08-15 corrections appended to it**, and §8 (acceptance criteria).
- Working directory for all cargo commands is `C:\oss\aether-vfs\rust`.
- Baseline: `cargo test --workspace` = **583 passed, 0 failed, 1 ignored**. Never lower it **except** for the three tests named in Task 6, which are designed to die here.
- Zero clippy warnings: `cargo clippy --all-targets -- -D warnings`.
- `vfs-payload` is a separate workspace: `cargo build --manifest-path crates/vfs-payload/Cargo.toml --target-dir target`.
- **Build first, then test.** `cargo test --workspace` relinks concurrently with running tests and can lock `vfs_shim_dll.dll` mid-run.
- **Run injected test binaries one at a time** while the shim tree is dirty; `ensure_inject_artifacts` relinks the DLL whenever shim sources are newer and will replace it under a running binary. Prebuilding does not help.
- **`tools\gamectl.ps1 -Action launch` runs the RELEASE binary.** Rebuild release and confirm the file sizes change before trusting any live result. They have been stale three times in this project.
- Inert zombie `vfs-fixture-escape.exe` processes may exist holding old renamed artifacts; ignore them, but note the shutdown defect in Task 8.
- **The full suite exceeds ten minutes.** Scope your runs and state exactly what you ran.
- Conventional commit prefixes. Commit after every task.

### The risk this gate carries, stated plainly

The spec's hypothesis is that serving these files through the director previously produced "Steam Error" because of *an open that failed to resolve*, **not** an integrity check. The code comment warns against the competing theory: *"Steam does NOT compare the in-memory image against the on-disk PE. Do not 'fix' anything here on the theory that the mapped image must match disk."*

**That hypothesis may be wrong.** If a real launch fails with the exceptions closed, the spec's contingency governs and it is not optional: **stop, report the diagnosis, and do not reintroduce a bypass to make the gate green.** A gate declared done with a game that does not launch is worse than an honest failure — this stage has spent four gates establishing that claims must match reality.

### Facts established by survey, so you do not rediscover them

1. **The exceptions are at `hook.rs:1195-1239`**, matched on **basename only**, case-insensitively, at any depth. `steam_appid.txt` and `SkyrimSELauncher.exe` are unconditional; `steam_api{,64}.dll` is gated on `keep_host_steam_api()` which **defaults true**; `SkyrimSE.exe` on `fuse_skyrim_exe()` which **defaults false** (so that bypass is on by default).
2. **FUSE-relative OA resolution already works.** `tramp_create_abs` builds a fresh absolute OA and never hands a synthetic handle to the kernel; `parent_dir_of_handle` case 1 resolves a synthetic `RootDirectory` via `PATH_TABLE`. The spec's prescribed fix may already exist — Task 1 tests that before anything is built.
3. **`drm_exe_trace` is inert for the case it exists to explain.** It only sees opens through `try_fuse_create`, and a launch-to-menu never opens `SkyrimSE.exe` there.
4. **`skyrim-live` deliberately does not mount the Steam library**, because doing so let the game load masters/BSAs/DLLs from the host install.
5. **Three tests are designed to fail here** and say so in their own doc comments: `cow_seed_reporting.rs`, `overlay_failure_reporting.rs`, `cow_seed_reentrancy.rs`.

---

### Task 1: Find out whether the exceptions can just be closed

**Files:** none — this task changes nothing. It produces a finding.

**Interfaces:** none.

**Why this is first.** The spec prescribes building FUSE-relative OA resolution so these paths can route. That resolution now exists (fact 2). So the blocker may be gone, and the rest of this plan may be much smaller than it looks. **Building the enablement before checking whether it is needed is how a gate spends a week on a solved problem.**

- [ ] **Step 1: Turn the exceptions off and run the suite**

Flip `keep_host_steam_api()` to false and `fuse_skyrim_exe()` to true — the two flags already invert two of the four — and make the two unconditional ones conditional on the same switch. Do this behind a temporary env flag, not by deleting code.

- [ ] **Step 2: Run the injected e2e suite** one binary at a time, and report which tests fail and how.

- [ ] **Step 3: Report the finding**, and stop.

State: does anything break with the exceptions closed, and if so, is the failure an unresolved open (the spec's hypothesis) or something else? **Do not proceed to a fix.** The next task's shape depends entirely on this answer, and I would rather re-plan than have you guess.

---

### Tasks 2 and 3: RESOLVED BY TASK 1 — do not execute

**Task 2 (widen `drm_exe_trace`) is folded into Task 9.** The tracer exists to diagnose a live failure. Task 1 showed the exceptions close cleanly at the shim level — 28/28 injected e2e including the escape matrix — so the contingency is now unlikely to fire. If the live run *does* fail, Task 9 widens the tracer then, as diagnosis rather than speculative preparation. Note Task 1's correction: closing the exceptions may make the tracer start seeing `SkyrimSE.exe` opens on its own, since staged-dir opens now reach the director.

**Task 3 (narrow mount) is unnecessary. The four files are already reachable.** Task 1 established it: `skyrim-live.rs:587-599` already mounts both the runtime root and `staged_dir_path` as root-0 disk layers, and `write_steam_appid` (`:867`) places `steam_appid.txt` in the overlay. Nothing needs mounting, and the conflict with `skyrim-live.rs:222-224`'s deliberate refusal to mount the Steam library never has to be resolved — we are not mounting it.

**Why the spec's blocker evaporated, recorded because it is the useful part.** FUSE-relative `OBJECT_ATTRIBUTES` resolution is needed only to keep the exceptions *open*: `tramp_create_abs` exists precisely because an excepted open must reach the kernel carrying an OA whose root is a handle the kernel never issued. With the exceptions closed, `try_fuse_create` returns a synthetic handle or seals the path — the kernel is never called and the original OA is never passed on. The `STATUS_OBJECT_NAME_NOT_FOUND` the spec blames sits on the PassThrough arm, **which is the exception's own arm**. The blocker was self-inflicted by the thing being removed.

---

### Task 4: Close the exceptions

**Files:** `crates/vfs-shim/src/hook.rs`, `crates/vfs-env/src/lib.rs`

**Interfaces:** `try_fuse_create` has no filename-based early return. `keep_host_steam_api` and `fuse_skyrim_exe` are gone.

Delete the block at `hook.rs:1195-1239` and both env flags. Keep `hookstats::OpenOutcome::FellThroughDrmException` **wired and reading zero** — a removed counter cannot prove the class stayed closed, and the acceptance criteria depend on the reconciliation.

- [ ] **Step 1: Write the failing test** — an open of each of the four names under a managed root is answered by the director, and **creates no access to the real file**. Assert the filesystem side, not just the status.

- [ ] **Step 2: Run to verify it fails**

- [ ] **Step 3: Implement**

- [ ] **Step 4: Verify** the escape matrix passes and `FellThroughDrmException` reads zero.

- [ ] **Step 5: Commit**

---

### Task 5: Hook `NtDeleteFile` and close the cross-boundary rename

**Files:** `crates/vfs-shim/src/hook.rs`

**Interfaces:** A delete or rename that names a path under a managed root is answered by the director or fails; neither reaches real disk.

**`NtDeleteFile` is the highest-priority item this gate inherits.** It is unhooked and **path-based** — it takes only an `OBJECT_ATTRIBUTES`, no handle — so an unhooked call reaches real disk directly. Every other unhooked API on the list fails safely; this one does not.

**Rename into a managed root from outside** also still bypasses everything: `Engine::rename` refuses only when *both* sides resolve under managed roots, and a source outside every root is never recorded in `PATH_TABLE`, so `setinfo_hook`'s engine branch is skipped and the real `NtSetInformationFile` runs — physically creating a file under the destination root.

- [ ] **Step 1: Write the failing tests** — a `NtDeleteFile` of a path under a managed root does not delete the real file; a rename from outside into a managed root fails rather than landing.

- [ ] **Step 2: Run to verify they fail**

- [ ] **Step 3: Implement**

- [ ] **Step 4: Commit**

---

### Task 6: Collapse what the exceptions were keeping alive

**Files:** `crates/vfs-redirect/src/lib.rs`, `crates/vfs-shim/src/{hook,engine,overlay}.rs`, and the three tests below

**Interfaces:** `Decision` collapses to under-root or not. `Engine::cow_seed` and the shim-local overlay write path go if nothing reaches them.

With Task 4 landed, `try_fuse_create` returns `None` only when there is no FUSE client or the path is outside every root. **Establish what that leaves reachable, then delete what is not** — the same investigate-then-act shape that stopped an earlier gate deleting live machinery.

**Three tests are designed to die here.** Their own doc comments say so — `cow_seed_reporting.rs:32-33`: *"Gate 5 owns those exceptions; when it closes them this test will fail, and the right answer then is that copy-up has no live callers left."* Same for `overlay_failure_reporting.rs` and `cow_seed_reentrancy.rs`. Removing them is correct **if** copy-up is genuinely dead; verify that rather than assuming it, and say what you verified.

Closing the exceptions also removes the enabling condition for the **unbounded recursion** (a DRM-named file plus an overlay nested under a managed root, reproduced as a stack overflow). Confirm it is gone rather than dormant.

- [ ] **Step 1: Determine reachability** and report before changing anything.

- [ ] **Step 2: Delete what is dead**, compiler-driven.

- [ ] **Step 3: Account for the test count** — say exactly which tests went and why each was tied to deleted behaviour.

- [ ] **Step 4: Commit**

---

### Task 7: The phantom whiteout marker

**Files:** `crates/vfs-shim/src/hook.rs`, `crates/vfs-shim/src/overlay.rs`

**Interfaces:** A shim-written whiteout does not appear as a file in a director listing.

The shim spells whiteouts `<name>.__vfs_wh__`; `vfs-compose` spells them `.wh.<name>`. The director's listing branch does not filter the shim's spelling — `hook.rs:3661-3673` documents this — so **a shim whiteout shows the game a phantom `<file>.__vfs_wh__` entry and hides nothing.** `Overlay::apply_to_listing` has filtering logic but it is dead on this path.

If Task 6 deletes the shim-local overlay write path entirely, this dissolves — check that first and skip the task if so, saying why.

- [ ] **Step 1: Write the failing test** — a shim whiteout is neither visible as an entry nor ineffective at hiding its target.

- [ ] **Step 2: Implement or dissolve**, and say which.

- [ ] **Step 3: Commit**

---

### Task 8: The `DllMain` shutdown stall

**Files:** `crates/vfs-shim/src/hookstats.rs`, `crates/vfs-shim/src/bootstrap.rs`

**Interfaces:** An injected process exits without leaving an unreapable zombie holding the shim DLL.

Injected processes intermittently hang at `DLL_PROCESS_DETACH`, leaving unreapable processes with `vfs_shim_dll.dll` still mapped — which then break subsequent builds with `os error 5` until the artifact is renamed aside. Three such zombies exist on this machine.

Mechanism: at detach every other thread is already terminated; a reporter thread killed mid-`fs::write` leaves the CRT heap lock held, and rendering must allocate. A 5 ms reporter tick means **any** injected exit can lose this race. An earlier gate reproduced it on demand and backed out the change that did so — which removed the trigger, not the defect.

Suggested direction: make the reporter stoppable and joined before detach, or its writes interruptible.

- [ ] **Step 1: Reproduce it deliberately** — an earlier gate did so with an exit flush; you need a reliable trigger before you can claim a fix.

- [ ] **Step 2: Implement**

- [ ] **Step 3: Verify** by running the previously-hanging shape repeatedly. State how many runs.

- [ ] **Step 4: Commit**

---

### Task 9: The live acceptance run

**Files:** `rust/docs/bypass-baseline.md`, `rust/docs/escape-matrix.md`

**Interfaces:** No new API. This is stage 2a's final acceptance evidence.

Launch, reach gameplay, save, quit — with **every** exception closed.

**The pairing that makes it meaningful**, established last gate: routed writes non-zero **and** every fall-through class zero. Last gate's save routed 2,488,141 bytes byte-exact with `FellThroughDrmException 16` as the only remaining class. **That 16 must now be 0.**

**If the game does not launch, the spec's contingency governs.** Stop. Report the tracer output and the diagnosis. Do not reintroduce a bypass to make the gate green. Record the failure in `bypass-baseline.md` as a finding — an honest "the hypothesis was wrong and here is the evidence" is a real result and closes stage 2a's open question either way.

- [ ] **Step 1: Rebuild release and verify the artifacts changed size.**

- [ ] **Step 2: Run the session.**

- [ ] **Step 3: Record in `bypass-baseline.md`** alongside the previous runs: per-outcome counts, the reconciliation, routed write ops and bytes, and whether any fall-through class is non-zero.

- [ ] **Step 4: Update `escape-matrix.md`** — it currently records the DRM exceptions and `Decision::Redirect`/`Deny` as present and gate-5-scoped.

- [ ] **Step 5: Full gate**

```powershell
cargo build --all-targets
cargo build --manifest-path crates/vfs-payload/Cargo.toml --target-dir target
cargo test --workspace
cargo clippy --all-targets -- -D warnings
```

- [ ] **Step 6: Commit**

---

## Gate 5 Exit Criteria

- [ ] No filename-based early return remains in `try_fuse_create`; `keep_host_steam_api` and `fuse_skyrim_exe` are gone.
- [ ] The four names resolve through the director from the existing root-0 layers (no new mount — Task 1 established they are already reachable), and the Steam library remains unmounted.
- [ ] `FellThroughDrmException` is still wired and reads **zero** in a live session.
- [ ] `NtDeleteFile` is hooked; a delete under a managed root does not reach the real file.
- [ ] A rename from outside into a managed root fails rather than landing.
- [ ] `Decision::Redirect`/`Deny` are deleted, or what keeps each alive is named.
- [ ] The unbounded recursion is gone by construction, not dormant.
- [ ] A shim whiteout is neither visible nor ineffective — or the path it lives on is deleted.
- [ ] An injected process exits without leaving a zombie holding the shim DLL.
- [ ] **A live session launches, reaches gameplay, saves, and shows every fall-through class at zero** — or the failure is recorded with its diagnosis and no bypass is reintroduced.
- [ ] Workspace at or above 583 minus only the three tests designed to die here; clippy clean; payload workspace builds.

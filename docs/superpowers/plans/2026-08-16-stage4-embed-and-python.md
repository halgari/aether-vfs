# Stage 4: `vfs-embed` and the Python Binding — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make this a library a host language embeds, not an application that happens to have a CLI. A Python program declares roots, composes providers out of Rust primitives, supplies its *own* provider written in Python, launches a game, and reads back what the game wrote — with the director running in the Python process.

**Architecture:** Extract the session lifecycle that currently lives split across `vfs_director::Session` and `vfs_directord::SessionRegistry` into a public `vfs-embed` crate. Prove the seam by re-pointing the existing `vfs` CLI at it — one implementation, two callers — before adding a third caller in Python via PyO3.

**Tech Stack:** Rust 2021, PyO3 + maturin (abi3 wheel), Windows NT API, shared-memory ring IPC, gRPC control plane.

## Global Constraints

- Design spec: `docs/superpowers/specs/2026-08-13-pluggable-providers-design.md` — §4 (architecture and the crate table), §5 (the provider contract), §6 (primitives and composition), §8 (the Python binding, which contains the target API verbatim), §10 (testing and conformance).
- Working directory for all cargo commands is `C:\oss\aether-vfs\rust`.
- Baseline: `cargo test --workspace` = **591 passed, 0 failed, 3 ignored**. Never lower it.
- Zero clippy warnings: `cargo clippy --all-targets -- -D warnings`.
- `vfs-payload` is a separate workspace with `panic = "abort"`; the main workspace is `panic = "unwind"`. **That split exists for PyO3 and is load-bearing for this stage** — do not undo it. See Task 1.
- **Build first, then test.** `cargo test --workspace` relinks concurrently with running tests and can lock `vfs_shim_dll.dll` mid-run.
- **`cargo build --workspace --tests` does NOT build `vfs_shim_dll.dll`** — `--tests` filters out the cdylib exactly as `--bin` does. Use an unfiltered `cargo build -p vfs-shim-dll`.
- Run injected test binaries **one at a time** while the shim tree is dirty.
- **The full suite exceeds ten minutes.** Scope your runs and state exactly what you ran.
- Conventional commit prefixes. Commit after every task.

### What is already true, so you do not rediscover it

1. **The write path is done and proven live.** Provider write ops, director dispatch, server-side copy-up via `OverlayProvider`, and rejection diagnostics all exist. A real Skyrim session saved 2,459,865 bytes byte-exact through the director with every fall-through class at zero.
2. **Composition is unified.** `vfs_director::compose_root` is the single composition function; `Session::mount`/`mount_at`/`set_write_layer_at` and the registry all funnel through it.
3. **The conformance suite is `assert_conformance(p: Arc<dyn Provider>)`** (`vfs-provider/src/conformance.rs:448`), capability-parameterised, in-process, no FFI. Task 6 is what makes a Python provider satisfiable by it.
4. **`tools/python_source_plugin/` is not a PyO3 prototype.** It is a standalone gRPC server over the existing `remote` source path — read-only, not wired into the build. Useful as proof that cross-language providers work; **not** the thing this stage builds.
5. **No `vfs-embed`, no PyO3 crate, no `pyproject.toml` or maturin config exists anywhere.**

---

### Task 1: Pay the debt `panic = "unwind"` created

**Files:** `crates/vfs-shim/src/hook.rs`

**Interfaces:** Every `extern "system"` hook entry point contains its own panic.

The workspace was switched from `abort` to `unwind` so PyO3 could turn panics into Python exceptions (spec §9). That is correct for the host process and this stage depends on it. But the shim DLL is built from the same workspace and injected into the *game*, and there it made things worse: `hook.rs:239-257` records that a panic inside an `extern "system"` function aborts anyway, because rustc forces an abort at that boundary — so unwind buys nothing there **and lets the panic run `Drop` impls through live game-process stack frames first.**

There are ~20 such entry points (`create_hook`, `write_hook`, `read_hook`, and the rest; the list is `install_all_detours`'). None wraps its body.

**Do this first**, because it is a known degradation in the injected component caused by the very change this stage builds on, and because it is far easier to add uniformly now than to retrofit after the surface grows.

- [ ] **Step 1: Write the failing test**

A hook whose body panics returns a sane NT status to its caller and does **not** run the panicking frame's destructors. Prove the second half — a `Drop` impl that sets a flag is the cheap way.

- [ ] **Step 2: Run to verify it fails**

- [ ] **Step 3: Implement**

Wrap each entry body. Decide what a caught panic returns — `STATUS_UNSUCCESSFUL` is the obvious candidate, but say why, and make it uniform. Watch reentrancy: the existing `ShimIoGuard`/`hook_reenter` machinery must not be left held by the unwind.

- [ ] **Step 4: Verify** — `-p vfs-shim` and the injected e2e, one binary at a time.

- [ ] **Step 5: Commit**

---

### Task 2: The `vfs-embed` crate

**Files:** Create `crates/vfs-embed/`; modify `crates/vfs-director/src/session.rs`, `crates/vfs-directord/src/registry.rs`

**Interfaces:** `vfs_embed::Session` — create, `declare_root`, `mount`, `set_write_layer`, `launch`, `rejected_writes`. Public, documented, and stable enough for two hosts to depend on.

Spec §4 places `vfs-embed` between the hosts (`vfs.exe`, `aethervfs`) and the `Director`/`vfs-provider`/primitive crates: *"Public embeddable API: session lifecycle, roots, composition, launch."*

Today that role is split. `vfs_director::Session` owns `declare_root` (`session.rs:199`), `mount`/`mount_at`/`set_root_mounts` (`:234-325`), `set_write_layer` (`:325-370`) and `launch` (`:510`). `vfs_directord::SessionRegistry` owns config→provider-graph building (`registry.rs:62-93`) and the gRPC-driven lifecycle.

**This is an extraction, not a redesign.** Move the seam; do not change what it does. Mixing an API extraction with semantic change is how a migration like this goes sideways — stage 1 of this project was deliberately a no-behaviour-change refactor for the same reason.

**Judgement I want:** decide whether `vfs-embed` wraps `Session` or *is* the new home for it, and whether `SessionRegistry`'s graph-building belongs inside or stays in the daemon. Argue it. The test is whether a host that is not the daemon can do everything the daemon can without reaching past the crate.

- [ ] **Step 1: Write the failing test** — a session built entirely through `vfs_embed`'s public API, with a root, a composed provider graph, and a write read back through the director. It must not reference `vfs_director` or `vfs_directord` types.

- [ ] **Step 2: Run to verify it fails**

- [ ] **Step 3: Implement**

- [ ] **Step 4: Verify** the full suite. Nothing should change behaviourally.

- [ ] **Step 5: Commit**

---

### Task 3: Re-point the `vfs` CLI onto `vfs-embed`

**Files:** `crates/vfs-directord/src/main.rs`, `crates/vfs-directord/src/registry.rs`

**Interfaces:** No CLI-visible change. Same subcommands, same behaviour.

Spec §8: *"It is a host over `vfs-embed`, exactly like `vfs.exe`. One session-lifecycle implementation, two callers."* This task makes the first caller real, and it is the honest test of Task 2 — an API that only its author calls has not been proven to be an API.

`vfs main.rs` is already thin (232 lines delegating into `apply_session_config`, `connect_or_spawn`, `serve_daemon`); the logic lives in `registry.rs` (890 lines) and `service.rs`.

**Do not port `skyrim-live.rs` here.** It is 2130 lines of scenario-specific harness and belongs to stage 5's CLI slimming.

- [ ] **Step 1: Re-point it**, and report anything the CLI needed that `vfs-embed` could not provide — that list is Task 2's real review.

- [ ] **Step 2: Verify** the e2e suite, which drives the CLI surface.

- [ ] **Step 3: Commit**

---

### Task 4: The `memory` provider

**Files:** Create in `crates/vfs-compose/`; register in `crates/vfs-source/src/lib.rs`

**Interfaces:** A read-write in-memory provider, constructible from a name→bytes map, readable back after a session.

The spec's target API uses it twice, and the second use is the point:

```python
inis = vfs.memory({"Skyrim.ini": ini_bytes})
...
print(inis.read("Skyrim.ini"))     # what the game actually wrote
```

`InlineProvider` (`vfs-compose/src/inline.rs`) is the closest thing and is **test-only, unregistered, and not writable**. Decide whether to promote it or write a sibling, and say which.

`SourceSpec` (`vfs-source/src/lib.rs:45-59`) has `Disk`, `Zip`, `Http` (unsupported), `Remote` — add `Memory` so it is reachable from config too, not only from code.

- [ ] **Step 1: Write the failing test** — `assert_conformance` over it with `Access::ReadWrite`, plus a write-then-read-back-from-the-host round trip.

- [ ] **Step 2: Run to verify it fails**

- [ ] **Step 3: Implement**

- [ ] **Step 4: Commit**

---

### Task 5: The `aethervfs` package skeleton

**Files:** Create `crates/vfs-python/` (or `bindings/python/` — your call, say why); `pyproject.toml`; maturin config

**Interfaces:** `import aethervfs` works; `vfs.Session(name)`, `session.add_root(id, name, path)`, `session.mount(root, provider)`, `session.launch(exe)`, `session.rejected_writes()`.

Spec §8: *"Package `aethervfs`, built with maturin and PyO3 as an abi3 wheel so one Windows wheel covers Python 3.8+."*

Build the skeleton with **Rust primitives only** — no Python-authored providers yet. `vfs.disk(path)` mounted at a root, launched, torn down. That is a complete vertical slice through PyO3, maturin, and `vfs-embed`, and it will surface the packaging problems before they are entangled with GIL questions.

**State the toolchain requirement explicitly in the report:** what must be installed to build the wheel, and whether it is present on this machine.

- [ ] **Step 1: Skeleton + build**, then a Python script that creates a session, mounts `vfs.disk`, and tears down cleanly.

- [ ] **Step 2: Verify** the wheel builds and imports.

- [ ] **Step 3: Commit**

---

### Task 6: `PyProvider` — a Python class as a first-class provider

**Files:** `crates/vfs-python/`

**Interfaces:** A Python class deriving `vfs.Provider` becomes an `Arc<dyn Provider>` the director can mount anywhere a Rust provider goes.

This is the stage's centre. The spec is unusually specific, and every clause is a requirement:

- **GIL discipline.** `PyProvider` acquires the GIL per call. Rust-side blocking (ring waits, disk I/O) runs under `allow_threads` **so the GIL is never held across a wait.** A Python provider serialises every director thread that reaches it — that is why `slow` exists.
- **Errors.** `vfs.VfsError(code)` maps to `ST_*`. Any other exception becomes `ST_IO_ERROR` with its traceback logged. **No exception crosses the FFI boundary uncaught.**
- **Registration-time validation.** The binding inspects the class at construction. Declaring `ReadWrite` without defining `write_at` is an error *there*, with the session never starting.
- **Data transfer.** `read_at`/`read_next` return `bytes`; Rust copies. A writable `memoryview` is explicitly deferred.

- [ ] **Step 1: Write the failing tests** — a Python provider serving bytes through the director; a Python provider raising `VfsError` mapping to the right `ST_*`; a Python provider raising `ValueError` becoming `ST_IO_ERROR` with the traceback logged and **the process surviving**; a `ReadWrite` class missing `write_at` refused at construction.

- [ ] **Step 2: Run to verify they fail**

- [ ] **Step 3: Implement**

- [ ] **Step 4: Verify** no exception path can abort the host. Task 1's `catch_unwind` covers the shim; this is the same discipline at the binding boundary, and it is the reason the workspace uses `unwind` at all.

- [ ] **Step 5: Commit**

---

### Task 7: Expose the primitives

**Files:** `crates/vfs-python/`

**Interfaces:** `vfs.disk`, `vfs.memory`, `vfs.readonly`, `vfs.seekable`, `vfs.cached`, `vfs.layered`, `vfs.overlay`, `vfs.router`.

Spec §8, on its own example: *"Everything except `SteamCdn` is a Rust primitive. That is the test of whether §6 succeeded."*

If a primitive turns out to be awkward to expose, that is a finding about §6's design, not a Python problem — **report it rather than papering over it in the binding.**

- [ ] **Step 1: Write the failing test** — the spec's own composition, built from Python: `cached(seekable(...))`, `layered(readonly(base), disk(...))`, `router({"*.ini": memory}, default=overlay(disk, upper=disk))`.

- [ ] **Step 2: Implement**

- [ ] **Step 3: Commit**

---

### Task 8: The Python conformance runner

**Files:** `crates/vfs-python/`, plus a Python test module

**Interfaces:** A Python-authored provider can be run against the same `assert_conformance` suite Rust providers face.

This is stage 4's gate: *"Python-authored provider passes conformance."*

`assert_conformance` takes `Arc<dyn Provider>` in-process (`conformance.rs:448`). Task 6's `PyProvider` is the adapter that makes a Python object satisfy it. Expose a `vfs.assert_conformance(provider)` that runs the **real** suite — not a reimplementation of it in Python.

A second suite would drift from the first, and the whole point is that a Python provider is held to the identical contract.

- [ ] **Step 1: Write the failing test** — a deliberately minimal Python provider passing conformance at `SeqRead`, and a Python provider that *lies* about its capabilities failing it.

- [ ] **Step 2: Implement**

- [ ] **Step 3: Commit**

---

### Task 9: The spec's example, end to end

**Files:** a Python example/test

**Interfaces:** None. This is the acceptance evidence.

Run the §8 example as close to verbatim as the environment allows: a Python-authored provider, Rust primitives composed from Python, two roots, a launch, and `inis.read("Skyrim.ini")` showing what the game actually wrote.

**If the real Skyrim launch is out of scope here, say so and run everything else** — stage 5 owns the full game port. What must work now is the whole chain with a stand-in executable.

- [ ] **Step 1: Run it**, and record what worked and what did not.

- [ ] **Step 2: Full gate**

```powershell
cargo build --all-targets
cargo build -p vfs-shim-dll
cargo build --manifest-path crates/vfs-payload/Cargo.toml --target-dir target
cargo test --workspace
cargo clippy --all-targets -- -D warnings
```

- [ ] **Step 3: Commit**

---

## Stage 4 Exit Criteria

- [ ] Every `extern "system"` hook contains its own panic, and a caught panic does not run destructors through game frames.
- [ ] `vfs-embed` exists and a host can run a full session through it without naming `vfs_director` or `vfs_directord` types.
- [ ] The `vfs` CLI runs on `vfs-embed` with no CLI-visible change — one implementation, two callers.
- [ ] A `memory` provider exists, passes `assert_conformance` as `ReadWrite`, and is readable from the host after a session.
- [ ] `import aethervfs` works from a maturin-built abi3 wheel.
- [ ] A Python-authored provider serves a real session through the director.
- [ ] No Python exception can abort the host process; `VfsError` maps to `ST_*`, everything else to `ST_IO_ERROR` with the traceback logged.
- [ ] A `ReadWrite` provider missing `write_at` is refused at construction, not at first write.
- [ ] **A Python-authored provider passes the same `assert_conformance` suite Rust providers do** — the stage gate.
- [ ] Every element of the spec's §8 example except the provider itself is a Rust primitive.
- [ ] Workspace at or above 591; clippy clean; payload workspace builds.

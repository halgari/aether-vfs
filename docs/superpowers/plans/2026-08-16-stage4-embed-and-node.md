# Stage 4: `vfs-embed` and the Node Binding — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make this a library a host language embeds, not an application that happens to have a CLI. A TypeScript program declares roots, composes providers out of Rust primitives, supplies its *own* provider written in JavaScript, launches a process, and reads back what it wrote — with the director running in the Node process. Python follows in 4b, against the contract Node forces.

**Architecture:** Extract the session lifecycle that currently lives split across `vfs_director::Session` and `vfs_directord::SessionRegistry` into a public `vfs-embed` crate. Prove the seam by re-pointing the existing `vfs` CLI at it — one implementation, two callers — before adding a third caller in Node via napi-rs.

**Tech Stack:** Rust 2021, napi-rs (N-API), Node/TypeScript, Windows NT API, shared-memory ring IPC, gRPC control plane.

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

### Task 5: The Node threading spike — measure before committing the shape

**Files:** a throwaway crate or example; nothing permanent unless it earns its place.

**Interfaces:** none. This produces numbers and a go/no-go on the boundary shape.

Spec §8b names one thing as unresolved: **whether an event-loop round trip per read is viable at real read volumes, or whether a JS provider is only usable behind `cached`.** The answer changes whether `cached` is a recommendation or a requirement, and it is cheap to measure now and expensive to discover in stage 5.

Build the smallest thing that answers it: a director worker thread calling a trivial JS provider through an N-API threadsafe function, blocking until the promise settles.

- [ ] **Step 1: Measure** the per-call round-trip cost, and the throughput of a sequential read workload through a JS provider — with and without `cached` in front.

- [ ] **Step 2: Prove the deadlock is real and the contract prevents it.** Spec §8b forbids provider calls originating on the host's main thread. Demonstrate what happens when that rule is broken, so the guard in Task 7 is aimed at something observed rather than feared.

- [ ] **Step 3: Report** the numbers and a recommendation. If `cached` turns out to be mandatory rather than advisory, say so — that is a finding about the design, and §8b should be corrected to state it.

---

### Task 6: The `aethervfs` Node package skeleton

**Files:** Create `bindings/node/` (or `crates/vfs-node/` — your call, say why); `package.json`; napi-rs config

**Interfaces:** `require('aethervfs')` works; `new Session(name)`, `session.addRoot(id, name, path)`, `session.mount(root, provider)`, `session.launch(exe)`, `session.rejectedWrites()`.

**Rust primitives only** — no JS-authored providers yet. `disk(path)` mounted at a root, launched, torn down. A complete vertical slice through napi-rs and `vfs-embed`, surfacing the packaging problems before they tangle with the threading ones.

Use **N-API via napi-rs**, not raw V8 bindings — ABI stability across Node versions is the whole point, and it is what makes Electron a later packaging question rather than a rewrite.

**State the toolchain requirement in the report:** what must be installed to build and load the addon, and whether it is present on this machine.

- [ ] **Step 1: Skeleton + build**, then a Node script that creates a session, mounts `disk`, and tears down cleanly.

- [ ] **Step 2: Verify** the addon builds and loads under plain Node.

- [ ] **Step 3: Commit**

---

### Task 7: `NodeProvider` — a JS object as a first-class provider

**Files:** the Node binding crate

**Interfaces:** A JS object implementing the provider methods becomes an `Arc<dyn Provider>` the director mounts anywhere a Rust provider goes.

This is the stage's centre, and spec §8b's threading contract is the specification:

1. **Provider calls originate only on director worker threads**, never the host's main thread. Task 5 will have demonstrated the deadlock; enforce against it rather than documenting it.
2. **The call may block the calling director thread** for as long as the host takes. `async` methods returning a `Promise` are expected and supported — the director thread parks until it settles.
3. **No throw or rejection crosses the boundary uncaught.** `VfsError(code)` maps to `ST_*`; anything else becomes `ST_IO_ERROR` with the stack logged.
4. **A provider that never settles** hangs one director thread, not the session — and that is a diagnosable failure that should be **counted**, not merely survived.
5. **Registration-time validation:** declaring `ReadWrite` without a `writeAt` is an error at construction, with the session never starting.

- [ ] **Step 1: Write the failing tests** — a JS provider serving bytes through the director; an `async` provider whose promise resolves late; `VfsError` mapping to the right `ST_*`; a plain throw becoming `ST_IO_ERROR` with the stack logged and **the process surviving**; a never-settling promise counted rather than silently hanging; a `ReadWrite` object missing `writeAt` refused at construction.

- [ ] **Step 2: Run to verify they fail**

- [ ] **Step 3: Implement**

- [ ] **Step 4: Verify** no host exception path can abort the process.

- [ ] **Step 5: Commit**

---

### Task 8: Expose the primitives to JavaScript

**Files:** the Node binding crate

**Interfaces:** `disk`, `memory`, `readonly`, `seekable`, `cached`, `layered`, `overlay`, `router`.

Spec §8's test, which applies unchanged to Node: *"Everything except [the host provider] is a Rust primitive. That is the test of whether §6 succeeded."*

If a primitive is awkward to expose, that is a finding about §6's design — **report it rather than papering over it in the binding.**

- [ ] **Step 1: Write the failing test** — the spec's composition, built from TypeScript: `cached(seekable(...))`, `layered(readonly(base), disk(...))`, `router` with a memory provider for `*.ini` and an `overlay` default.

- [ ] **Step 2: Implement**

- [ ] **Step 3: Commit**

---

### Task 9: The Node conformance runner

**Files:** the Node binding crate, plus a TypeScript test module

**Interfaces:** A JS-authored provider runs against the same `assert_conformance` suite Rust providers face.

This is stage 4's gate, restated for Node: **a host-authored provider passes conformance.**

`assert_conformance` takes `Arc<dyn Provider>` in-process (`conformance.rs:448`). Task 7's `NodeProvider` is the adapter. Expose `assertConformance(provider)` running the **real** suite — not a reimplementation in TypeScript. A second suite would drift, and the point is that a host provider is held to the identical contract.

- [ ] **Step 1: Write the failing test** — a minimal JS provider passing conformance at `SeqRead`, and a JS provider that *lies* about its capabilities failing it.

- [ ] **Step 2: Implement**

- [ ] **Step 3: Commit**

---

### Task 10: The example, end to end

**Files:** a TypeScript example/test

**Interfaces:** None. This is the acceptance evidence.

Run spec §8's example translated to TypeScript: a JS-authored provider, Rust primitives composed from JS, two roots, a launch, and reading back what the process actually wrote.

**The real Skyrim launch is stage 5's.** What must work now is the whole chain with a stand-in executable.

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

### Deferred to stage 4b: the Python binding

Spec §8 stands as written and is still wanted — `aethervfs` on PyPI, PyO3, maturin, abi3 wheel. It moves **after** Node for the reason in §8b: the GIL forgives a boundary shape that Node's event loop does not, so the stricter runtime defines the contract and Python implements against a settled one.

Everything in tasks 1-4 and 9 is shared. What Python adds is `PyProvider`, the GIL discipline (`allow_threads` so the GIL is never held across a wait), and the packaging.

---

## Stage 4 Exit Criteria

- [ ] Every `extern "system"` hook contains its own panic, and a caught panic does not run destructors through game frames.
- [ ] `vfs-embed` exists and a host can run a full session through it without naming `vfs_director` or `vfs_directord` types.
- [ ] The `vfs` CLI runs on `vfs-embed` with no CLI-visible change — one implementation, two callers.
- [ ] A `memory` provider exists, passes `assert_conformance` as `ReadWrite`, and is readable from the host after a session.
- [ ] The per-call event-loop round-trip cost is **measured**, and whether `cached` is mandatory or advisory is stated on evidence.
- [ ] `require('aethervfs')` works from a napi-rs addon under plain Node.
- [ ] A JS-authored provider serves a real session through the director, including an `async` method whose promise resolves late.
- [ ] No JS throw or rejection can abort the host process; `VfsError` maps to `ST_*`, everything else to `ST_IO_ERROR` with the stack logged.
- [ ] A never-settling promise is **counted**, not silently hung.
- [ ] A provider call originating on the host's main thread is refused rather than deadlocking.
- [ ] A `ReadWrite` provider missing `writeAt` is refused at construction, not at first write.
- [ ] **A JS-authored provider passes the same `assert_conformance` suite Rust providers do** — the stage gate.
- [ ] Every element of the example except the provider itself is a Rust primitive.
- [ ] Workspace at or above 591; clippy clean; payload workspace builds.

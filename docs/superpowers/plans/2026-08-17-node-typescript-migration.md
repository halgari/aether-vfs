# The Node Binding in TypeScript — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The `aethervfs` Node package is written in TypeScript, tested and built with pnpm and vitest, and its type declarations are **emitted from the implementation** rather than maintained beside it.

**Architecture:** The Rust addon does not change. What changes is the JavaScript layer around it: `index.cjs` + a hand-written `index.d.ts` become TypeScript sources with `tsc` emitting both JS and declarations; `node --test` becomes vitest; npm becomes pnpm.

**Tech Stack:** TypeScript, pnpm, vitest, napi-rs (unchanged), Node ≥ 22.6.

## Global Constraints

- Working directory for cargo commands is `C:\oss\aether-vfs\rust`. The Node package is `rust/crates/vfs-node/`.
- Baseline: `cargo test --workspace` = **654 passed, 1 failed, 3 ignored**. That one failure is `vfs-directord::e2e::directory_enumeration_under_a_managed_root_hides_an_unserved_real_file`, **pre-existing and red on master** — verified in a clean worktree at `b37a816` with its own target directory. It is not yours; do not fix it and do not let it mask a regression you caused.
- Node suites today: **36 tests, 35 pass, 0 fail, 1 todo**. The todo is the casefold failure. **It must remain a failing assertion, not become a skip.**
- Zero clippy warnings: `cargo clippy --all-targets -- -D warnings`.
- Node **v24.19.0** at `C:\Program Files\nodejs`; pnpm **11.22.0** at `%APPDATA%\npm`. **Shells spawned in this session have a stale `PATH`** — prepend both, or use full paths.
- `cargo build --workspace --tests` does not build cdylibs. The addon needs an unfiltered `cargo build -p vfs-node`.
- Conventional commit prefixes. Commit after every task.

### What must not regress

1. **The published package stays dependency-free.** The original zero-dependency choice was deliberate. pnpm, vitest and TypeScript are **devDependencies**; a consumer installing `aethervfs` must still get no transitive runtime dependencies. `pnpm install` becoming required for *development* is the cost being accepted here.
2. **The addon still builds with cargo alone**, with no Node toolchain, because `napi-sys` resolves `napi_*` from the host process at runtime.
3. **CI must keep working.** `.github/workflows/ci.yml` gained clippy, a Node job, and an `index.d.ts` drift check. The Node job invokes npm today; it has to invoke pnpm after this, and the drift check's subject changes when the declaration becomes generated.

### The honest case for this migration

Not "types would have caught the bugs" — a review established that both Critical documentation defects were JSDoc **prose**, which no type checker reads. The real reasons:

- **`index.cjs` (608 lines) and `index.d.ts` (660 lines) are two artefacts that must agree by discipline.** They did not: the declaration claimed environment variables were "child only" when the seam sets them process-wide, and its `memory()` example used an unfolded name and the wrong root — teaching the exact silent corruption the same file warns about 280 lines later.
- **The tree is already mixed** — five `.cjs` test files against two `.cts`, four `.cjs` examples against one `.cts`.
- **The prose half still needs its own gate.** The declared-vs-runtime drift check that already exists is what catches what `tsc` cannot; keep it.

---

### Task 1: pnpm, TypeScript, and vitest — the harness before the migration

**Files:** `crates/vfs-node/package.json`, `pnpm-lock.yaml`, `tsconfig.json`, `vitest.config.ts`, `.gitignore`

**Interfaces:** `pnpm install`, `pnpm test`, `pnpm build` all work against the code **as it stands today**, before any file is converted.

Doing the harness first means a failure during conversion is unambiguously the conversion's fault.

**Vitest's pool needs choosing against observed behaviour, not defaults.** This package is unusually hostile to test isolation:

- The addon holds **process-global Rust statics** — bridge handles are process-global integers by construction.
- The deadlock guard keys on **thread identity**, so "the main thread" inside a vitest worker is *not* the process main thread. Tests asserting refusal-versus-service depend on which loop is which.
- A leaked provider handle **keeps its event loop alive**; `releaseProvider` is mandatory, and a leak hangs teardown.
- A leaked injected child **keeps `aethervfs.node` mapped**, and the next build fails `EBUSY` — this already silently invalidated one mutation check earlier in the project.

Try the defaults, observe what breaks, then choose. `pool: 'forks'` with `fileParallelism: false` is the likely answer, but **measure rather than assume**, and record why in the config file next to the setting.

- [ ] **Step 1: `pnpm install` with the devDependencies**, and confirm `pnpm why` shows no runtime dependency reaching the published package.

- [ ] **Step 2: Make vitest run the existing `.cjs` and `.cts` tests unconverted.** Same count, same results, casefold still failing.

- [ ] **Step 3: Record the pool decision** with the evidence for it.

- [ ] **Step 4: Commit**

---

### Task 2: Convert the implementation

**Files:** `crates/vfs-node/src/*.ts` (new), replacing `index.cjs`, `provider-host.cjs`, `scripts/build.cjs`

**Interfaces:** Unchanged as seen from JavaScript. `require('aethervfs')` and any ESM entry both keep working.

`index.d.ts` stops being a source file and becomes a **build output**. That is the point of the task.

**Three details in the current declaration that the conversion must preserve rather than lose**, all of them corrections someone made deliberately:

- `registerProvider` must still state that `releaseProvider` is **mandatory**, because omitting it hangs the process.
- `providerCalls` is `undefined`, not `null` — the file's own header had this right while a JSDoc line had it wrong.
- `mount()` is not pure bookkeeping: it **throws on `seqread`**, and the declaration says so.

**Do not silently drop the module-path registration shape.** `providerWorker({ module, options })` takes a path, not an object, because isolates share no JS objects. Types must express that, not paper over it.

- [ ] **Step 1: Convert, and generate the declaration.** Then diff the generated `.d.ts` against the hand-written one and **report every difference** — each is either an improvement, a loss, or a latent bug the hand-written version was hiding.

- [ ] **Step 2: Verify** the suite is unchanged in count and outcome.

- [ ] **Step 3: Keep the drift check working.** Its subject is now generated; say what it checks and whether it still earns its place.

- [ ] **Step 4: Commit**

---

### Task 3: Convert the tests and examples

**Files:** everything under `crates/vfs-node/test/` and `examples/`

**Interfaces:** None. Same coverage, expressed in TypeScript under vitest.

Seven test files and five examples, ~2,500 lines. **Same assertions.** A conversion that quietly weakens one is worse than no conversion, and this project has caught tests claiming coverage they lacked more than once.

**Two that need specific care:**

- **The casefold test must remain a *failing* assertion.** In vitest that is `test.fails`, not `it.todo` or `skip`. It is pinned evidence that the spec's own example is broken; a skip erases exactly what it exists to preserve.
- **`self-call-worker.cjs`** exists to prove the deadlock guard refuses a same-loop call. Under vitest's pool it may no longer be running where it thinks it is. Verify it still refuses for the right reason, and that the guard's owning-thread comparison is still what makes it fail.

- [ ] **Step 1: Convert the tests**, and confirm count and outcomes match exactly.

- [ ] **Step 2: Convert the examples**, and run the §8 end-to-end example — it is stage 4's acceptance evidence and must still produce its two carrying numbers (165,376 bytes staged; 40 bytes read by the child).

- [ ] **Step 3: Commit**

---

### Task 4: CI, and the last of npm

**Files:** `.github/workflows/ci.yml`, any remaining npm invocation

**Interfaces:** CI runs the Node gates through pnpm.

The Node job is the only automation that verifies the addon's JS layer and **the stage gate itself** — `assertConformance` running the real Rust suite against a JS provider. It was added days ago precisely because none of that was checked; do not break it in passing.

- [ ] **Step 1: Move CI to pnpm**, with a lockfile-respecting install (`--frozen-lockfile`).

- [ ] **Step 2: Confirm the build step still produces the addon**, since `cargo build --workspace --tests` does not.

- [ ] **Step 3: Grep for surviving `npm` invocations** in scripts, docs and READMEs — the root `README.md` and the package's own docs both reference commands.

- [ ] **Step 4: Full gate**

```powershell
cargo build --all-targets
cargo build -p vfs-node
cargo build -p vfs-shim-dll
cargo test --workspace
cargo clippy --all-targets -- -D warnings
pnpm --dir rust/crates/vfs-node test
```

- [ ] **Step 5: Commit**

---

## Exit Criteria

- [ ] `pnpm install`, `pnpm test`, `pnpm build` all work; no npm invocation survives in scripts, CI, or docs.
- [ ] `index.d.ts` is **generated**, and the generated-versus-hand-written diff was reported item by item.
- [ ] The published package has **zero runtime dependencies**; vitest, TypeScript and pnpm are devDependencies only.
- [ ] The addon still builds with `cargo build -p vfs-node` and no Node toolchain present.
- [ ] Test count and outcomes are **unchanged**: 36 tests, 35 passing, and the casefold assertion still **failing** — as `test.fails`, not a skip.
- [ ] The §8 end-to-end example still runs and still reports 165,376 bytes staged and 40 bytes read by the child.
- [ ] The deadlock-guard test still refuses a same-loop call **for the right reason** under vitest's pool.
- [ ] Vitest's pool choice is recorded with the evidence that produced it.
- [ ] CI runs clippy, the addon build, and the Node gates through pnpm.
- [ ] `cargo test --workspace` shows no new failures beyond the pre-existing, master-red enumeration test.

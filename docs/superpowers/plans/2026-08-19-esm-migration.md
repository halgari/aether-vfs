# aethervfs ESM Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** make `aethervfs` an ESM package on current TypeScript, and have `providerWorker()` load ESM provider entries.

**Architecture:** the package already compiles under `module: nodenext`, so the CommonJS-ness comes entirely from the `.cts` extensions and the absent `"type": "module"`. The migration therefore splits cleanly in two: a large mechanical format flip that changes no behaviour, then a small behavioural change in the worker host where `require(path)` becomes `await import(fileURL)`.

**Tech Stack:** Node ≥24, TypeScript, vitest 3, napi-rs (Rust addon, untouched), pnpm.

**Spec:** `docs/superpowers/specs/2026-08-19-esm-migration-design.md`

## Global Constraints

- **Node ≥24**, Windows x64 only. `pnpm`, not npm, for installs in this package.
- Work in `rust/crates/vfs-node/`. **No Rust changes** — `aethervfs.node`, `vfs_shim_dll.dll` and `vfs_payload.dll` are untouched build artifacts.
- **Provider entries are ESM. CommonJS is not supported and must not be tested for.** No compatibility shim.
- **`outDir` stays absent.** Sources sit beside their outputs, which works only because the source and output extensions differ. This is load-bearing: `index` hands its own directory to Rust as the shim-DLL search path, and `native` looks for `aethervfs.node` there. Emitting to a separate directory breaks DLL resolution.
- **`.mts` → `.mjs`** is the naming scheme, mirroring the existing `.cts` → `.cjs` convention. Explicit beats relying on `"type": "module"` inference.
- These properties must survive, all of them: a provider instance never crosses an isolate; the worker's loop is the servicing loop; a factory is constructed on the worker's loop; DLL resolution still records the package directory; the deadlock guard still refuses a worker driving a session that mounts it; **failure is always reported to the parent.**
- Baseline to hold: `npx tsc --noEmit` clean, `npx vitest run` **42/42 passing**.
- Conventional commit prefixes.

---

## File Structure

| File | Now | After | Responsibility |
|---|---|---|---|
| `native.cts` | CJS | `native.mts` | Loads the `.node` addon; hand-written declarations for the Rust binary. |
| `index.cts` | CJS | `index.mts` | Public API. Hands its directory to Rust; spawns the provider worker. |
| `provider-host.cts` | CJS | `provider-host.mts` | Worker entry. Loads the provider module and registers it. |
| `package.json` | — | modified | `"type": "module"`, `main`/`types`/`files` renamed. |
| `tsconfig.json`, `tsconfig.build.json` | — | modified | `include` lists follow the renames. |
| `scripts/check-types.mts` | — | modified | Source→output map follows the renames. |
| `test/**/*.cts`, `examples/**/*.cts` | CJS | `.mts` | Suites and examples; several examples *are* provider entries. |

---

## Task 1: Toolchain currency

**Files:** Modify `package.json`

**Interfaces:**
- Produces: nothing consumed by later tasks; this is an isolated version bump that must be green before a migration lands on top of it.

Do this first and separately. A compiler upgrade and a module-format change both alter emit, and landing them together means a failure in the worker cannot be attributed to either. This task is the known-good floor.

`@types/node` is `^24.3.0` and the runtime is Node 24 — already correct, leave it. TypeScript is `^5.9.2`; the current release is 7.x.

- [ ] **Step 1: Record the baseline**

```bash
cd rust/crates/vfs-node
npx tsc --noEmit && npx vitest run
```
Expected: typecheck silent, `42 passed`.

- [ ] **Step 2: Upgrade TypeScript**

```bash
pnpm add -D typescript@^7
```

- [ ] **Step 3: Typecheck and fix what the new compiler reports**

```bash
npx tsc --noEmit
```

The package is already on `moduleResolution: nodenext`, so the removal of `node10` resolution in TypeScript 7 does not affect it. Expect few or no errors. Fix anything reported; do **not** silence a diagnostic with `any` or a `@ts-expect-error` — if the new compiler found something real, fix the code.

- [ ] **Step 4: Full test run**

```bash
npx vitest run
```
Expected: `42 passed`. If a test fails, that is a finding about TypeScript 7's emit and must be reported, not worked around.

- [ ] **Step 5: Commit**

```bash
git add package.json pnpm-lock.yaml
git commit -m "build: upgrade to TypeScript 7"
```

---

## Task 2: The format flip

**Files:**
- Rename: `native.cts` → `native.mts`, `index.cts` → `index.mts`, `provider-host.cts` → `provider-host.mts`
- Rename: `test/**/*.cts` → `.mts`, `examples/**/*.cts` → `.mts`
- Modify: `package.json`, `tsconfig.json`, `tsconfig.build.json`, `scripts/check-types.mts`, `scripts/build.mts`

**Interfaces:**
- Consumes: nothing.
- Produces: an ESM package whose emitted entry is `index.mjs` with types `index.d.mts`; the worker entry is `provider-host.mjs`. Task 3 changes how that host loads a module; Task 4 tests it.

**Behaviour must not change in this task.** `provider-host` keeps loading the provider entry through `require` — via `createRequire` — so that this commit is a pure format change. Task 3 makes the semantic change on its own. Resist doing both here; the whole point of the split is that a worker misbehaving afterwards has one suspect, not two.

- [ ] **Step 1: Rename the three sources and update package.json**

```bash
git mv native.cts native.mts
git mv index.cts index.mts
git mv provider-host.cts provider-host.mts
```

In `package.json`: add `"type": "module"`, and update

```json
  "main": "index.mjs",
  "types": "index.d.mts",
  "files": [
    "index.mjs", "index.d.mts",
    "native.mjs", "native.d.mts",
    "provider-host.mjs",
    "aethervfs.node", "vfs_shim_dll.dll", "vfs_payload.dll"
  ],
```

- [ ] **Step 2: Replace CommonJS globals in `native.mts`**

`__dirname` and `require` do not exist in ESM. The addon is a `.node` binary and can only be loaded by `require`, so create one:

```ts
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const packageDir = import.meta.dirname;
const addonPath = path.join(packageDir, 'aethervfs.node');
const addon = require(addonPath) as Record<string, unknown>;
```

`import.meta.dirname` is Node 20.11+ and this package requires ≥24. It must resolve to the same directory `__dirname` did — which holds because sources sit beside outputs and `outDir` is absent.

- [ ] **Step 3: Replace CommonJS globals in `index.mts`**

Three sites:

```ts
native.setPackageDir(import.meta.dirname);            // was __dirname
import { Worker as WorkerCtor } from 'node:worker_threads';   // was a lazy require
// ...
const worker = new WorkerCtor(path.join(import.meta.dirname, 'provider-host.mjs'), {
```

The `worker_threads` require was lazy "as it was before the conversion". A static ESM import is correct now — there is no cost to hoisting it, and a lazy `require` in an ESM file would need `createRequire` for no benefit.

- [ ] **Step 4: Keep `provider-host.mts` behaviourally identical**

It is now ESM, so its `require(data.module)` needs a `createRequire`:

```ts
import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
// ... unchanged below:
const mod = require(data.module) as Record<string, unknown>;
```

Also update its import of the package entry: `import * as aether from './index.cjs'` becomes `'./index.mjs'`.

This is deliberately temporary and Task 3 replaces it. Leave a comment saying so, so a reader does not mistake it for the intended design.

- [ ] **Step 5: Update the configs and scripts**

`tsconfig.json` `include`: `index.mts`, `native.mts`, `provider-host.mts`, `test/**/*.mts`, `examples/**/*.mts` (the `scripts`, `bench` and `vitest.config.ts` entries are unchanged). `exclude`: `**/*.d.mts`, `**/*.mjs` alongside the existing entries.

`tsconfig.build.json` `include`: the three `.mts` sources. Its `allowImportingTsExtensions: false` guard stays.

`scripts/check-types.mts` maps sources to outputs at lines 69–71 — update all three pairs to `.mts` → `.mjs`/`.d.mts`.

Check `scripts/build.mts` for any of the six old filenames and update.

- [ ] **Step 6: Convert tests and examples**

```bash
for f in test/*.cts examples/*.cts; do git mv "$f" "${f%.cts}.mts"; done
```

Then in those files: relative specifiers ending `.cts` become `.mts`, and `./index.cjs` becomes `./index.mjs`.

The base `tsconfig.json` records that files node loads directly kept `require` **with a type annotation**, because node's type stripping erases annotations but does not rewrite module syntax. Under ESM that workaround is unnecessary — convert those to real `import` statements and delete the comments explaining the workaround. This is the simplification the migration buys; leaving the `require` forms behind would keep the scar without the wound.

`vitest.config.ts`'s esbuild filter is already `/\.[cm]?ts$/`, which covers `.mts`. Its comment says "all five suites are `.cts`" — update it.

- [ ] **Step 7: Build, typecheck, test**

```bash
npx tsc -p tsconfig.build.json && npx tsc --noEmit && npx vitest run
```
Expected: emit produces `index.mjs`/`index.d.mts`/`native.mjs`/`native.d.mts`/`provider-host.mjs`; typecheck clean; `42 passed`.

Delete the stale `.cjs`/`.d.cts` outputs from the working tree — they are gitignored build products, but a leftover `index.cjs` beside a new `index.mjs` is exactly the kind of thing that makes a later failure confusing.

- [ ] **Step 8: Run the examples**

```bash
node examples/vertical-slice.mts && node examples/js-provider.mts && node examples/spec-8-example.mts
```
Expected: all three run to completion. These exercise the real worker path, so they are the first genuine signal that the flip did not break provider loading.

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "refactor: convert the package to ESM"
```

---

## Task 3: Load provider entries with `import()`

**Files:** Modify `provider-host.mts`, `index.mts` (doc comment only)

**Interfaces:**
- Consumes: the ESM package from Task 2.
- Produces: `providerWorker({ module })` accepts an ESM entry. Task 4 tests it.

This is the small, risky one. Everything in it is about a worker that must never leave its parent waiting.

- [ ] **Step 1: Register the release handler before loading**

Read the current ordering: the `port.on('message', …)` release handler is registered **after** the synchronous try/catch. That is safe today only because loading is synchronous. Once loading is awaited, a `release` arriving during the load would hit no listener, and the worker would stay alive forever holding its threadsafe function — a hang with no diagnostic.

Register the handler first, and record a release that arrives early:

```ts
let handle: number | null = null;
let releaseRequested = false;

port.on('message', (msg: unknown) => {
  const m = msg as { type?: unknown } | null;
  if (!m || m.type !== 'release') return;
  if (handle === null) {
    // The load has not finished. Record it; the loader releases on completion.
    releaseRequested = true;
    return;
  }
  aether.releaseProvider(handle);
  port.close();
});
```

The `handle === null` early return in the original silently dropped such a message. That was unreachable before and is reachable now.

- [ ] **Step 2: Replace `require` with `import()` and simplify the selection**

```ts
try {
  const mod = (await import(pathToFileURL(data.module).href)) as Record<string, unknown>;

  // Two accepted shapes: the export named by `spec.export`, else `provider`,
  // else `default`. A function is called with `options` and returns the
  // provider — a factory, so it is constructed on this loop, where its
  // methods will run.
  const picked = (data.export ? mod[data.export] : (mod.provider ?? mod.default)) as
    | Picked
    | undefined
    | null;

  if (picked === undefined || picked === null) {
    throw new Error(
      data.export
        ? `module has no export named ${JSON.stringify(data.export)}; it exports: ${Object.keys(mod).join(', ') || '(nothing)'}`
        : `module exported nothing usable as a provider or provider factory; it exports: ${Object.keys(mod).join(', ') || '(nothing)'}`
    );
  }

  const obj = typeof picked === 'function' ? picked(data.options) : picked;
  const provider = aether.registerProvider(obj, data.providerOptions);
  handle = provider.handle;
  port.postMessage({ ok: true, handle });
  if (releaseRequested) {
    aether.releaseProvider(handle);
    port.close();
  }
} catch (e) {
  port.postMessage({
    ok: false,
    message: e instanceof Error && e.message ? String(e.message) : String(e),
    stack: e instanceof Error && e.stack ? String(e.stack) : '',
  });
  port.close();
}
```

Three things to note. `pathToFileURL` is required, not cosmetic: `data.module` is an absolute path and on Windows `C:\…` parses as a URL scheme. The `?? mod` fallback arm is **gone** — it only ever matched because a CommonJS `module.exports = x` makes the module and the value the same object, and against an ESM namespace it is meaningless. And the error now lists the exports actually found, which turns "not a provider" into something a reader can act on.

Import `pathToFileURL` from `node:url`.

- [ ] **Step 3: Verify the failure path really reports**

```bash
npx vitest run
node examples/js-provider.mts
```

Then, by hand, confirm a provider entry that throws at import time still rejects `providerWorker()` rather than hanging. Task 4 turns this into a test; do the manual check now so you find out immediately rather than at the end.

- [ ] **Step 4: Update the API documentation**

`ProviderWorkerSpec.module`'s doc comment claims a CommonJS module is required. Rewrite it: the path must be **absolute** and name an **ESM** module. Keep and sharpen the real reason for a path rather than an object — isolates share no JS objects, so a provider instance cannot cross one; what crosses is the integer handle. Remove the `require.resolve()` suggestion, which is CJS-specific — `import.meta.resolve` is the ESM equivalent.

- [ ] **Step 5: Commit**

```bash
git add provider-host.mts index.mts
git commit -m "feat: load provider entries as ESM"
```

---

## Task 4: Tests for the new loading contract

**Files:** Create `test/provider-esm.test.mts`, `test/fixtures/` entries as needed

**Interfaces:**
- Consumes: `providerWorker` from Task 3.

Four behaviours, each a Definition-of-done item. The failure cases matter more than the success case: a successful load is already covered by the examples and the existing suites, whereas a *failing* load is the path that can hang.

- [ ] **Step 1: Write the tests**

```ts
import { describe, expect, it } from 'vitest';
import { providerWorker } from '../index.mjs';

describe('ESM provider entries', () => {
  it('registers a default-exported factory', async () => {
    const w = await providerWorker({ module: fixture('esm-default.mjs') });
    try {
      expect(w.handle).toBeGreaterThan(0);
    } finally {
      await w.close();
    }
  });

  it('registers a named export via spec.export', async () => {
    const w = await providerWorker({ module: fixture('esm-named.mjs'), export: 'makeProvider' });
    try {
      expect(w.handle).toBeGreaterThan(0);
    } finally {
      await w.close();
    }
  });

  it('reports a module that throws on load, rather than hanging', async () => {
    // The whole reason the release handler is registered before the await.
    await expect(providerWorker({ module: fixture('esm-throws.mjs') })).rejects.toThrow(/boom/);
  });

  it('names the exports it found when nothing is usable', async () => {
    await expect(providerWorker({ module: fixture('esm-empty.mjs') })).rejects.toThrow(/exports: somethingElse/);
  });
});
```

`fixture()` resolves an absolute path under `test/fixtures/` — `providerWorker` requires absolute paths and rejects relative ones.

The four fixtures: `esm-default.mjs` exports a factory as `default`; `esm-named.mjs` exports one as `makeProvider`; `esm-throws.mjs` does `throw new Error('boom')` at module scope; `esm-empty.mjs` exports only `somethingElse`.

Give each test an explicit timeout well under vitest's default so a hang fails fast and visibly instead of stalling the suite.

- [ ] **Step 2: Run them and watch the right ones fail first**

```bash
npx vitest run test/provider-esm.test.mts
```

- [ ] **Step 3: Confirm the full suite**

```bash
npx tsc --noEmit && npx vitest run
```
Expected: `46 passed` (42 baseline + 4).

- [ ] **Step 4: Commit**

```bash
git add test/provider-esm.test.mts test/fixtures
git commit -m "test: cover ESM provider entries and their failure modes"
```

---

## Task 5: Verify against a clean rebuild

**Files:** none — this task produces evidence, and a fix only if it finds one.

`aethervfs.node` and the two DLLs are gitignored build artifacts. Everything so far has run against binaries already on disk, which may predate the migration. A green suite against a stale artifact proves less than it appears to.

- [ ] **Step 1: Remove the artifacts and rebuild**

```bash
rm -f aethervfs.node vfs_shim_dll.dll vfs_payload.dll
npx tsc -p tsconfig.build.json && node scripts/build.mts
```

Rust is untouched by this migration, so the rebuild should be uneventful. If it fails, that is a pre-existing build problem — report it rather than fixing it here, unless the migration caused it.

- [ ] **Step 2: Full verification against the fresh build**

```bash
node scripts/check-types.mts && npx tsc --noEmit && npx vitest run
node examples/vertical-slice.mts && node examples/js-provider.mts && node examples/spec-8-example.mts
```
Expected: check-types passes with the new source→output map, typecheck clean, `46 passed`, all three examples run.

- [ ] **Step 3: Commit only if something needed fixing**

```bash
git commit -am "fix: <whatever the clean rebuild exposed>"
```

---

## Definition of done

> **Amended during execution.** Two items below moved. The test count is **48**,
> not 46: a fifth test was added for the release-during-load race (the guard was
> load-bearing and completely untested), and a sixth for rejecting a `file://`
> URL as `spec.module`. Task 1 was **dropped** — TypeScript 7 removes the
> Compiler API that `scripts/check-types.mts` uses to detect drift between the
> addon's real exports and `native.mts`'s declarations, and losing that gate was
> the worse trade. The package stays on TypeScript 5.9.3.

1. `npx tsc --noEmit` clean; `npx vitest run` green at **48**.
2. A provider entry written as **ESM** registers and serves — both as a default export and as a named export via `spec.export`.
3. A provider entry that **throws on load** reports a diagnosable failure and does not hang.
4. A provider entry that **exports nothing usable** fails at the load site naming the exports actually found.
5. `assertConformance` passes (it is exercised by the existing suites).
6. Every example runs, converted to ESM.
7. Verified against a **rebuilt** `.node`, not the artifact that was on disk.
8. `ProviderWorkerSpec.module`'s doc comment no longer claims CommonJS is required and explains the real isolate constraint.
9. No CommonJS compatibility shim, and no test asserting CJS entries work.
10. `package.json` is `"type": "module"` with `main: index.mjs`, and no `.cts` source remains in the package.

## Follow-on, not in this plan

`one-click-modding-mvp` pins this repo as a submodule. After this merges, that pin moves and its `src/vfs/providerWorker.cts` and `depotProvider.cts` become ordinary ESM `.ts` modules — which also removes the `tsc`-in-`beforeAll` hack in its worker test and simplifies its `verify-build` guard. That is a separate plan in that repo.

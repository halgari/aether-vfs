# aethervfs — ESM migration design

**Goal:** the `aethervfs` Node package becomes an ESM package on current Node and
TypeScript, and `providerWorker()` accepts an ESM *or* CommonJS provider entry
instead of requiring CommonJS.

**Status:** proposed, 2026-08-19.

---

## 1. Why

Two reasons, one of which is a defect.

**The defect.** `providerWorker()`'s documented contract says the module path
must be **CommonJS**. That is not a property of the design — it is a consequence
of one line, `provider-host.cts:50`:

```ts
const mod = require(data.module) as Record<string, unknown>;
```

The genuine constraint that shapes this API is different and remains true: **a
provider instance cannot cross an isolate boundary**, so registration takes a
module *path* resolved inside the worker rather than an object handed across.
Whether that path names an ESM or a CommonJS module is orthogonal to it, and the
docs currently conflate the two.

The cost lands on consumers. `one-click-modding-mvp` carries two files —
`src/vfs/providerWorker.cts` and `src/vfs/depotProvider.cts` — that exist as
`.cts` purely to satisfy this. That in turn forced a `tsc` build inside a test's
`beforeAll` (because Node's type-stripping cannot resolve the `.cts` dependency
graph under `moduleResolution: node`), and a bespoke `verify-build` guard to
confirm the compiled `.cjs` exists.

**The currency.** The package is `main: index.cjs` with `moduleResolution: node`.
TypeScript 7 has **removed** `moduleResolution: node10`, so that setting is not
merely dated — it blocks the compiler upgrade outright.

## 2. What changes

| | Now | After |
|---|---|---|
| Package format | CommonJS (`main: index.cjs`) | ESM (`"type": "module"`) |
| Provider entry | CommonJS only | **ESM or CommonJS** |
| `module` / `moduleResolution` | `commonjs` / `node` | `nodenext` / `nodenext` |
| Native addon load | `require('./index.cjs')` | `createRequire(import.meta.url)` |

## 3. The provider-host change

`require(data.module)` becomes a dynamic import of a `file://` URL:

```ts
const url = pathToFileURL(data.module).href;
const mod = (await import(url)) as Record<string, unknown>;
```

`import()` loads **both** module formats, so this is a strict superset of what
works today. Two details matter:

**Path, not specifier.** `data.module` is an absolute filesystem path. Passing it
to `import()` unconverted breaks on Windows, where `C:\...` parses as a URL
scheme. `pathToFileURL` is not optional.

**CJS arrives under `default`.** A CommonJS module loaded through `import()`
exposes its `module.exports` as the namespace's `default`. The existing
selection order — a named export, then `provider`, then `default`, then the
module itself — already handles this, but the reason changes and the comment
must say so. A CJS entry doing `module.exports = factory` previously matched the
"module itself" arm; it now matches `default`.

**The host becomes async.** `import()` returns a promise, so the top-level
`try`/`catch` that currently registers the provider and posts the ready message
becomes an async flow. The error path must stay equivalent: a failure to load,
a missing export, or a non-provider value must still post the same failure shape
to the parent rather than rejecting silently. This is the one part of the change
with real behavioural risk, because a worker that neither signals ready nor
reports a failure is indistinguishable from a hang — the same class of defect
already recorded against `bootstrap.rs`'s `Err(_) => 2`.

## 4. What must not regress

These are the properties the current design paid for, and the migration is only
correct if all of them survive:

1. **A provider instance never crosses an isolate.** Registration stays
   path-based; nothing starts passing objects.
2. **The worker's loop is the servicing loop.** `registerProvider` binds the
   threadsafe function to whichever isolate calls it, and that must remain the
   worker's.
3. **A factory is constructed on the worker's loop**, not the caller's.
4. **DLL resolution still records the package directory.** The addon is loaded
   through the package's own entry rather than the `.node` directly,
   deliberately; `createRequire` must preserve that.
5. **The deadlock guard still refuses** a worker driving a session that mounts it.
6. **Failure is always reported.** No path may leave the parent waiting.
7. **Conformance (`assertConformance`) passes unchanged**, and the measured
   throughput characteristics in spec §8c do not regress.

## 5. Consumer impact

`one-click-modding-mvp` is the only external consumer and is ours.

After this change its `providerWorker.cts` and `depotProvider.cts` can become
ordinary ESM `.ts` modules. That removes the `tsc`-in-`beforeAll` hack, lets the
`verify-build` guard shrink to checking a normal build output, and deletes the
"why are these two files different" question permanently.

That work happens in that repo, after this lands and the submodule pin moves.

## 6. Scope

**In:** the JS/TS surface of `rust/crates/vfs-node` — `index.cts`, `native.cts`,
`provider-host.cts`, examples, tests, benches, `package.json`, `tsconfig`s; the
`moduleResolution` change; TypeScript and toolchain currency.

**Out:** any Rust change. The `.node` addon and the two DLLs are untouched
build artifacts. No behavioural change to the VFS itself, no new API beyond
widening what `providerWorker()` accepts, and no change to the spec's measured
performance contract.

## 7. Risks

**The async host is the real one.** Getting the ready/failure signalling wrong
produces a hang with no diagnostic, which is expensive to debug and exactly the
failure mode this project has been bitten by before. It needs a test that
asserts a *failing* provider entry reports its failure, not merely that a good
one loads.

**Examples run as `.cts` today** (`node examples/js-provider.cts`) and are part
of `npm test`. They must keep working, whether by staying CJS — which the widened
contract now permits — or by converting.

**The `.node` and DLLs are gitignored build artifacts.** A clean checkout needs
a Rust build before tests pass, so the migration must be verified against a
rebuild, not only against the artifacts currently on disk.

## 8. Definition of done

1. `pnpm typecheck` clean and `pnpm vitest run` green at **42/42**, the current
   baseline.
2. A provider entry written as **ESM** registers and serves.
3. A provider entry written as **CommonJS** still registers and serves —
   backward compatibility is a tested claim, not an assumption.
4. A provider entry that **throws on load** reports a diagnosable failure to the
   parent, and does not hang.
5. `assertConformance` passes.
6. The examples in `npm test` all still run.
7. Verified against a **rebuilt** `.node`, not only the artifact on disk.
8. `ProviderWorkerSpec.module`'s doc comment no longer claims CommonJS is
   required, and explains what the real isolate constraint is.

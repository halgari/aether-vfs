# vfs-node review — `feat/stage4-embed`

**Verdict: mergeable after one Critical teardown hang is fixed; the FFI boundary itself is sound and the four must-preserve conversion details all survived.**

## What was run

* `pnpm --dir rust/crates/vfs-node test` — **5 files / 36 tests passed**, including the `test.fails`
  casefold case failing for the right reason.
* `cargo test -p vfs-node --tests` — 2 passed.
* Targeted probes (node `-e`) for handle lifecycle, release semantics, worker-death behaviour,
  `providerCalls` shape, `mount(seqread)`, and option-validation panics.
* A faithful re-implementation of `attrs()`/`decorates_a_function()` from
  `tests/napi_entry_points_contain_panics.rs`, run against synthetic sources.

## Priorities, answered

1. **Panic containment.** All 53 `#[napi]` functions carry `catch_unwind`; the only `#[napi`
   attributes without it are `#[napi(object)]`, the two class/impl attributes, and prose. Verified by
   grep and by the structural test. Containment demonstrably *works* (`panicForTest` probes, plus an
   unintended reachable panic caught cleanly). Two scope gaps in the guard, below.
2. **Handle lifecycle.** Clean. `#![deny(unsafe_code)]` holds crate-wide, `lookup_provider` uses
   bounds-checked `Vec::get`, bridges are a `HashMap`, and napi's `u32` conversion is total — every
   garbage integer (`-1`, `NaN`, `1e30`, `4294967295`, a foreign live handle) produces a named error.
   Use-after-release and post-worker-death calls both return `ST_IO_ERROR` with an accurate message,
   not a hang and not memory unsafety. Double release is a silent no-op, matching the documented
   idempotence.
3. **Deadlock guard.** Genuinely loop-keyed: `Bridge::owner` is `std::thread::current().id()`
   captured in `register_provider`, compared in `dispatch` before anything is queued. The test's two
   migration-added assertions do the right work — measured `ThreadId(1)` for the main-loop provider
   and `ThreadId(21)` for the worker, so `notStrictEqual` is load-bearing, and the refusal message is
   required to name the worker's own loop. `pool: 'forks'` is pinned with the reason.
4. **Conversion fidelity.** All four preserved. `providerCalls` is absent → `undefined` (measured:
   `'providerCalls' in report === false`) and asserted twice; `mount()` throws on `seqread` and
   `native.cts:480-489` says so; `releaseProvider` is documented mandatory in three places and made
   structural via `Symbol.dispose`/`asyncDispose` plus an exit warning; `providerWorker({ module })`
   is typed `module: string` with the isolate reasoning in both the interface and the function doc.
   No test became a skip — assertion counts are identical file-for-file across the `.cjs`→`.cts`
   commits (the single `-1` in `primitives.test.cts` is a comment mentioning `assert.ok`), and the
   casefold case is a real `test.fails`. `tsconfig.json` is `strict` + `noUncheckedIndexedAccess`
   with `allowJs: false`; nothing was loosened.
5. **Zero runtime dependencies.** Confirmed from `package.json` (no `dependencies` key at all) and
   from `pnpm-lock.yaml`'s importer block: `@types/node`, `typescript`, `vitest` as devDependencies
   only.

## Findings

### Critical

**`index.cts:1063` — `ProviderWorker.close()` never resolves once the worker has already exited, so
`await using` teardown hangs. VERIFIED.**

`close()` registers `this.worker.once('exit', …)` *after* the fact and awaits it. If the worker is
already gone — terminated, crashed, or exited on its own — `'exit'` has already fired and will not
fire again, and the 2 s `terminate()` backstop does not re-emit it on a dead worker. Probe: register
a worker provider, `await w.worker.terminate()`, then `Promise.race([w.close(), 8 s timer])` →
`TIMEOUT-close-never-resolved after 8010 ms`. Because `[Symbol.asyncDispose]` awaits `close()`, an
`await using w = await providerWorker(...)` block never completes and everything after it is
silently skipped. This is the exact failure class the release-accounting section and
`teardownTimeout: 30_000` exist for, and the `timer.unref?.()` means it can also present as a
process that exits mid-teardown rather than reporting anything. Compounding it: `providerWorker`'s
`worker.once('error', fail)` returns early once `settled` is true, so a worker that dies after
registration raises nothing anywhere — no rejection, no event, no counter.

### Important

* **`tests/napi_entry_points_contain_panics.rs:106-136` — the enumeration silently *skips* items it
  does not recognise, so the check can pass while an unguarded `#[napi]` entry point exists.**
  Verified by re-implementing the algorithm: a blank line between the attribute and the item
  (`item = ""`), a `/** … */` doc comment after the attribute, `pub(super) fn`, and
  `pub(crate) async fn` all fall out of `decorates_a_function` and are dropped from `functions`
  without a word. The `>= 40` floor does not catch it (there are 53). A `#[napi]` whose item is not a
  recognised function/struct/impl should be a hard failure, not an exclusion. No entry point is
  currently mis-formatted — verified.
* **`src/lib.rs:1080-1091`, `tests/napi_entry_points_contain_panics.rs:1-47` — the containment claim
  overreaches: `napi` 2.16.17 contains no `catch_unwind` anywhere in its runtime, and the check sees
  only `#[napi]` functions.** Three uncontained `extern "C"` frames exist in this crate:
  `ConformanceTask::compute`/`resolve` (`src/conformance.rs:138,187`, reached through
  `async_work.rs:100` with no wrapper), the `create_threadsafe_function` callback closure
  (`src/jsprovider.rs:1051`), and `Drop for Session` (`src/lib.rs:1039`, run from napi's finalizer).
  Each happens to be panic-free today — `compute` wraps the suite itself, `stop_serve` reduces to
  `let _ = j.join()` — but none of that is what the structural test guards, so "which is what makes
  that total" and "the property that survives an edit" are both stronger than the check earns.
* **`src/lib.rs:1088-1091` — "there is nothing else in this crate to point at … no `unwrap`, no
  `expect` on a reachable path" is false: `registerProvider(obj, { stallWarnMs: Infinity })` panics.**
  VERIFIED — `src/jsprovider.rs:1046` calls `Duration::from_secs_f64` on a host-supplied `f64` whose
  only filter is `> 0.0`, so `Infinity`, `1e300` and `Number.MAX_VALUE` all reach
  `cannot convert float seconds to Duration`. `callTimeoutMs` has the same shape at line 1045.
  Containment holds (it arrives as a catchable `Error` and the process survives), so this is a false
  doc claim plus a missing range check, not UB — but `panicForTest` is not the only reachable panic.
* **`index.cts:673-678` — `releaseProvider(n)` skips the validation every other wrapper applies, so a
  non-integer silently releases a *different, live* provider.** `handleOf` enforces
  `Number.isInteger(p) && p >= 0`, but the `typeof handle === 'number'` branch bypasses it and hands
  the value to napi's `ToUint32`. Measured: `releaseProvider(1.7)` targets handle 1 and
  `releaseProvider(NaN)` targets handle 0. Releasing an unrelated provider is exactly the
  loop-never-drains failure the surrounding 40 lines of prose are about.

### Minor

* `src/jsprovider.rs:648-659` — a `VfsError(ST_OK)` (`threw: true`, `status: 0`) is correctly mapped
  to `ST_IO_ERROR`, but logs "provider signalled status 0 … which is not a recognised ST_* code",
  which is untrue of 0. The `status == ST_OK` case wants its own message.
* `src/jsprovider.rs:1204`, `native.cts:755` — `released` means "`releaseProvider` was called", not
  "the loop is alive". After `worker.terminate()` it reports `false` while every call fails with
  `Closing` (measured). The declaration says only `released: boolean`.
* `src/lib.rs:1010-1014`, `native.cts:578` — `openTotals()` is always a two-element tuple and is
  typed `number[]`, so `[succeeded, failed]` lives only in prose.
* `src/jsprovider.rs:412-429` — the guard is checked before the released-tsfn check, so a
  main-loop provider that has been released reports "would deadlock" rather than "the loop is gone".
  Correct outcome, misleading first sentence.

import { defineConfig } from 'vitest/config';

export default defineConfig({
  // Vite's esbuild transform filter is `/\.(m?ts|[jt]sx)$/` — `.ts`, `.mts`,
  // `.jsx`, `.tsx`, and **not** `.cts`. **All six suites are `.mts`** as of the
  // ESM migration's task 2 (they were `.cts` before that), so without this filter
  // widened they would reach rollup as plain JavaScript and die on their first
  // `import type` with
  // `Parse failure: Expected ',', got '{'`. Widening the filter is the whole fix;
  // esbuild's `.cts` loader itself is already correct.
  //
  // It is load-bearing for one thing beyond syntax: `test/dispose.test.mts` uses
  // real `using` declarations, and esbuild is what lowers them now that the file
  // is TypeScript rather than the `.cjs` node ran natively. The tests there assert
  // that the dispose actually happened, including out of a `throw`, so a broken
  // lowering fails rather than passes quietly.
  esbuild: {
    include: [/\.[cm]?ts$/, /\.[jt]sx$/],
  },

  test: {
    // Every suite is `.mts` as of the ESM migration's task 2 (`.cts` before
    // that), and the glob is narrowed to match on purpose: `tsconfig.json` cannot
    // typecheck a `.cjs`/`.mjs` file (`allowJs` is off, and every one of those in
    // this package is a build output), so a suite added as `.mjs` would run
    // untypechecked. Keeping the two sets identical means a file is either both
    // run and checked, or neither. There is no `setupFiles` any more — the
    // TypeScript migration's `vitest-setup.ts` mapped `require('node:test')` onto
    // vitest so the suites could run unconverted, and its task 3 converted them
    // and deleted it.
    include: ['test/**/*.test.mts'],

    // ---------------------------------------------------------------------
    // POOL: forks, one process per test file. Do not change this to `threads`.
    // ---------------------------------------------------------------------
    //
    // This is `forks` for a reason, not because it is vitest 3's default. Pinning
    // it means a future default flip cannot silently move this suite onto worker
    // threads, where it still *passes* while testing something else.
    //
    // **The reason, from the code.** `vfs-embed`'s `Session::serve`/`launch` write
    // ten process-global `VFS_*` environment variables and serialize them on a
    // `LAUNCH_ENV_LOCK` (`crates/vfs-embed/src/session.rs`). That lock's own
    // doc comment states its limit: it "cannot serialize a host's *own* threads,
    // and `set_var` in a multi-threaded process races anything else reading the
    // environment." Under `pool: 'threads'` the five test files *are* a host's own
    // threads — the exact case the lock is documented as unable to cover. Under
    // `forks` each file gets its own process environment and the question does
    // not arise.
    //
    // **And from measurement.** Provider handles are process-global integers by
    // construction, and the deadlock guard keys on `std::thread::current().id()`
    // captured where the threadsafe function was bound. Running the suite five
    // times per configuration:
    //
    //   forks,   parallel : provider 9 on ThreadId(1), provider 10 on ThreadId(21)
    //   forks,   serial   : provider 9 on ThreadId(1), provider 10 on ThreadId(21)
    //   threads, serial   : provider 9 on ThreadId(1), provider 10 on ThreadId(21)
    //   threads, parallel : provider 97 on ThreadId(1|2|3), provider 98 on ThreadId(50)
    //
    // The forks rows are bit-identical to the `node --test` baseline this
    // migration has to preserve. The threads-parallel row is not: the handles are
    // in the nineties because five files' providers are alive in one registry at
    // once, and the guard's owning thread drifts run to run because "the main
    // thread" is whichever worker first asked Rust std for a thread id. Every
    // configuration reported 36/36, so the suite does not catch this — which is
    // precisely why the pool is pinned here instead of left to a default.
    //
    // This is also the JS-side reading of a convention the project already
    // adopted for Rust (audit §2.5): a test asserting on process-global state
    // takes the lock, or lives in its own test binary. Its own process is what
    // "its own test binary" means here.
    pool: 'forks',

    // ---------------------------------------------------------------------
    // fileParallelism: left at vitest's default (true), against expectation.
    // ---------------------------------------------------------------------
    //
    // Recorded because the obvious guess is wrong and the next person will make
    // it too. `fileParallelism: false` was predicted to be necessary and was
    // measured not to be:
    //
    //   * Outcomes identical — 36/36 over five runs each, parallel and serial,
    //     including with `isolate: false` and `poolOptions.forks.singleFork`.
    //   * No cross-file contamination to prevent. Under `forks` the isolation
    //     comes from process-per-file; serializing adds nothing on top.
    //   * Timing assertions unaffected. The tightest is the cost check's 500 µs
    //     ceiling: 61.6-66.2 µs parallel, 62.9-64.0 µs serial, and 77-78 µs in
    //     both when all 16 cores are saturated with competing load.
    //   * No leaked `vfs-probe.exe` after a parallel run, and `cargo build
    //     -p vfs-node` immediately afterwards does not hit `EBUSY`.
    //   * Serializing costs 34% wall clock: 4.32-4.36 s parallel, 5.73-5.81 s
    //     serial.
    //
    // Two claims that sounded good and are *not* the reason, so nobody rebuilds
    // the argument on them: scratch directories are `mkdtemp`, so they cannot
    // collide; and the ring section is `Local\vfs_ring_{pid}_{millis}`, so it is
    // already process-unique. The Rust-side stderr ordering is non-reproducible
    // under *both* settings, so serializing does not buy determinism there either.
    //
    // **What would change this answer:** setting `isolate: false` or
    // `poolOptions.forks.singleFork`. Either puts several test files back in one
    // process, at which point process-per-file is gone and serialization becomes
    // the only remaining protection for the global registry and the `VFS_*`
    // environment. If you set either, set `fileParallelism: false` with it.

    // node:test's default timeout is Infinity; vitest's is 5 s. These tests spawn
    // injected child processes, start worker loops, and run the real Rust
    // conformance suite — the cost check alone is ~1.7 s here and 4000 round
    // trips is the kind of thing that goes ten times slower on a loaded CI
    // runner. Adopting vitest's default would introduce timeout failures the
    // suite never had, so the budget sits well above anything observed rather
    // than snugly around it.
    testTimeout: 120_000,
    hookTimeout: 120_000,

    // A leaked provider handle keeps its event loop alive, so a teardown bug
    // shows up as a hang rather than a failure. Give it room to finish, then let
    // vitest say so instead of waiting forever.
    teardownTimeout: 30_000,
  },
});

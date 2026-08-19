// Tests for the loading contract task 3 put in place: a provider module is now
// loaded with `await import(pathToFileURL(data.module).href)` rather than
// `require(data.module)`. The four tests in the first `describe` cover the
// shapes that contract accepts (a default export, a named export via
// `spec.export`) and the failure modes that mattered *because* the load is now
// awaited rather than synchronous — a module that throws, or one that loads
// cleanly but exports nothing usable, both have to reject `providerWorker()`
// rather than leave it (and the worker) hanging.
//
// The fifth test, in the second `describe`, is the one that actually pins the
// reason `provider-host.mts` registers its release handler *before* the
// `await import(...)` and records an early release in `releaseRequested` rather
// than acting on it immediately: a `release` arriving while the load is still in
// flight has nowhere else to go. That guard is not reachable through the public
// `providerWorker()` API — `ProviderWorker`, and therefore `close()`, is not
// constructed until the worker's `ok` message arrives, so no caller of that API
// can ever get a release message to the worker before its load finishes. Proving
// the guard exists means constructing the raw `Worker` with `workerData`
// directly, the same way `providerWorker()` invokes `provider-host.mjs`
// internally.

import { describe, expect, it } from 'vitest';
import path from 'node:path';
import { Worker } from 'node:worker_threads';

import { providerWorker } from '../index.mjs';

/** An absolute path under `test/fixtures/` — `providerWorker` requires one. */
function fixture(name: string): string {
  return path.join(import.meta.dirname, 'fixtures', name);
}

describe('ESM provider entries', () => {
  it(
    'registers a default-exported factory',
    async () => {
      const w = await providerWorker({ module: fixture('esm-default.mts') });
      try {
        // Not `toBeGreaterThan(0)`: a handle is an index into a process-global
        // registry (`rust/crates/vfs-node/src/lib.rs`'s `intern_provider`), and
        // this file runs alone in its own forked process (`vitest.config.ts`
        // pins `pool: 'forks'`), so the first registration in it legitimately
        // gets handle 0 — `test/dispose.test.mts` makes the same point
        // ("releasing handle 0 released a live provider"). What matters here is
        // that registration produced a real handle at all.
        expect(w.handle).toBeGreaterThanOrEqual(0);
      } finally {
        await w.close();
      }
    },
    5000
  );

  it(
    'registers a named export via spec.export',
    async () => {
      const w = await providerWorker({ module: fixture('esm-named.mts'), export: 'makeProvider' });
      try {
        expect(w.handle).toBeGreaterThanOrEqual(0);
      } finally {
        await w.close();
      }
    },
    5000
  );

  it(
    'reports a module that throws on load, rather than hanging',
    async () => {
      // The whole reason the release handler is registered before the await:
      // this rejection has to reach providerWorker() rather than leave the
      // worker's 'ok'/'error' listeners waiting on a message that never comes.
      await expect(providerWorker({ module: fixture('esm-throws.mts') })).rejects.toThrow(/boom/);
    },
    5000
  );

  it(
    'names the exports it found when nothing is usable',
    async () => {
      await expect(providerWorker({ module: fixture('esm-empty.mts') })).rejects.toThrow(
        /exports: somethingElse/
      );
    },
    5000
  );
});

describe('release arriving while the module is still loading', () => {
  const HOST: string = path.join(import.meta.dirname, '..', 'provider-host.mjs');
  const SLOW: string = fixture('esm-slow.mts');

  /**
   * `p`, or a rejection naming what failed to settle in time.
   *
   * The failure mode under test is a hang: a worker that posts `{ ok: true }`
   * and then never exits, because the threadsafe function it registered is
   * never released. `Promise.race` against a short timer is what turns that
   * hang into a fast, visible failure instead of a stall behind vitest's
   * (here, 120 s) per-test budget — a test whose only symptom is "the suite
   * never finishes" is barely better than no test at all.
   */
  async function withTimeout<T>(p: Promise<T>, what: string, ms = 3000): Promise<T> {
    let timer: NodeJS.Timeout | undefined;
    try {
      return await Promise.race([
        p,
        new Promise<never>((_resolve, reject) => {
          timer = setTimeout(
            () =>
              reject(
                new Error(
                  `${what} did not settle within ${ms} ms — that is the release-during-load ` +
                    'hang, not a slow machine: a worker released after its load completes ' +
                    'exits in single-digit milliseconds.'
                )
              ),
            ms
          );
        }),
      ]);
    } finally {
      clearTimeout(timer);
    }
  }

  it(
    'exits when a release message arrives mid-load, rather than hanging forever',
    async () => {
      // This is deliberately *not* providerWorker(): ProviderWorker (and
      // close()) is only constructed after the worker's 'ok' message arrives,
      // so nothing built on the public API can post a release before the load
      // finishes. Constructing the Worker directly, against the built
      // provider-host.mjs, is the only way to land a release message mid-load —
      // esm-slow.mts takes ~300 ms to evaluate, and posting immediately after
      // construction lands this well inside that window.
      const worker = new Worker(HOST, {
        workerData: {
          module: SLOW,
          export: null,
          options: {},
          providerOptions: {},
        },
      });
      try {
        const exited = new Promise<number>((resolve) => worker.once('exit', resolve));
        worker.postMessage({ type: 'release' });
        await withTimeout(exited, 'the worker exiting after a release sent mid-load');
      } finally {
        // Guarantees the test process can still exit even if the assertion
        // above failed — the point of this test is exactly a worker that would
        // otherwise never stop on its own.
        await worker.terminate().catch(() => {});
      }
    },
    5000
  );
});

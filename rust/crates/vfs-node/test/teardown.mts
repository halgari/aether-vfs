// node:test's `t.after`, expressed in vitest — and why it is not simply
// `onTestFinished`.
//
// Three suites here take a per-test teardown context: a scratch directory to
// remove, a `ProviderWorker` to close, a provider handle to release. Ten call
// sites, and every one releases something whose leak is a **hang** rather than a
// failure — a live threadsafe function holds a ref on the loop that services it,
// so an unreleased provider is a loop that never drains, and nothing that runs
// "on the way out" ever runs.
//
// Vitest's `onTestFinished` is the right primitive and the wrong order, in two
// ways that both matter here:
//
//   * it runs its callbacks **last-in-first-out**, like a stack, while
//     node:test's `after` hooks run **first-in-first-out**. Draining one array
//     through a single `onTestFinished` keeps node's order, so a teardown
//     sequence that worked under `node --test` still works and this conversion
//     changes no behaviour it did not set out to change;
//   * node:test stops at the first hook that throws. Every callback here runs
//     even if an earlier one throws, and the errors are reported together,
//     because a failed `fs.rmSync` skipping the `releaseProvider` queued behind
//     it would hang teardown instead of failing it. Strictly safer, and it loses
//     no information.
//
// **This is not the node:test shim.** `vitest-setup.ts` patched `Module._load`
// and re-implemented a runner; it is deleted, and the suites now call vitest's
// own `test()` directly. What is left is one ordering decision, in one place,
// with its reason beside it.

import { onTestFinished } from 'vitest';

/** The slice of node:test's `TestContext` these suites ever used. */
export interface TestTeardown {
  after(fn: () => unknown): void;
}

/**
 * Call once at the top of a test body, then register teardown on the result.
 *
 * ```ts
 * test('…', async () => {
 *   const t = teardown();
 *   const dir = scratch(t, 'name');   // removed when the test ends
 * });
 * ```
 */
export function teardown(): TestTeardown {
  const queue: Array<() => unknown> = [];

  onTestFinished(async () => {
    const failures: unknown[] = [];
    for (const fn of queue) {
      try {
        await fn();
      } catch (err) {
        failures.push(err);
      }
    }
    if (failures.length === 1) throw failures[0];
    if (failures.length > 1) {
      throw new AggregateError(failures, `${failures.length} teardown callback(s) threw`);
    }
  });

  return {
    after(fn: () => unknown): void {
      queue.push(fn);
    },
  };
}

// Make `require('node:test')` mean vitest, so the existing suites run unchanged.
//
// ## Why this file exists at all
//
// The five test files were written against `node:test`. Task 1 of this migration
// stands the harness up *before* converting anything, so that a failure during
// the conversion is unambiguously the conversion's fault. That means vitest has
// to run `provider.test.cjs`, `panic.test.cjs`, `dispose.test.cjs`,
// `primitives.test.cts` and `conformance.test.cts` with not one character
// edited — and every one of them opens with some form of
// `require('node:test')`.
//
// Without interception that require reaches the real builtin, node:test's own
// runner runs the tests, prints its own ticks, and vitest reports
// `No test suite found in file` while exiting 1. That was measured, not assumed.
//
// ## Why `Module._load` and not `resolve.alias`
//
// `node:test` is a builtin specifier. Vite's resolver short-circuits builtins
// before aliases are consulted, and vitest hands CJS test files a `createRequire`
// require rather than routing bare specifiers through the module graph. Patching
// `Module._load` is the one seam both `.cjs` and `.cts` requires pass through,
// and it was verified to intercept from a setup file — the setup file runs in the
// same worker as the test file that follows it, so the patch is in place before
// the first `require('node:test')` executes.
//
// This file is temporary scaffolding. Task 3 converts the tests to vitest's own
// API, and it should be deleted then. Until then it is load-bearing.

import {
  afterAll,
  afterEach,
  beforeAll,
  beforeEach,
  describe,
  onTestFinished,
  test as vitestTest,
} from 'vitest';
import Module from 'node:module';

type TestFn = (t: NodeTestContext) => unknown;

/**
 * The node:test options this shim honours — and only those.
 *
 * `concurrency`, `plan`, `signal` and the rest are deliberately absent rather
 * than accepted and ignored. `assertKnownOptions` below rejects anything not
 * listed here, for the same reason the `TestContext` proxy throws on unknown
 * members: a silently dropped `plan: 3` would let a suite report coverage it does
 * not have.
 */
interface NodeTestOptions {
  todo?: boolean | string;
  skip?: boolean | string;
  only?: boolean;
  timeout?: number;
}

const HONOURED_OPTIONS = new Set(['todo', 'skip', 'only', 'timeout']);

function assertKnownOptions(name: string, options: NodeTestOptions): void {
  const unknown = Object.keys(options).filter((k) => !HONOURED_OPTIONS.has(k));
  if (unknown.length > 0) {
    throw new Error(
      `vitest-setup.ts: test('${name}') passes node:test option(s) ` +
        `${unknown.map((k) => `\`${k}\``).join(', ')} that this shim does not implement. ` +
        `Implement them here, or convert this test to vitest's API (task 3). Ignoring them ` +
        `would silently change what the test asserts.`
    );
  }
}

/**
 * The slice of node:test's `TestContext` these suites actually use.
 *
 * `t.after` is the whole of it — ten call sites across three files, every one of
 * them releasing something whose leak is a hang rather than a failure: a scratch
 * directory, a `ProviderWorker`, or a provider handle whose event loop stays
 * alive until `releaseProvider`.
 */
interface NodeTestContext {
  after(fn: () => unknown): void;
  readonly name: string;
}

/**
 * node:test runs a test's `after` hooks in registration order; vitest's
 * `onTestFinished` runs its callbacks in reverse, like a stack. Registering one
 * `onTestFinished` per test and draining an array through it keeps node's order,
 * so a teardown sequence that worked before still works.
 *
 * Every callback runs even if an earlier one throws, and the errors are reported
 * together afterwards. node:test stops at the first throwing hook, which here
 * would mean a failed `fs.rmSync` silently skipping the `releaseProvider` behind
 * it — and an unreleased provider handle keeps its event loop alive and hangs
 * teardown instead of failing it. Draining unconditionally is strictly safer and
 * loses no information.
 */
function makeContext(name: string): NodeTestContext {
  const teardown: Array<() => unknown> = [];
  onTestFinished(async () => {
    const failures: unknown[] = [];
    for (const fn of teardown) {
      try {
        await fn();
      } catch (err) {
        failures.push(err);
      }
    }
    if (failures.length === 1) throw failures[0];
    if (failures.length > 1) {
      throw new AggregateError(failures, `${failures.length} t.after() callbacks threw`);
    }
  });

  const ctx = {
    after(fn: () => unknown) {
      teardown.push(fn);
    },
    name,
  };

  // Anything outside the implemented slice throws by name rather than reading as
  // `undefined`. A shim that silently no-ops `t.plan()` or swallows a subtest
  // would let a suite report coverage it no longer has, which is the failure mode
  // this project has been bitten by before.
  return new Proxy(ctx, {
    get(target, prop, receiver) {
      if (typeof prop === 'symbol' || prop in target) {
        return Reflect.get(target, prop, receiver);
      }
      throw new Error(
        `vitest-setup.ts: the node:test shim does not implement TestContext.${String(prop)}. ` +
          `Add it here, or convert this test to vitest's API (task 3).`
      );
    },
  }) as NodeTestContext;
}

function nodeTest(name: string, a?: NodeTestOptions | TestFn, b?: TestFn): void {
  const options: NodeTestOptions = typeof a === 'function' || a == null ? {} : a;
  const fn = (typeof a === 'function' ? a : b) as TestFn | undefined;
  assertKnownOptions(name, options);

  if (fn === undefined) {
    // node:test treats a body-less test as a todo. Nothing here does that, and
    // guessing would be the silent-no-op failure mode above.
    throw new Error(`vitest-setup.ts: test('${name}') was given no function.`);
  }

  const run = () => fn(makeContext(name));

  // **`todo` becomes `test.fails`, never `skip`.**
  //
  // There is exactly one `todo` in the tree: primitives.test.cts's "6b. a
  // capitalised path in memory()". It is not unfinished work. It is a *failing*
  // assertion, pinned deliberately: the injected VFS shim folds vpath components
  // to lower case, `memory()` is case-sensitive, and spec §6's answer to that —
  // `casefold(p)` — does not exist in Rust. The test states the behaviour a host
  // is entitled to, so the day someone implements `casefold` it turns green for
  // the right reason. A skip would delete that evidence and leave a name behind.
  //
  // `test.fails` is a stricter contract than node's `todo`: node tolerates a todo
  // that passes, while `test.fails` reports `Expected test to fail, but it
  // passed` if the body stops throwing. That is the direction to be strict in —
  // the day `casefold` lands, the suite should go red and say so rather than
  // quietly keep a green tick on a stale name.
  //
  // **Do not add a `throw` for the "todo now passes" case.** An earlier version of
  // this shim did, and it silently defeated the whole mechanism: a throw inside a
  // `.fails` body is exactly what `.fails` is looking for, so a passing todo
  // reported green. Verified by probe — two todos, one failing and one passing,
  // and both came back passing until the throw was removed. `test.fails` must be
  // left to make that call on its own.
  //
  // Vitest prints nothing for a `.fails` test that duly failed, so the body is
  // wrapped only to log the error before rethrowing. Otherwise the run would
  // claim 36 passing tests with no visible trace of the failing assertion that is
  // the entire point of this one.
  if (options.todo !== undefined && options.todo !== false) {
    const why = typeof options.todo === 'string' ? options.todo : 'no reason given';
    vitestTest.fails(name, async () => {
      try {
        await run();
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        console.log(
          `    known-failing (node:test todo -> test.fails): ${why}\n` +
            `    the assertion still fails, as intended: ${message}`
        );
        throw err;
      }
    });
    return;
  }

  if (options.skip !== undefined && options.skip !== false) {
    vitestTest.skip(name, run);
    return;
  }
  if (options.only) {
    vitestTest.only(name, run, options.timeout);
    return;
  }
  vitestTest(name, run, options.timeout);
}

// node:test's module object is callable *and* carries named exports, and both
// shapes are in use here: `const test = require('node:test')` in provider.test.cjs
// and the two `.cts` files, `const { test } = require('node:test')` in
// dispose.test.cjs and panic.test.cjs.
const shim = Object.assign(nodeTest, {
  test: nodeTest,
  it: nodeTest,
  default: nodeTest,
  describe,
  suite: describe,
  before: beforeAll,
  after: afterAll,
  beforeEach,
  afterEach,
  skip: (name: string, a?: NodeTestOptions | TestFn, b?: TestFn) =>
    nodeTest(name, { ...(typeof a === 'function' || a == null ? {} : a), skip: true }, typeof a === 'function' ? a : b),
  todo: (name: string, a?: NodeTestOptions | TestFn, b?: TestFn) =>
    nodeTest(name, { ...(typeof a === 'function' || a == null ? {} : a), todo: true }, typeof a === 'function' ? a : b),
  only: (name: string, a?: NodeTestOptions | TestFn, b?: TestFn) =>
    nodeTest(name, { ...(typeof a === 'function' || a == null ? {} : a), only: true }, typeof a === 'function' ? a : b),
});

// Idempotent: setup files run once per test file, and in a non-isolated pool the
// same worker can run several. Double-patching would nest the interceptors, and
// the marker makes that impossible to do by accident.
const MARKER = '__aethervfs_node_test_shim__';
const mod = Module as unknown as Record<string, unknown> & {
  _load(request: string, parent: unknown, isMain: boolean): unknown;
};

if (!(MARKER in mod)) {
  const original = mod._load;
  mod[MARKER] = true;
  mod._load = function load(request: string, parent: unknown, isMain: boolean): unknown {
    // `node:test` only, deliberately not a bare `'test'`. All five suites use the
    // prefixed form, bare `require('test')` is not a builtin alias in Node, and
    // `test` is a real name on the npm registry — intercepting it would mean this
    // shim could silently shadow a package.
    if (request === 'node:test') return shim;
    return original.call(this, request, parent, isMain);
  };
}

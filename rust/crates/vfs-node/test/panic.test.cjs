'use strict';

// **A Rust panic must arrive in JavaScript as an exception, not as a dead
// process.**
//
// This is the behavioural half of `tests/napi_entry_points_contain_panics.rs`.
// That test proves every `#[napi]` function carries `catch_unwind`; it cannot
// prove the generated containment works, only that the flag is present. So this
// one calls a function that panics on purpose and requires the process to still
// be here afterwards.
//
// Two of these tests run the panic in a **child** process rather than in this
// one, and that is not belt-and-braces: if containment were missing, the panic
// would `abort()` — exit code `0xC0000409` / `STATUS_STACK_BUFFER_OVERRUN`, the
// signature of rustc's forced `panic_cannot_unwind` — and an in-process
// `assert.throws` cannot observe its own process dying. The child's exit code is
// what distinguishes "threw" from "aborted"; `node --test` reporting a failure
// requires a live reporter.
//
// Mutation-checked (2026-08-17): with `catch_unwind` removed from
// `panicForTest`'s attribute and the addon rebuilt, the child exits **3221226505
// = 0xC0000409** and `node --test` reports the whole file as one failure with
// zero tests run, because the harness process itself died. Restoring the flag
// returns all six to green. So this file cannot pass for a reason other than the
// containment working.

const assert = require('node:assert');
const { test } = require('node:test');
const { spawnSync } = require('node:child_process');
const path = require('node:path');

const vfs = require('..');

const PKG = path.resolve(__dirname, '..');

/** Run `expr` in a fresh node process; report how it ended. */
function inChild(expr) {
  const r = spawnSync(
    process.execPath,
    ['-e', `const vfs = require(${JSON.stringify(PKG)});\n${expr}`],
    { encoding: 'utf8', cwd: PKG }
  );
  return { status: r.status, stdout: r.stdout ?? '', stderr: r.stderr ?? '' };
}

test('the panic canary exists — this file cannot pass by having nothing to call', () => {
  assert.strictEqual(
    typeof vfs.panicForTest,
    'function',
    'panicForTest is gone; the containment is no longer demonstrated by anything'
  );
});

test('a panic in a #[napi] function surfaces as a JS exception', () => {
  assert.throws(
    () => vfs.panicForTest('string'),
    (e) => {
      assert.ok(e instanceof Error, `expected an Error, got ${typeof e}`);
      assert.match(e.message, /deliberate panic from panicForTest/);
      return true;
    }
  );
  // The point of the test: execution continues here.
  assert.strictEqual(typeof vfs.version(), 'string', 'the addon stopped answering after the panic');
});

test('every panic payload shape becomes a message, not an abort', () => {
  for (const [kind, pattern] of [
    ['string', /deliberate panic from panicForTest/],
    ['str', /deliberate &str panic/],
    // Neither `String` nor `&str`: napi's own fallback rendering.
    ['other', /panic from Rust code/],
  ]) {
    assert.throws(() => vfs.panicForTest(kind), pattern, `payload shape ${kind}`);
  }
  assert.strictEqual(typeof vfs.version(), 'string');
});

test('a panic does not abort the process — measured on a child, not asserted here', () => {
  const caught = inChild(`
    try { vfs.panicForTest('string'); }
    catch (e) { process.stdout.write('CAUGHT:' + e.message); process.exit(7); }
    process.stdout.write('NOT-THROWN'); process.exit(8);
  `);
  assert.strictEqual(
    caught.status,
    7,
    `expected the child to catch and exit 7; it exited ${caught.status}. ` +
      'A status near 3221225477 (0xC0000409) is the forced-unwind abort — i.e. the ' +
      "containment is gone. stdout=" + JSON.stringify(caught.stdout) +
      ' stderr=' + JSON.stringify(caught.stderr.slice(-400))
  );
  assert.match(caught.stdout, /^CAUGHT:/);
});

test('the process is usable after an uncaught panic-turned-exception', () => {
  // Uncaught: node prints the error and exits 1, which is an *ordinary*
  // uncaught-exception exit. An abort would not be 1.
  const uncaught = inChild(`vfs.panicForTest('string');`);
  assert.strictEqual(
    uncaught.status,
    1,
    `an uncaught panic-exception must exit 1 like any uncaught throw; got ${uncaught.status}. ` +
      'stderr=' + JSON.stringify(uncaught.stderr.slice(-400))
  );
  assert.match(uncaught.stderr, /deliberate panic from panicForTest/);
});

test('a bad argument is an ordinary error, so the canary is not a panic-only door', () => {
  assert.throws(() => vfs.panicForTest('nonsense'), /expected 'string', 'str' or 'other'/);
});

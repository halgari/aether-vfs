#!/usr/bin/env node

// Task 9: stage 4's gate — a host-authored provider passes conformance.
//
// Not *a* conformance suite. **The** one: `vfs_provider::assert_conformance`, the
// same function `DiskProvider`, `ZipProvider`, `MemoryProvider`,
// `LayeredProvider`, `RouterProvider`, `ReadOnlyProvider` and `SeekableProvider`
// are held to in their own Rust test suites, called on the `Arc<dyn Provider>`
// behind a handle. Spec §10 asks for one suite run against every provider in
// every language, and a second suite written in TypeScript would drift from the
// first — after which the two would disagree about what a provider owes, which is
// worse than having only one.
//
// Five properties:
//
//   1. a deliberately minimal JS provider passes at `seqread`;
//   2. the same object passes on the main loop *and* in a worker — the async
//      design is what makes the first of those possible at all;
//   3. it passes the *positional* suite through `seekable()`, which is spec §10's
//      own example (`assert_conformance(seekable(seq_fixture()))`) written from a
//      host;
//   4. a provider that lies about `readwrite` fails, and
//   5. a provider that lies about positional access fails —
//      both with the failing case named, and both after registration has
//      accepted them, which is where conformance earns its place.
//
// Written in TypeScript, run by Node's own type stripping. See the header of
// `primitives.test.cts` for what that does and does not buy.

import type { Provider, ProviderWorker } from '../index.cjs';

const test = require('node:test') as typeof import('node:test');
const assert = require('node:assert') as typeof import('node:assert');
const fs = require('node:fs') as typeof import('node:fs');
const os = require('node:os') as typeof import('node:os');
const path = require('node:path') as typeof import('node:path');

const aether = require(path.join(__dirname, '..', 'index.cjs'));
const {
  assertConformance,
  conformanceFixture,
  writeConformanceFixture,
  disk,
  memory,
  seekable,
  readonly,
  registerProvider,
  releaseProvider,
  providerWorker,
} = aether;

const FIXTURE_MODULE: string = require.resolve(path.join(__dirname, 'conformance-providers.cjs'));
const make = require(FIXTURE_MODULE);

interface Ctx {
  after(fn: () => unknown): void;
}

// Mirrors the exported `ConformanceReport`. Optional fields are `?:` and not
// `| null` because napi-rs omits an object key for a Rust `None` — see the note
// on `ConformanceReport.providerCalls` in `native.cts`.
interface ConformanceReport {
  handle: number;
  kind?: string;
  access: 'seqread' | 'read' | 'readwrite';
  immutable: boolean;
  slow: boolean;
  preferredBlock?: number;
  cases: string[];
  providerCalls?: number;
  durationMs: number;
}

function scratch(t: Ctx, name: string): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), `aethervfs-t9-${name}-`));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

/** A provider serviced by *this* loop, released when the test ends. */
function onMainLoop(t: Ctx, kind: string): Provider {
  const p: Provider = registerProvider(make({ kind }));
  t.after(() => releaseProvider(p.handle));
  return p;
}

function show(label: string, r: ConformanceReport): void {
  console.log(
    `    ${label}: access=${r.access} cases=[${r.cases.join(', ')}] ` +
      `providerCalls=${r.providerCalls} in ${r.durationMs.toFixed(1)} ms`
  );
}

// ---------------------------------------------------------------------------
// 1 & 2. The honest provider, on both kinds of loop.
// ---------------------------------------------------------------------------

test('1. a minimal JS provider passes the real conformance suite at seqread', async (t) => {
  const p = onMainLoop(t, 'sequential');

  // Registered on *this* loop and driven from it — the configuration task 7's
  // deadlock guard refuses for a blocking call. It works here because the suite
  // runs on a libuv pool thread and `await` leaves this loop free to service the
  // callbacks. That is the reason `assertConformance` returns a promise, and it
  // is what lets a host write a provider inline in its test file with no worker.
  const report: ConformanceReport = await assertConformance(p);

  assert.strictEqual(report.access, 'seqread');
  assert.deepStrictEqual(
    report.cases,
    ['common', 'sequential'],
    'a seqread provider faces the sequential cases and not the positional ones'
  );
  assert.strictEqual(report.kind, 'js');
  assert.strictEqual(report.immutable, true);
  assert.strictEqual(report.preferredBlock, 65536);

  // **The number that says the suite did work.** A JS provider that "passed"
  // without the bridge being crossed was skipped, not tested — and that failure
  // would look exactly like a pass without this assertion.
  assert.ok(
    typeof report.providerCalls === 'number' && report.providerCalls > 25,
    `the suite drove the provider across the bridge (${report.providerCalls} calls)`
  );
  show('minimal seqread provider on the main loop', report);
});

test('2. the same provider passes when it lives on a worker loop', async (t) => {
  const pw: ProviderWorker = await providerWorker({
    module: FIXTURE_MODULE,
    options: { kind: 'sequential' },
  });
  t.after(() => pw.close());

  const report: ConformanceReport = await assertConformance(pw.provider);
  assert.deepStrictEqual(report.cases, ['common', 'sequential']);
  assert.ok(typeof report.providerCalls === 'number' && report.providerCalls > 25);
  show('minimal seqread provider on a worker loop', report);

  // Neither loop refused a call: the suite ran on a libuv thread, which is
  // neither the main loop nor the worker's.
  assert.strictEqual(pw.stats()!.selfCallRefusals, 0);
  assert.strictEqual(pw.stats()!.stalledCalls, 0);
  assert.strictEqual(pw.stats()!.hostErrors, 0);
  assert.strictEqual(pw.stats()!.calls, pw.stats()!.settledCalls);
});

// ---------------------------------------------------------------------------
// 3. Spec §10's own example: a combinator over a host-authored leaf.
// ---------------------------------------------------------------------------

test('3. seekable() over the JS provider passes the *positional* suite', async (t) => {
  const p = onMainLoop(t, 'sequential');

  // `assert_conformance(seekable(seq_fixture()))` — spec §10 writes it in Rust
  // about a Rust fixture. This is the same sentence with a JavaScript leaf, and
  // it is the strongest single statement in this file: the positional cases are
  // being served by a Rust cursor over a JS `readNext`, and the provider that
  // could not answer one positional read on its own now answers all of them.
  const wrapped: Provider = seekable(p);
  const report: ConformanceReport = await assertConformance(wrapped);

  assert.strictEqual(report.access, 'read', 'seekable promoted the declaration');
  assert.deepStrictEqual(report.cases, ['common', 'positional']);
  assert.strictEqual(report.kind, 'seekable');
  // No bridge on the wrapper, so no counter — the leaf has it. `undefined` and
  // not `null`: napi-rs renders a Rust `None` as `undefined`, which task 7's
  // report already recorded for `callTimeoutMs`.
  assert.strictEqual(report.providerCalls, undefined);
  assert.ok(p.stats()!.calls > 25, 'the leaf was driven through the wrapper');
  show('seekable(js seqread)', report);

  // And the un-wrapped provider still refuses positional reads, which is what
  // makes the run above a promotion rather than a no-op.
  const bare: ConformanceReport = await assertConformance(p);
  assert.deepStrictEqual(bare.cases, ['common', 'sequential']);
});

// ---------------------------------------------------------------------------
// 4 & 5. The liars. A runner that passes everything is not a runner.
// ---------------------------------------------------------------------------

test('4. a provider that declares readwrite and discards writes fails', async (t) => {
  // Registration accepts it, and that is the point: task 7 already refuses
  // `readwrite` with no `writeAt`, so the lie has to be one construction cannot
  // see. This provider has every write method, returns the right byte count, and
  // reports the right size from `getattr` and `open`. Only reading the bytes back
  // shows the gap — which is a case in the suite and nowhere else.
  const p = onMainLoop(t, 'discardingWrites');
  assert.strictEqual(p.capabilities().access, 'readwrite', 'construction accepted the declaration');
  assert.ok(p.stats()!.methods.includes('writeAt'), 'and found writeAt, so rule 5 passed');

  await assert.rejects(
    () => assertConformance(p),
    (e: Error) => {
      // The failing case, by name. Not "conformance failed".
      assert.match(e.message, /written bytes did not read back/);
      assert.match(e.message, /assertConformance\(provider \d+\) failed after \d+ provider calls/);
      console.log(`    liar 1 rejected: ${e.message.split('.')[0]}.`);
      return true;
    }
  );

  // It failed *during* the write cases, not before them: the read cases passed,
  // so the provider is not simply broken.
  assert.ok(p.stats()!.calls > 25, `the suite got well into the run (${p.stats()!.calls} calls)`);
});

test('5. a provider that declares positional reads but ignores the offset fails', async (t) => {
  const p = onMainLoop(t, 'ignoresOffset');
  assert.strictEqual(p.capabilities().access, 'read');
  assert.ok(p.stats()!.methods.includes('readAt'), 'readAt is present, so construction passed');

  await assert.rejects(
    () => assertConformance(p),
    (e: Error) => {
      // Specifically the unaligned mid-file read. A cursor-based reader
      // reproduces a sequential walk exactly, so the whole-file read and all
      // three EOF cases pass; this is the case that does the work.
      assert.match(e.message, /unaligned read_at/);
      console.log(`    liar 2 rejected: ${e.message.split('.')[0]}.`);
      return true;
    }
  );
});

// ---------------------------------------------------------------------------
// 6. The controls. Without these, an `assertConformance` that rejected
//    everything would pass tests 4 and 5 and look like a working gate.
// ---------------------------------------------------------------------------

test('6. the runner passes Rust providers too, including composed ones', async (t) => {
  const dir = path.join(scratch(t, 'fixture'), 'tree');
  writeConformanceFixture(dir);

  // Real disk. `DiskProvider` is `ReadWrite`, so this is the only run in the file
  // that exercises the *write* cases to completion — the same cases liar 1 fails.
  const d: ConformanceReport = await assertConformance(disk(dir));
  assert.strictEqual(d.access, 'readwrite');
  assert.deepStrictEqual(d.cases, ['common', 'positional', 'writable']);
  assert.strictEqual(d.providerCalls, undefined, 'a Rust provider has no bridge to count');
  show('disk(fixtureTree)', d);

  // The reference tree comes from Rust, so `memory()` can be seeded with it
  // without this file holding a second copy of the contract.
  const fixture = conformanceFixture();
  assert.deepStrictEqual(
    fixture.map((f: { path: string }) => f.path).sort(),
    ['a.txt', 'sub/b.txt'],
    'and the tree really is what the suite expects'
  );
  const m: ConformanceReport = await assertConformance(
    memory(fixture.map((f: { path: string; bytes: Buffer }) => ({ path: f.path, bytes: f.bytes })))
  );
  assert.deepStrictEqual(m.cases, ['common', 'positional', 'writable']);
  show('memory(conformanceFixture())', m);

  // A combinator over a Rust leaf: `readonly` clamps to `read`, so the write
  // cases must *not* run — which is also the check that `cases` is derived from
  // the declaration rather than hard-coded.
  const ro: ConformanceReport = await assertConformance(readonly(disk(dir)));
  assert.deepStrictEqual(ro.cases, ['common', 'positional']);
  show('readonly(disk(fixtureTree))', ro);

  // **And a Rust provider that does not serve the tree must be rejected.** Added
  // because a mutation found the gap: no-op the suite and every assertion above
  // still passes, since `cases` and `kind` are derived from the declaration and
  // need no run. This one cannot pass without the suite actually executing.
  const empty = path.join(scratch(t, 'empty'), 'nothing');
  fs.mkdirSync(empty, { recursive: true });
  await assert.rejects(
    () => assertConformance(disk(empty)),
    (e: Error) => {
      assert.match(e.message, /getattr/);
      console.log(`    disk(emptyDir) rejected: ${e.message.split('.')[0]}.`);
      return true;
    }
  );
});

test('7. a handle that is not a provider is refused rather than run', async () => {
  await assert.rejects(() => assertConformance(9999), /no provider with handle 9999/);
});

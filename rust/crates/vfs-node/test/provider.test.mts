// Task 7: a JavaScript object as a first-class provider.
//
// Seven properties, one test each, and every one of them is a property spec §8b
// states as a contract rather than a nicety:
//
//   1. a JS provider serves bytes through the director — host-side *and* over the
//      ring, from a real director worker thread inside an injected process;
//   2. an `async` provider whose promise resolves late;
//   3. `VfsError(code)` maps to the right `ST_*`, and a code the workspace does
//      not define is clamped rather than passed through;
//   4. a plain `throw` becomes `ST_IO_ERROR` with the stack logged, and the
//      process survives;
//   5. a promise that never settles is counted, not silently hung;
//   6. a `ReadWrite` object with no `writeAt` is refused at construction, with
//      the session never starting;
//   7. the deadlock guard: a call serviced by the loop blocked waiting for it is
//      refused with a diagnosable error, from a worker as well as from main.
//
// Run: `pnpm test:unit` in the package directory, or `pnpm exec vitest run
// test/provider.test.mts` for this file alone.
//
// The stderr lines about provider throws, stalls and refusals are the *point* —
// they are the "logged" half of rules 3 and 4 — so they are not suppressed.
//
// ## `import` throughout, as of task 3
//
// Test files are transformed by vite before vitest runs them, so real `import`
// statements work — and a real `import` is what makes `assert.ok` an assertion
// function to TypeScript, which `require(...) as typeof import('node:assert')`
// never was (TS2775, 134 of them across this tree before the TypeScript
// migration's task 3). The provider fixture beside this file is ESM too, as of
// the ESM migration's task 3, and is loaded the same way in both places that
// matter: a real `import` here, and `await import(pathToFileURL(...).href)`
// inside the provider worker `providerWorker({ module: FIXTURE })` spawns — see
// the header of `providers.mts` and of `provider-host.mts`.

import { test } from 'vitest';
import assert from 'node:assert';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { Worker } from 'node:worker_threads';

import { teardown, type TestTeardown } from './teardown.mts';
import type { Provider, ProviderOptions, ProviderStats, ProviderWorker } from '../index.mjs';
import make, { type MakeOptions, type TestProvider } from './providers.mts';
import type { SelfCallResult } from './self-call-worker.mts';
import * as aether from '../index.mjs';

const { Session, disk, registerProvider, providerWorker, releaseProvider, VfsError } = aether;

const FIXTURE: string = path.join(import.meta.dirname, 'providers.mts');

const probeExe = path.join(import.meta.dirname, '..', 'fixtures', 'vfs-probe.exe');

// ---------------------------------------------------------------------------
// Scaffolding. Each test gets its own scratch tree and its own session, and the
// managed root is asserted empty — a JS provider that "worked" because a real
// file was sitting under the root would be the exact failure this project keeps
// catching.
// ---------------------------------------------------------------------------

function scratch(t: TestTeardown, name: string): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), `aethervfs-t7-${name}-`));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

function newSession(t: TestTeardown, name: string) {
  const s = new Session(`t7-${name}`);
  t.after(() => {
    try {
      s.close();
    } catch {
      /* already closed */
    }
    fs.rmSync(s.baseDir, { recursive: true, force: true });
  });
  return s;
}

/** A provider on its own worker loop, released when the test ends. */
async function workerProvider(
  t: TestTeardown,
  options: MakeOptions,
  providerOptions?: ProviderOptions
): Promise<ProviderWorker> {
  const pw = await providerWorker({
    module: FIXTURE,
    options,
    provider: providerOptions,
  });
  t.after(() => pw.close());
  return pw;
}

/** A provider serviced by *this* loop, released when the test ends. */
function mainLoopProvider(t: TestTeardown, options: MakeOptions): Provider {
  const p = registerProvider(make(options));
  t.after(() => releaseProvider(p.handle));
  return p;
}

function emptyRoot(t: TestTeardown, name: string): string {
  const dir = path.join(scratch(t, name), 'game-root');
  fs.mkdirSync(dir, { recursive: true });
  return dir;
}

function show(label: string, stats: ProviderStats): void {
  const keep = [
    'calls',
    'settledCalls',
    'vfsErrors',
    'hostErrors',
    'stalledCalls',
    'abandonedCalls',
    'selfCallRefusals',
    'dispatchFailures',
  ] as const;
  const shown = Object.fromEntries(keep.filter((k) => stats[k] !== 0).map((k) => [k, stats[k]]));
  console.log(`    ${label}: ${JSON.stringify(shown)}`);
}

// ---------------------------------------------------------------------------
// 1. Bytes through the director.
// ---------------------------------------------------------------------------

test('1. a JS provider serves bytes through the director, host-side and over the ring', async () => {
  const t = teardown();
  const root = emptyRoot(t, 'bytes');
  const content = path.join(path.dirname(root), 'content');
  const out = path.join(path.dirname(root), 'out.bin');
  fs.mkdirSync(content, { recursive: true });
  // Only the launch image is on real disk, and it is *not* under the managed
  // root — it is content in a `disk()` provider, so `launch` has to stage it.
  fs.copyFileSync(probeExe, path.join(content, 'probe.exe'));

  const pw = await workerProvider(t, { kind: 'bytes' });

  const s = newSession(t, 'bytes');
  s.addRoot(0, 'game', root);
  s.mount(0, disk(content));
  s.mount(0, pw.provider);

  // Host-side, through the graph the injected process will see.
  assert.strictEqual(s.readFile('js-served.txt').toString(), 'bytes-from-javascript');
  assert.deepStrictEqual([...s.readFile('small.bin')].slice(0, 3), [0xab, 0xab, 0xab]);
  assert.strictEqual(s.readFile('small.bin').length, 64);

  // A path the JS provider does not have must still be not-found, not empty.
  assert.throws(() => s.readFile('absent.txt'), /ST_NOT_FOUND/);

  const afterHost = pw.stats()!;
  assert.ok(afterHost.calls > 0, 'the bridge was actually crossed');
  assert.strictEqual(afterHost.calls, afterHost.settledCalls);
  assert.strictEqual(afterHost.hostErrors, 0);
  show('after host-side reads', afterHost);

  // Now the real thing: an injected child reading a path with no file behind it
  // anywhere, served by JavaScript over the ring from a director worker thread.
  const virtualPath = path.join(root, 'js-served.txt');
  assert.strictEqual(fs.existsSync(virtualPath), false);
  assert.deepStrictEqual(fs.readdirSync(root), [], 'the managed root must be empty on disk');

  const code = s.launch('probe.exe', { args: [virtualPath, out], wait: true });
  assert.strictEqual(code, 0, 'the child must exit cleanly');
  assert.strictEqual(
    fs.readFileSync(out).toString(),
    'bytes-from-javascript',
    "the child's read went over the ring into JavaScript"
  );
  assert.deepStrictEqual(fs.readdirSync(root), [], 'nothing was extracted into the managed root');

  const afterRing = pw.stats()!;
  assert.ok(afterRing.calls > afterHost.calls, 'the child’s reads crossed the bridge too');
  assert.strictEqual(afterRing.hostErrors, 0);
  assert.strictEqual(afterRing.selfCallRefusals, 0, 'main → worker is legal and must not be refused');
  show('after the injected launch', afterRing);
  console.log(`    ownerThread: ${afterRing.ownerThread}  (the session ran on the main loop)`);
  console.log(`    methods:     ${afterRing.methods.join(', ')}`);
  console.log(`    access:      ${afterRing.access}, slow=${afterRing.slow}, immutable=${afterRing.immutable}`);
});

// ---------------------------------------------------------------------------
// 2. A promise that resolves late.
// ---------------------------------------------------------------------------

test('2. an async provider whose promise resolves late parks the caller and then answers', async () => {
  const t = teardown();
  const root = emptyRoot(t, 'async');
  const pw = await workerProvider(t, { kind: 'async', delayMs: 150 });

  const s = newSession(t, 'async');
  s.addRoot(0, 'game', root);
  s.mount(0, pw.provider);

  const t0 = process.hrtime.bigint();
  const got = s.readFile('late.txt').toString();
  const ms = Number(process.hrtime.bigint() - t0) / 1e6;

  assert.strictEqual(got, 'resolved-after-a-delay');
  // The point of the assertion is that the calling thread genuinely *waited*: a
  // bridge that returned early would have to have invented the bytes.
  assert.ok(ms >= 140, `readFile parked for the promise (waited ${ms.toFixed(1)} ms)`);
  console.log(`    readFile('late.txt') waited ${ms.toFixed(1)} ms for a 150 ms promise`);

  const st = pw.stats()!;
  assert.strictEqual(st.hostErrors, 0);
  assert.strictEqual(st.abandonedCalls, 0);
  assert.strictEqual(st.stalledCalls, 0, 'a 150 ms promise is not a stall at the 5 s default');
  show('async provider', st);
});

// ---------------------------------------------------------------------------
// 3. VfsError → ST_*.
// ---------------------------------------------------------------------------

test('3. VfsError maps to the status it names, and an undefined code is clamped', async () => {
  const t = teardown();
  const root = emptyRoot(t, 'errors');
  const pw = await workerProvider(t, { kind: 'errors' });

  const s = newSession(t, 'errors');
  s.addRoot(0, 'game', root);
  s.mount(0, pw.provider);

  // By name, and by number from statusCodes(): both reach the director intact.
  assert.throws(() => s.readFile('readonly.txt'), /ST_READ_ONLY/);
  assert.throws(() => s.readFile('nospace.txt'), /ST_NO_SPACE/);
  // A path the provider refuses at `open`.
  assert.throws(() => s.readFile('no-such-file.txt'), /ST_NOT_FOUND/);
  // 12345 is not a status this workspace defines. Rust is the authority and
  // clamps it, so a host cannot inject an arbitrary code into the director.
  assert.throws(() => s.readFile('bogus.txt'), /ST_IO_ERROR/);
  // A VfsError naming ST_OK is a failure claiming to be a success. It must not
  // come back as "worked, returned nothing".
  assert.throws(() => s.readFile('okerror.txt'), /ST_IO_ERROR/);

  const st = pw.stats()!;
  assert.strictEqual(st.hostErrors, 0, 'a VfsError is not a host error');
  assert.ok(st.vfsErrors >= 4, `deliberate statuses are counted (${st.vfsErrors})`);
  show('error provider', st);

  // And the session still works: a status is a failed call, not a failed graph.
  assert.strictEqual(s.readFile('ok.txt').toString(), 'ok');
});

// ---------------------------------------------------------------------------
// 4. A plain throw.
// ---------------------------------------------------------------------------

test('4. a plain throw becomes ST_IO_ERROR with the stack logged, and the process survives', async () => {
  const t = teardown();
  const root = emptyRoot(t, 'throw');
  const pw = await workerProvider(t, { kind: 'errors' });

  const s = newSession(t, 'throw');
  s.addRoot(0, 'game', root);
  s.mount(0, pw.provider);

  assert.throws(() => s.readFile('boom.txt'), /ST_IO_ERROR/);
  let st = pw.stats()!;
  assert.strictEqual(st.hostErrors, 1);
  assert.match(st.lastHostError!, /boom — a plain throw from a JS provider/);
  // It is a stack, not just a message: the frame names the file it was thrown in.
  assert.match(st.lastHostError!, /providers\.mts/);
  assert.match(st.lastHostError!, /\n\s+at /);
  console.log(`    lastHostError first two lines:`);
  for (const line of st.lastHostError!.split('\n').slice(0, 2)) console.log(`      ${line.trim()}`);

  // A rejection is the same path.
  assert.throws(() => s.readFile('rejected.txt'), /ST_IO_ERROR/);
  st = pw.stats()!;
  assert.strictEqual(st.hostErrors, 2);
  assert.match(st.lastHostError!, /rejected — a promise that says no/);

  // A result that is not bytes at all: the throw comes from the dispatcher's own
  // coercion, not from the host, and has to be caught in the same place — an
  // exception escaping a threadsafe-function callback ends the *process* rather
  // than the call. That this test keeps running is the assertion.
  assert.throws(() => s.readFile('badshape.txt'), /ST_IO_ERROR/);
  st = pw.stats()!;
  assert.strictEqual(st.hostErrors, 3);
  assert.match(st.lastHostError!, /must return a Buffer, a typed array or a string; got number/);

  // The process survived all three, and so did the provider.
  assert.strictEqual(s.readFile('ok.txt').toString(), 'ok');
  assert.strictEqual(aether.outstandingProviderCalls(), 0, 'no call was left dangling');
  show('after two uncaught host failures', pw.stats()!);
});

// ---------------------------------------------------------------------------
// 5. A promise that never settles.
// ---------------------------------------------------------------------------

test('5. a never-settling promise is counted, and with callTimeoutMs it releases the thread', async () => {
  const t = teardown();
  const root = emptyRoot(t, 'never');
  // `stallWarnMs` is what counts the hang; `callTimeoutMs` is what lets a test
  // observe the counter instead of inheriting the contract's default, which is
  // to park the director thread indefinitely (spec §8b: "hangs one director
  // thread, not the session").
  const pw = await workerProvider(t, { kind: 'never' }, { stallWarnMs: 100, callTimeoutMs: 400 });

  const s = newSession(t, 'never');
  s.addRoot(0, 'game', root);
  s.mount(0, pw.provider);

  const t0 = process.hrtime.bigint();
  assert.throws(() => s.readFile('never.txt'), /ST_IO_ERROR|never settl|abandoned/);
  const ms = Number(process.hrtime.bigint() - t0) / 1e6;

  const st = pw.stats()!;
  assert.strictEqual(st.abandonedCalls, 1, 'the abandoned call is counted');
  assert.ok(st.stalledCalls >= 1, 'the stall is counted before the timeout, and separately');
  assert.match(st.lastDiagnostic!, /abandoned `readAt`/);
  assert.ok(ms >= 380 && ms < 3000, `it waited the configured timeout, not forever (${ms.toFixed(0)} ms)`);
  console.log(`    readFile('never.txt') gave up after ${ms.toFixed(0)} ms`);
  console.log(`    lastDiagnostic: ${st.lastDiagnostic!.split('.')[0]}.`);
  show('never-settling provider', st);

  // The lost call is one call. The session, the provider and the process are all
  // still working, and nothing is left in the call table for a late completion
  // to write into.
  assert.strictEqual(s.readFile('ok.txt').toString(), 'ok');
  assert.strictEqual(aether.outstandingProviderCalls(), 0);
});

test('5b. a slow-but-settling call is counted as a stall without being abandoned', async () => {
  const t = teardown();
  const root = emptyRoot(t, 'slow');
  // No callTimeoutMs: this is the *default* contract — wait as long as the host
  // takes. The stall counter is what makes that diagnosable rather than silent.
  const pw = await workerProvider(t, { kind: 'slow', delayMs: 250 }, { stallWarnMs: 60 });

  const s = newSession(t, 'slow');
  s.addRoot(0, 'game', root);
  s.mount(0, pw.provider);

  assert.strictEqual(s.readFile('slow.txt').toString(), 'eventually');
  const st = pw.stats()!;
  // napi-rs renders a `None` as `undefined`, so `== null` rather than `=== null`.
  assert.ok(st.callTimeoutMs == null, 'the default is to wait, not to abandon');
  assert.ok(st.stalledCalls >= 1, 'the slow call was counted');
  assert.strictEqual(st.abandonedCalls, 0, 'and not abandoned');
  assert.strictEqual(st.settledCalls, st.calls);
  assert.match(st.lastDiagnostic!, /has not settled `readAt`/);
  show('slow-but-settling provider', st);
});

// ---------------------------------------------------------------------------
// 6. Registration-time validation.
// ---------------------------------------------------------------------------

test('6. a ReadWrite object with no writeAt is refused at construction, and no session starts', async () => {
  const t = teardown();
  const root = emptyRoot(t, 'validate');

  // Written the way a host would write it, so what the test proves is that the
  // *session never starts* — not merely that a constructor threw.
  let session: ReturnType<typeof newSession> | null = null;
  assert.throws(
    () => {
      const provider = registerProvider(make({ kind: 'readWriteWithoutWriteAt' }));
      session = newSession(t, 'validate');
      session.addRoot(0, 'game', root);
      session.mount(0, provider);
      session.serve();
    },
    (e: Error) => {
      assert.match(e.message, /must implement writeAt/);
      assert.match(e.message, /ReadWrite/);
      assert.match(e.message, /registration-time validation/);
      console.log(`    refused: ${e.message.split('.')[0]}.`);
      return true;
    }
  );
  assert.strictEqual(session, null, 'the session was never constructed, let alone started');

  // The same rule at the other access tier.
  assert.throws(
    () => registerProvider(make({ kind: 'seqReadWithoutReadNext' })),
    /must implement readNext/
  );

  // Missing a core method is the same defect with a different name. TypeScript
  // would refuse to *write* this object — `open` is required on `ProviderObject`,
  // so `delete` needs the cast — and that is the point: the runtime check is what
  // a JavaScript host and a dynamically assembled object still depend on.
  const noOpen: TestProvider = make({ kind: 'bytes' });
  delete (noOpen as Partial<TestProvider>).open;
  assert.throws(() => registerProvider(noOpen), /must implement open/);

  // Contradictory capabilities are refused in the same place.
  const rwImmutable: TestProvider = make({ kind: 'readWrite' });
  rwImmutable.capabilities = { access: 'readwrite', immutable: true };
  assert.throws(() => registerProvider(rwImmutable), /cannot be immutable/);

  // The positive control: with `writeAt` present it registers, reports
  // ReadWrite, and an injected child's write actually reaches it.
  const content = path.join(path.dirname(root), 'content');
  fs.mkdirSync(content, { recursive: true });
  fs.copyFileSync(probeExe, path.join(content, 'probe.exe'));
  const out = path.join(path.dirname(root), 'src.txt');
  fs.writeFileSync(out, 'written-into-javascript');

  const pw = await workerProvider(t, { kind: 'readWrite' });
  assert.strictEqual(pw.stats()!.access, 'readwrite');
  assert.ok(pw.stats()!.methods.includes('writeAt'));

  const s = newSession(t, 'validate-ok');
  s.addRoot(0, 'game', root);
  s.mount(0, disk(content));
  s.mount(0, pw.provider, 'js');
  assert.strictEqual(s.readFile('js/seed.txt').toString(), 'seed');

  const code = s.launch('probe.exe', {
    args: [out, path.join(root, 'js', 'from-the-child.bin')],
    wait: true,
  });
  assert.strictEqual(code, 0);
  assert.strictEqual(
    s.readFile('js/from-the-child.bin').toString(),
    'written-into-javascript',
    "the child's write was served by the JS provider's writeAt"
  );
  assert.deepStrictEqual(fs.readdirSync(root), [], 'and did not land on real disk under the root');
  show('read-write provider after a child write', pw.stats()!);
});

// ---------------------------------------------------------------------------
// 7. The deadlock guard.
// ---------------------------------------------------------------------------

test('7. a call serviced by the loop that is blocked waiting for it is refused, not hung', async () => {
  const t = teardown();
  const root = emptyRoot(t, 'guard');

  // Registered on *this* loop, then driven from this loop. Task 5 measured this
  // configuration as never settling (2 s timeout, `settled: false`).
  const p = mainLoopProvider(t, { kind: 'bytes' });
  const s = newSession(t, 'guard');
  s.addRoot(0, 'game', root);
  s.mount(0, p);

  const t0 = process.hrtime.bigint();
  assert.throws(
    () => s.readFile('js-served.txt'),
    (e: Error) => {
      assert.match(e.message, /would deadlock/);
      assert.match(e.message, /serviced by ThreadId/);
      assert.match(e.message, /same thread/);
      console.log(`    main → main-loop refused: ${e.message.split('.')[0]}.`);
      return true;
    }
  );
  const ms = Number(process.hrtime.bigint() - t0) / 1e6;
  assert.ok(ms < 500, `refused immediately rather than hung (${ms.toFixed(1)} ms)`);
  console.log(`    refusal took ${ms.toFixed(2)} ms (task 5 measured this configuration at 2014 ms timeout, never settling)`);

  const st = p.stats()!;
  assert.strictEqual(st.selfCallRefusals, 1);
  assert.strictEqual(st.calls, 0, 'nothing was queued — the guard runs before the tsfn call');
  assert.match(st.lastDiagnostic!, /must never be issued on the loop that services/);
  show('main-loop provider after a self-call', st);

  // `launch` resolves a relative image through the same graph on the same
  // thread, so it is refused with the same explanation rather than hanging.
  assert.throws(() => s.launch('js-served.txt', { wait: true }), /would deadlock|does not exist/);

  // The other half of the rule, and the half §8b's first wording got wrong: a
  // worker calling into its *own* loop is not the main thread and deadlocks all
  // the same.
  const workerRoot = emptyRoot(t, 'guard-worker');
  const result = await new Promise<SelfCallResult>((resolve, reject) => {
    const w = new Worker(path.join(import.meta.dirname, 'self-call-worker.mts'), {
      workerData: { gameRoot: workerRoot },
    });
    w.once('message', resolve);
    w.once('error', reject);
  });
  assert.strictEqual(result.setupError, undefined, result.setupError);
  assert.strictEqual(result.threw, true, 'worker A → worker A’s own loop must be refused too');
  assert.match(result.message!, /would deadlock/);
  assert.ok(result.elapsedMs! < 500, `and refused immediately (${result.elapsedMs} ms)`);
  assert.strictEqual(result.stats!.selfCallRefusals, 1);
  console.log(`    worker → its own loop refused in ${result.elapsedMs} ms (${result.ownerThread})`);

  // **The two assertions below were added by the vitest migration (task 3), and
  // they are the only additions in this conversion.** Everything above is the
  // node:test suite unchanged. They exist because task 1 measured that the guard
  // keys on the thread `registerProvider` ran on, and that under `pool:
  // 'threads'` that thread drifts run to run while the suite still reports
  // 36/36 — i.e. this test could pass while no longer testing loop identity.
  // These two make that a red test instead of a silent pass: the refusal must
  // name the *worker's own* thread, and that thread must not be the one the
  // main-loop provider above is bound to.
  assert.ok(
    result.message!.includes(`serviced by ${result.ownerThread}`),
    `the refusal must name the worker's own loop (${result.ownerThread}), not another: ${result.message}`
  );
  assert.notStrictEqual(
    result.ownerThread,
    st.ownerThread,
    'a worker provider and a main-loop provider must be on different loops, or this ' +
      'test is not the worker half of the rule'
  );

  // And the control that makes the guard mean loop identity rather than "no
  // calls from the main thread": the same session pattern with the provider on a
  // worker succeeds from this very thread.
  const okRoot = emptyRoot(t, 'guard-control');
  const pw = await workerProvider(t, { kind: 'bytes' });
  const s2 = newSession(t, 'guard-control');
  s2.addRoot(0, 'game', okRoot);
  s2.mount(0, pw.provider);
  assert.strictEqual(s2.readFile('js-served.txt').toString(), 'bytes-from-javascript');
  assert.strictEqual(pw.stats()!.selfCallRefusals, 0);
  console.log('    main → worker on the same call path: served, 0 refusals');
});

// ---------------------------------------------------------------------------
// A cost sanity check. Not a benchmark — task 5's harness is the benchmark — but
// enough to notice if the boundary regressed from microseconds to milliseconds,
// which is the only kind of regression that would change the design.
//
// **What this number is not.** A `readFile` is open + read + close through the
// whole director stack, so dividing its wall time by three crossings attributes
// the director's own work to the boundary. It is an *upper bound* on the crossing
// cost and is not comparable to task 5’s 1.7–2.0 µs bare round trip, which is
// recorded in docs/benchmarks/node-ffi-round-trip.md. The harness that produced
// it (`spike-node/`) has been deleted, so nothing in the tree measures the bare
// crossing any more — this upper bound is what is left, and it is enough for the
// only regression that would change the design.
//
// The `disk()` row is here for scale rather than as a baseline to subtract:
// reading the same 64 bytes off NTFS is not the same leaf work as slicing a
// Buffer, and the difference comes out *negative* — which is the useful finding.
// A JS provider is not the expensive part of a read.
// ---------------------------------------------------------------------------

test('a readFile through a JS provider stays in the tens of microseconds', async () => {
  const t = teardown();
  const dir = scratch(t, 'cost');
  const rustRoot = path.join(dir, 'rust-root');
  const jsRoot = path.join(dir, 'js-root');
  const content = path.join(dir, 'content');
  for (const d of [rustRoot, jsRoot, content]) fs.mkdirSync(d, { recursive: true });
  // The same 64 bytes the JS fixture serves, so the leaf work is comparable.
  fs.writeFileSync(path.join(content, 'small.bin'), Buffer.alloc(64, 0xab));

  const rust = newSession(t, 'cost-rust');
  rust.addRoot(0, 'game', rustRoot);
  rust.mount(0, disk(content));

  const pw = await workerProvider(t, { kind: 'bytes' });
  const js = newSession(t, 'cost-js');
  js.addRoot(0, 'game', jsRoot);
  js.mount(0, pw.provider);

  const iterations = 4000;
  const time = (session: ReturnType<typeof newSession>): number => {
    for (let i = 0; i < 200; i++) session.readFile('small.bin'); // warm
    const t0 = process.hrtime.bigint();
    for (let i = 0; i < iterations; i++) session.readFile('small.bin');
    return Number(process.hrtime.bigint() - t0) / 1e6;
  };

  const before = pw.stats()!.calls;
  const jsMs = time(js);
  const crossings = pw.stats()!.calls - before;
  const rustMs = time(rust);

  const usPerReadFileJs = (jsMs * 1000) / iterations;
  const usPerReadFileRust = (rustMs * 1000) / iterations;
  console.log(`    ${iterations} readFile('small.bin'), 64 bytes, same graph shape:`);
  console.log(
    `      JS provider on a worker   ${usPerReadFileJs.toFixed(2)} µs per readFile ` +
      `(${(crossings / iterations).toFixed(1)} bridge crossings each, so < ${(usPerReadFileJs / (crossings / iterations)).toFixed(2)} µs per crossing)`
  );
  console.log(
    `      Rust disk() on real NTFS  ${usPerReadFileRust.toFixed(2)} µs per readFile ` +
      `— for scale, not as a baseline: the JS provider is ${(usPerReadFileRust / usPerReadFileJs).toFixed(1)}× faster ` +
      'because its leaf is a Buffer slice and disk()’s is a real file open'
  );
  assert.ok(crossings >= iterations * 3, 'open + readAt + close each crossed');
  // The regression this guards against is milliseconds, which would mean the
  // round trip had stopped being an event-loop hop and started being a queue.
  assert.ok(
    usPerReadFileJs < 500,
    `a readFile through JS is tens of microseconds (${usPerReadFileJs.toFixed(2)} µs)`
  );
});

// §3 — the JavaScript provider bridge, and a live boundary number at last.
//
// `docs/benchmarks/node-ffi-round-trip.md` records 1.7–2.0 µs for a blocking
// round trip, director thread → JS → back, and carries a banner saying the
// harness that produced it (`spike-node/`) was deleted and **nothing in the tree
// reproduces it**. `provider.test.cts` says the same in its own comment: its
// ~63 µs per `readFile` is an upper bound over three-plus crossings and "is not
// comparable" to the bare number.
//
// This narrows that gap without pretending to close it. The trick is `getattr`:
// it is exactly **one** provider call, and the same `session.getattr` against a
// Rust `memory()` leaf costs the same graph traversal with **zero** crossings. The
// difference is one round trip plus one dispatch, measured rather than recalled.
// The crossing counts are read from `provider.stats().calls`, so the divisor is a
// fact and not an assumption — which is the part `provider.test.cts` could not do
// when it divided by "3.1 crossings each".
//
// The provider lives on a worker loop, which is the shape §8c measured as the
// only one immune to a busy main loop, and the only one a host should ship.

import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';

import {
  PKG_DIR,
  assertAtLeast,
  assertAtMostNs,
  assertAtMostRatio,
  assertExact,
  benchRequire,
  fmtNs,
  heading,
  measure,
  sink,
  table,
} from '../harness.mts';

type Vfs = typeof import('../../index.cjs');

/** The task-7 fixture: `bytes` serves `small.bin` as 64 bytes of 0xab. */
const FIXTURE = path.join(PKG_DIR, 'test', 'providers.cts');

export async function run(): Promise<void> {
  const vfs = benchRequire(path.join(PKG_DIR, 'index.cjs')) as Vfs;

  heading('3. the JavaScript provider bridge');

  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'aethervfs-bench-js-'));
  const jsRoot = path.join(scratch, 'js-root');
  const memRoot = path.join(scratch, 'mem-root');
  for (const d of [jsRoot, memRoot]) fs.mkdirSync(d, { recursive: true });

  const pw = await vfs.providerWorker({ module: FIXTURE, options: { kind: 'bytes' } });
  const jsSession = new vfs.Session('bench-js');
  jsSession.addRoot(0, 'game', jsRoot);
  jsSession.mount(0, pw.provider);

  // The control: the same 64 bytes from a Rust leaf, same graph shape, no bridge.
  const memSession = new vfs.Session('bench-js-control');
  memSession.addRoot(0, 'game', memRoot);
  memSession.mount(0, vfs.memory({ 'small.bin': Buffer.alloc(64, 0xab) }));

  process.stdout.write(`  provider ${pw.handle} serviced by ${pw.stats()!.ownerThread}\n`);

  // ---- crossings per operation, counted rather than assumed --------------
  const crossings = (fn: () => void): number => {
    const before = pw.stats()!.calls;
    fn();
    return pw.stats()!.calls - before;
  };
  const perGetattr = crossings(() => sink(jsSession.getattr('small.bin')?.size));
  const perReadFile = crossings(() => sink(jsSession.readFile('small.bin').length));
  process.stdout.write(
    `  crossings: getattr ${perGetattr}, readFile ${perReadFile}   (from provider.stats().calls)\n`
  );

  assertExact(
    'a host-side getattr is exactly one provider call',
    perGetattr,
    1,
    'this is what makes the boundary estimate below a division by a known number rather than a guess'
  );
  assertAtLeast(
    'a host-side readFile is at least open + read + close',
    perReadFile,
    3,
    'fewer would mean the director answered without asking the provider, i.e. served stale bytes'
  );

  // ---- the measurements ---------------------------------------------------
  const jsGetattr = measure('getattr    JS provider (worker loop)', () => sink(jsSession.getattr('small.bin')?.size));
  const memGetattr = measure('getattr    memory()   (Rust leaf)', () => sink(memSession.getattr('small.bin')?.size));
  const jsRead = measure('readFile   JS provider   64 B', () => sink(jsSession.readFile('small.bin').length));
  const memRead = measure('readFile   memory()      64 B', () => sink(memSession.readFile('small.bin').length));
  table();

  const perCrossing = (jsGetattr.nsPerOp - memGetattr.nsPerOp) / perGetattr;
  const perCrossingBest = (jsGetattr.minNsPerOp - memGetattr.minNsPerOp) / perGetattr;
  process.stdout.write(
    `\n  boundary crossing, by difference (getattr JS - getattr memory) / ${perGetattr}:\n` +
      `    median ${fmtNs(perCrossing)}   best ${fmtNs(perCrossingBest)}\n` +
      '    for scale: docs/benchmarks/node-ffi-round-trip.md recorded 1.7-2.0 µs for the bare\n' +
      '    blocking round trip, on this machine, release, from a harness that no longer exists.\n' +
      '    This number includes the dispatcher and the graph delta, so it is an upper bound on it.\n'
  );

  assertAtMostNs(
    'a provider-call boundary crossing stays in the low microseconds',
    perCrossing,
    25_000,
    'the historical bare round trip was 1.7-2.0 µs; this estimate carries the dispatcher too. The ' +
      'regression worth catching is the boundary becoming a queue rather than an event-loop hop'
  );
  assertAtMostRatio(
    'a JS provider is within an order of magnitude of a Rust leaf',
    jsRead.nsPerOp / memRead.nsPerOp,
    30,
    'spec §8b bets that a host-language provider is a reasonable thing to build. The recorded ' +
      'finding was ~20% on a 4 KiB ring read; host-side against an in-memory Rust leaf is the ' +
      'harshest possible comparison, because memory() does almost nothing'
  );
  assertAtMostNs(
    'a readFile through a JS provider stays in the tens of microseconds',
    jsRead.nsPerOp,
    500_000,
    'the same ceiling provider.test.cts asserts, so the two agree on what a regression looks like'
  );

  jsSession.close();
  memSession.close();
  await pw.close();
  for (const s of [jsSession, memSession]) fs.rmSync(s.baseDir, { recursive: true, force: true });
  fs.rmSync(scratch, { recursive: true, force: true });
}

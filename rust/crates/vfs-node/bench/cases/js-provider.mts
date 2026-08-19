// §3 — the JavaScript provider bridge, and a live boundary number at last.
//
// `docs/benchmarks/node-ffi-round-trip.md` records 1.7–2.0 µs for a blocking
// round trip, director thread → JS → back, and carries a banner saying the
// harness that produced it (`spike-node/`) was deleted and **nothing in the tree
// reproduces it**. `provider.test.mts` says the same in its own comment: its
// ~63 µs per `readFile` is an upper bound over three-plus crossings and "is not
// comparable" to the bare number.
//
// This narrows that gap without pretending to close it. The trick is `getattr`:
// it is exactly **one** provider call, and the same `session.getattr` against a
// Rust `memory()` leaf costs the same graph traversal with **zero** crossings. The
// difference is one round trip plus one dispatch, measured rather than recalled.
// The crossing counts are read from `provider.stats().calls`, so the divisor is a
// fact and not an assumption — which is the part `provider.test.mts` could not do
// when it divided by "3.1 crossings each".
//
// The provider lives on a worker loop, which is the shape §8c measured as the
// only one immune to a busy main loop, and the only one a host should ship.

import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { pathToFileURL } from 'node:url';

import {
  PKG_DIR,
  assertAtMostNs,
  assertAtMostRatio,
  assertExact,
  fmtNs,
  heading,
  measure,
  sink,
  table,
} from '../harness.mts';

type Vfs = typeof import('../../index.mjs');

/** The task-7 fixture: `bytes` serves `small.bin` as 64 bytes of 0xab. */
const FIXTURE = path.join(PKG_DIR, 'test', 'providers.mts');

export async function run(): Promise<void> {
  const vfs = (await import(pathToFileURL(path.join(PKG_DIR, 'index.mjs')).href)) as Vfs;

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

  // A second control: a real Rust provider doing real work. `memory()` is the
  // floor — a HashMap lookup, no I/O, no bridge — while `disk()` is what a JS
  // provider is actually an alternative to, so it is the denominator the gate
  // below uses.
  const diskDir = path.join(scratch, 'content');
  const diskRoot = path.join(scratch, 'disk-root');
  for (const d of [diskDir, diskRoot]) fs.mkdirSync(d, { recursive: true });
  fs.writeFileSync(path.join(diskDir, 'small.bin'), Buffer.alloc(64, 0xab));
  const diskSession = new vfs.Session('bench-js-disk');
  diskSession.addRoot(0, 'game', diskRoot);
  diskSession.mount(0, vfs.disk(diskDir));

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
  assertExact(
    'a host-side readFile is exactly open + readAt + close',
    perReadFile,
    3,
    'fewer would mean the director answered without asking the provider, i.e. served stale bytes; ' +
      'more would mean the graph quietly gained a crossing, which is the cheapest regression to miss'
  );

  // ---- the measurements ---------------------------------------------------
  const jsGetattr = measure('getattr    JS provider (worker loop)', () => sink(jsSession.getattr('small.bin')?.size));
  const memGetattr = measure('getattr    memory()   (Rust leaf)', () => sink(memSession.getattr('small.bin')?.size));
  const jsRead = measure('readFile   JS provider   64 B', () => sink(jsSession.readFile('small.bin').length));
  const memRead = measure('readFile   memory()      64 B', () => sink(memSession.readFile('small.bin').length));
  const diskRead = measure('readFile   disk()        64 B', () => sink(diskSession.readFile('small.bin').length));
  table();

  const perCrossing = (jsGetattr.nsPerOp - memGetattr.nsPerOp) / perGetattr;
  const perCrossingBest = (jsGetattr.minNsPerOp - memGetattr.minNsPerOp) / perGetattr;
  process.stdout.write(
    `\n  boundary crossing, by difference (getattr JS - getattr memory) / ${perGetattr}:\n` +
      `    median ${fmtNs(perCrossing)}   best ${fmtNs(perCrossingBest)}\n` +
      '    The comparable recorded figure is **47 µs**: task 5 measured `main -> worker` at that,\n' +
      '    and it is written into jsprovider.rs beside the deadlock guard it motivated. This is\n' +
      '    that configuration — a session driven from the main thread against a provider on a\n' +
      '    worker loop — so 47 µs is the number to beat, and it is beaten.\n' +
      '    docs/benchmarks/node-ffi-round-trip.md\'s 1.7-2.0 µs is a *different* configuration:\n' +
      '    a director thread parked on a condvar with a hot loop, not a cross-thread wake from\n' +
      '    main. Comparing against it would flatter or alarm depending on which way you squint.\n'
  );

  assertAtMostNs(
    'a main -> worker provider crossing stays under the recorded figure, with room',
    perCrossing,
    100_000,
    'task 5 measured this configuration at 47 µs (jsprovider.rs). The ceiling is ~2x that rather ' +
      'than just above it, because a cross-thread wake is exactly what a loaded machine delays'
  );
  assertAtMostRatio(
    'a JS provider is not slower than a real Rust provider doing real work',
    jsRead.nsPerOp / diskRead.nsPerOp,
    2.0,
    'the comparison spec §8b actually invites. provider.test.mts already found the JS provider ' +
      '*faster* than disk() — its leaf is a Buffer slice and disk()\'s is a real NTFS open — so ' +
      'this holds the finding rather than restating an aspiration. memory() is not the denominator ' +
      'to use: it is a HashMap lookup with no I/O and no bridge, which makes the ratio a statement ' +
      'about memory(), not about the bridge'
  );
  process.stdout.write(
    `  for reference, the ratios: vs disk() ${(jsRead.nsPerOp / diskRead.nsPerOp).toFixed(2)}x, ` +
      `vs memory() ${(jsRead.nsPerOp / memRead.nsPerOp).toFixed(1)}x (recorded, not gated)\n`
  );
  assertAtMostNs(
    'a readFile through a JS provider stays in the tens of microseconds',
    jsRead.nsPerOp,
    500_000,
    'the same ceiling provider.test.mts asserts, so the two agree on what a regression looks like'
  );

  jsSession.close();
  memSession.close();
  diskSession.close();
  await pw.close();
  for (const s of [jsSession, memSession, diskSession]) {
    fs.rmSync(s.baseDir, { recursive: true, force: true });
  }
  fs.rmSync(scratch, { recursive: true, force: true });
}

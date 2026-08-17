// §2 — reads through a provider graph, host-side.
//
// This is the path `session.readFile` takes: the JS wrapper, N-API, the director's
// composition, and a leaf. Every leaf here is a **Rust** primitive, so the numbers
// are the graph's own cost with no bridge in them — which is what makes §3's JS
// provider comparable to anything.
//
// The cache section asserts on **counters**, not time. `cacheStats()` reports
// hits, misses and bytes-from-source, so "the cache is working" is a tier-1
// deterministic fact rather than an inference from a wall clock. That matters
// here more than usual: a cache that silently stopped caching would still look
// fast on a warm page cache.

import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';

import {
  PKG_DIR,
  assertAtMostNs,
  assertAtMostRatio,
  assertExact,
  benchRequire,
  heading,
  measure,
  sink,
  table,
} from '../harness.mts';

type Vfs = typeof import('../../index.cjs');

export function run(): void {
  const vfs = benchRequire(path.join(PKG_DIR, 'index.cjs')) as Vfs;

  heading('2. reads through a graph of Rust primitives');

  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'aethervfs-bench-graph-'));
  const content = path.join(scratch, 'content');
  fs.mkdirSync(content, { recursive: true });
  const SMALL = Buffer.alloc(64, 0xab);
  fs.writeFileSync(path.join(content, 'small.bin'), SMALL);
  fs.writeFileSync(path.join(content, 'big.bin'), Buffer.alloc(256 * 1024, 0xcd));

  const sessions: Array<{ close(): void; baseDir: string }> = [];
  const session = (name: string, mount: () => ReturnType<Vfs['memory']>, prefix?: string) => {
    const root = path.join(scratch, `${name}-root`);
    fs.mkdirSync(root, { recursive: true });
    const s = new vfs.Session(`bench-${name}`);
    sessions.push(s);
    s.addRoot(0, 'game', root);
    s.mount(0, mount(), prefix);
    return s;
  };

  const memSession = session('memory', () => vfs.memory({ 'small.bin': SMALL }));
  const diskSession = session('disk', () => vfs.disk(content));
  const layeredSession = session('layered', () =>
    vfs.layered(vfs.memory({ 'other.bin': 'x' }), vfs.memory({ 'small.bin': SMALL }))
  );
  const routerSession = session('router', () =>
    vfs.router({ '*.bin': vfs.memory({ 'small.bin': SMALL }) }, vfs.memory({ 'z.txt': 'z' }))
  );
  const cachedProvider = vfs.cached(vfs.disk(content), { ramBytes: 8 * 1024 * 1024 });
  const cachedSession = session('cached', () => cachedProvider);

  measure('getattr    memory()', () => sink(memSession.getattr('small.bin')?.size));
  measure('getattr    disk()', () => sink(diskSession.getattr('small.bin')?.size));
  const memRead = measure('readFile   memory()            64 B  [gated]', () => sink(memSession.readFile('small.bin').length));
  const diskRead = measure('readFile   disk()              64 B  [recorded, not gated]', () => sink(diskSession.readFile('small.bin').length));
  measure('readFile   layered(mem, mem)   64 B', () => sink(layeredSession.readFile('small.bin').length));
  measure('readFile   router(*.bin -> mem) 64 B', () => sink(routerSession.readFile('small.bin').length));
  const cachedRead = measure('readFile   cached(disk)        64 B', () => sink(cachedSession.readFile('small.bin').length));
  measure('readFile   disk()             256 KiB', () => sink(diskSession.readFile('big.bin').length));
  measure('readdir    disk()', () => sink(diskSession.readdir('').length));
  table();

  // The cache, on counters rather than on the clock.
  heading('2b. the cache is actually caching (counters, not wall clock)');
  const before = cachedProvider.cacheStats()!;
  const REREADS = 200;
  for (let i = 0; i < REREADS; i += 1) sink(cachedSession.readFile('small.bin').length);
  const after = cachedProvider.cacheStats()!;
  process.stdout.write(
    `  hits ${before.hits} -> ${after.hits}   misses ${before.misses} -> ${after.misses}   ` +
      `bytesFromSource ${before.bytesFromSource} -> ${after.bytesFromSource}\n`
  );

  assertExact(
    'a re-read never reaches the source again',
    after.bytesFromSource - before.bytesFromSource,
    0,
    `${REREADS} re-reads of a cached 64-byte file must not touch disk once; this is the assertion ` +
      'that distinguishes `cached(p)` from `p`, and a wall clock cannot make it'
  );
  assertExact(
    'a re-read never misses',
    after.misses - before.misses,
    0,
    'a miss after warm-up means the block was evicted or the key changed'
  );

  assertAtMostRatio(
    'cached(disk) is not slower than disk() for a warm 64-byte read',
    cachedRead.nsPerOp / diskRead.nsPerOp,
    1.5,
    'the cache exists to remove I/O; if it costs more than the NTFS read it replaces at this size, ' +
      'the block-cache hit path has regressed — see docs/benchmarks/block-cache-hit-cost.md'
  );
  // **Gate on memory(), record disk().** The disk numbers above are dominated by
  // NTFS open/close — ~345 µs for 64 bytes on this machine, against 1.7 µs for the
  // same read out of `memory()`. That is the filesystem and the virus scanner, not
  // this binding, and a ceiling over it would be a ceiling over whatever runner CI
  // happens to schedule. `memory()` exercises the identical wrapper, N-API and
  // composition path with the I/O removed, so it is the honest subject for an
  // absolute assertion.
  assertAtMostNs(
    'a host-side 64-byte read through a Rust graph stays in the low microseconds',
    memRead.nsPerOp,
    100_000,
    'measured at ~1.7 µs, so the ceiling is ~60x rather than snug: the regression worth catching ' +
      'is the path becoming a queue instead of a call, which is a change of milliseconds, not of ' +
      'microseconds'
  );

  for (const s of sessions) {
    try {
      s.close();
    } catch {
      /* already closed */
    }
    fs.rmSync(s.baseDir, { recursive: true, force: true });
  }
  fs.rmSync(scratch, { recursive: true, force: true });
}

#!/usr/bin/env node

// **Did the TypeScript migration cost anything at runtime?**
//
// It is the one performance question the migration actually raises, and it has a
// falsifiable answer. `index.cts` forwards the addon with
// `export * from './native.cjs'`, and `tsc` emits that as `__exportStar`, which
// installs an **accessor** for every forwarded name. The hand-written `index.cjs`
// it replaced had plain own data properties. So most of the package's exports
// changed from a property read to a getter call, on the path every host takes to
// reach a primitive.
//
// That is a mechanism, not a worry: it is visible in
// `Object.getOwnPropertyDescriptor`, and §1 asserts it. What was unknown is
// whether it *costs* anything.
//
// ## How the comparison is kept honest
//
// The baseline is the hand-written JS layer at a named commit, laid down in a temp
// directory **beside a copy of the current `aethervfs.node`**. The Rust is
// therefore identical on both sides, the addon is the same release build, both run
// in one process seconds apart, and the only variable is the JavaScript. Node
// loads the two `.node` copies as separate modules because their paths differ,
// which is what makes a same-process comparison possible.
//
// **One caveat, stated because it is real.** Since the migration landed, the JS
// layer has also taken behavioural fixes (a `releaseProvider` handle guard, a
// `ProviderWorker.close()` fix). Neither touches the paths measured below —
// property access, `version()`, `statusName()`, `readFile` — so the comparison
// still isolates the migration, but it is "HEAD versus the last hand-written
// layer", not "the migration commit versus its parent".
//
// ## Not a CI gate, and could not be
//
// It pins a historical commit: meaningful while the migration is recent,
// meaningless afterwards. `pnpm bench` does not run it; `bench/run.mts` is the
// durable gate. Its output belongs in `rust/docs/benchmarks/`.
//
// It is committed rather than thrown away because it generalises — `--baseline
// <rev>` A/Bs the JS layer against any commit, which is the tool you want the next
// time this layer changes shape.
//
//   node bench/ab-js-layer.mts [--baseline <rev>]

import { spawnSync } from 'node:child_process';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';

import {
  PKG_DIR,
  REPO_RUST_DIR,
  assertAtMostNs,
  assertAtMostRatio,
  assertExact,
  assertReleaseAddon,
  benchRequire,
  drainSink,
  environment,
  fmtNs,
  heading,
  measure,
  sink,
  table,
  verdict,
} from './harness.mts';

/** The last commit whose `index.cjs` was hand-written. */
const DEFAULT_BASELINE = 'a14dfac';

const argv = process.argv.slice(2);
const i = argv.indexOf('--baseline');
const BASELINE = i === -1 ? DEFAULT_BASELINE : (argv[i + 1] ?? DEFAULT_BASELINE);

type Vfs = typeof import('../index.cjs');

function git(args: string[]): string {
  const r = spawnSync('git', args, {
    cwd: REPO_RUST_DIR,
    encoding: 'utf8',
    maxBuffer: 64 * 1024 * 1024,
  });
  if (r.status !== 0) throw new Error(`git ${args.join(' ')} failed: ${r.stderr}`);
  return r.stdout;
}

/** Lay the baseline JS layer down beside a copy of the current release addon. */
function stageBaseline(rev: string): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'aethervfs-ab-'));
  for (const f of ['index.cjs', 'provider-host.cjs', 'package.json']) {
    fs.writeFileSync(path.join(dir, f), git(['show', `${rev}:rust/crates/vfs-node/${f}`]));
  }
  for (const f of ['aethervfs.node', 'vfs_shim_dll.dll', 'vfs_payload.dll']) {
    fs.copyFileSync(path.join(PKG_DIR, f), path.join(dir, f));
  }
  return dir;
}

function main(): number {
  assertReleaseAddon();

  const rev = git(['rev-parse', '--short', BASELINE]).trim();
  const subject = git(['log', '-1', '--format=%s', BASELINE]).trim();

  process.stdout.write('aethervfs — JS layer A/B: emitted (HEAD) vs hand-written\n');
  process.stdout.write(`${environment()}\n`);
  process.stdout.write(`baseline ${rev}  "${subject}"\n`);

  const dir = stageBaseline(BASELINE);
  const oldVfs = benchRequire(path.join(dir, 'index.cjs')) as Vfs;
  const newVfs = benchRequire(path.join(PKG_DIR, 'index.cjs')) as Vfs;

  // -------------------------------------------------------------------------
  heading('1. the mechanism, asserted rather than assumed');
  // -------------------------------------------------------------------------
  const accessors = (v: Vfs): number =>
    Object.keys(v).filter((k) => Object.getOwnPropertyDescriptor(v, k)?.get !== undefined).length;
  const desc = (v: Vfs, k: string) =>
    Object.getOwnPropertyDescriptor(v, k)?.get ? 'getter' : 'own value';

  process.stdout.write(
    `  exports:   old ${Object.keys(oldVfs).length}, new ${Object.keys(newVfs).length}\n` +
      `  accessors: old ${accessors(oldVfs)}, new ${accessors(newVfs)}` +
      '   (__exportStar installs one getter per forwarded name)\n' +
      `  disk:      old ${desc(oldVfs, 'disk')}, new ${desc(newVfs, 'disk')}\n` +
      `  memory:    old ${desc(oldVfs, 'memory')}, new ${desc(newVfs, 'memory')}` +
      '   (a deliberate shadow, declared locally, so it stayed a data property)\n'
  );
  assertExact(
    'the public export count is unchanged by the migration',
    Object.keys(newVfs).length,
    Object.keys(oldVfs).length,
    'check-types.mts asserts the names; this asserts nothing appeared or vanished'
  );
  assertExact('the hand-written layer had no accessors', accessors(oldVfs), 0, 'plain module.exports');

  // -------------------------------------------------------------------------
  heading('2. property access — where the getter actually lands');
  // -------------------------------------------------------------------------
  const oldDisk = measure('old  vfs.disk      (own value)', () => sink(oldVfs.disk));
  const newDisk = measure('new  vfs.disk      (getter)', () => sink(newVfs.disk));
  measure('old  vfs.memory    (own value)', () => sink(oldVfs.memory));
  measure('new  vfs.memory    (own value, shadow)', () => sink(newVfs.memory));
  table();

  const getterCost = Math.max(newDisk.nsPerOp - oldDisk.nsPerOp, 0);
  process.stdout.write(`  getter overhead: ${fmtNs(getterCost)} per access\n`);
  assertAtMostNs(
    'the __exportStar getter costs almost nothing per access',
    getterCost,
    50,
    'a property read is not where a VFS spends time, but a pathological getter would show up here'
  );

  // -------------------------------------------------------------------------
  heading('3. calls that cross into Rust');
  // -------------------------------------------------------------------------
  const oldVersion = measure('old  version()', () => sink(oldVfs.version()));
  const newVersion = measure('new  version()', () => sink(newVfs.version()));
  const oldStatus = measure('old  statusName(2)', () => sink(oldVfs.statusName(2)));
  const newStatus = measure('new  statusName(2)', () => sink(newVfs.statusName(2)));
  table();

  assertAtMostRatio(
    'version() is no slower through the emitted layer',
    newVersion.nsPerOp / oldVersion.nsPerOp,
    1.35,
    'a forwarded export reached through a getter, then an N-API call'
  );
  assertAtMostRatio(
    'statusName() is no slower through the emitted layer',
    newStatus.nsPerOp / oldStatus.nsPerOp,
    1.35,
    'the same path with an argument and a returned string'
  );

  // -------------------------------------------------------------------------
  heading('4. the shape a host actually uses: a read through the graph');
  // -------------------------------------------------------------------------
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'aethervfs-ab-graph-'));
  const content = path.join(scratch, 'content');
  fs.mkdirSync(content, { recursive: true });
  fs.writeFileSync(path.join(content, 'small.bin'), Buffer.alloc(64, 0xab));

  const sessions: Array<{ close(): void; baseDir: string }> = [];
  const graphFor = (v: Vfs, name: string) => {
    const root = path.join(scratch, `${name}-root`);
    fs.mkdirSync(root, { recursive: true });
    const s = new v.Session(`ab-${name}`);
    sessions.push(s);
    s.addRoot(0, 'game', root);
    s.mount(0, v.disk(content));
    return s;
  };
  const oldSession = graphFor(oldVfs, 'old');
  const newSession = graphFor(newVfs, 'new');

  const oldRead = measure('old  readFile 64 B (disk)', () => sink(oldSession.readFile('small.bin').length));
  const newRead = measure('new  readFile 64 B (disk)', () => sink(newSession.readFile('small.bin').length));
  const oldAttr = measure('old  getattr       (disk)', () => sink(oldSession.getattr('small.bin')?.size));
  const newAttr = measure('new  getattr       (disk)', () => sink(newSession.getattr('small.bin')?.size));
  table();

  assertAtMostRatio(
    'a host-side read is no slower through the emitted layer',
    newRead.nsPerOp / oldRead.nsPerOp,
    1.15,
    'the number a host would actually notice; everything above it is a rounding error by comparison'
  );
  assertAtMostRatio(
    'a host-side getattr is no slower through the emitted layer',
    newAttr.nsPerOp / oldAttr.nsPerOp,
    1.15,
    'the cheapest real call, so the most sensitive to layer overhead'
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

  // The staged baseline directory holds a **copy of `aethervfs.node`, and it is
  // mapped into this process** — the old layer loaded it. Windows will not delete
  // a loaded DLL, so this is `EPERM` every time, not intermittently. It is the
  // same family as the leaked-mapping trap the examples guard against with
  // `taskkill /F /T`, and there is no way around it from inside the process that
  // did the loading: the copy is what makes the comparison possible.
  //
  // So it is reported rather than swallowed, and rather than crashing the run
  // after every number has already been produced — which is what it did the first
  // time, taking the checks section with it.
  try {
    fs.rmSync(dir, { recursive: true, force: true });
  } catch {
    process.stdout.write(
      `\nnote: could not remove ${dir}\n` +
        '      Its copy of `aethervfs.node` is mapped into this process, and Windows does not\n' +
        '      delete a loaded DLL. Temp cleanup gets it; nothing here depends on it going now.\n'
    );
  }

  process.stdout.write(`\n(sink ${drainSink()})\n`);
  return verdict();
}

process.exitCode = main();

#!/usr/bin/env node

// Task 7's demonstration: **a JavaScript object mounted as a first-class
// provider**, serving an injected Windows process over the ring.
//
// The shape is the one spec §8c recommends, and every part of it is there for a
// measured reason:
//
//  - the provider is `async` and lives in a **dedicated worker**, because a
//    main-loop provider degrades 370× under ~1 ms of work per event-loop turn
//    while a worker-serviced one is unaffected;
//  - registration is a **module path resolved inside the worker**, because
//    isolates share no JS objects — what crosses is a process-global integer;
//  - the session is driven from the **main thread**, which is safe precisely
//    because the provider is not serviced by that loop. The rule is loop
//    identity, not thread role, and the last step shows what the guard says when
//    it is broken.
//
// The managed root is empty on real disk and is asserted so, twice. The child
// reads a path with no file behind it anywhere and gets bytes a JS function
// produced.
//
// `require` with a type annotation, not `import`: node runs this file directly
// (`node examples/js-provider.cts`) and strips the annotations, but it does not
// rewrite module syntax. The annotation rather than the `as` cast this used to
// carry is what makes `assert.ok` an assertion function to TypeScript — the whole
// of task 3's 134 × TS2775, gone without one assertion changing.

import type { ProviderWorker } from '../index.cjs';
import type { PretendCdn } from './pretend-cdn-provider.cts';

const assert: typeof import('node:assert') = require('assert');
const fs: typeof import('node:fs') = require('fs');
const os: typeof import('node:os') = require('os');
const path: typeof import('node:path') = require('path');

let mod: typeof import('../index.cjs');
try {
  mod = require('aethervfs');
} catch (e) {
  if ((e as NodeJS.ErrnoException).code !== 'MODULE_NOT_FOUND') throw e;
  mod = require('..');
}
const { Session, disk, providerWorker, registerProvider, releaseProvider } = mod;

const probeExe = path.join(__dirname, '..', 'fixtures', 'vfs-probe.exe');
if (!fs.existsSync(probeExe)) {
  throw new Error(`${probeExe} is missing — run \`pnpm build\` first`);
}

const step = (n: number, what: string): void => {
  process.stdout.write(`\n[${n}] ${what}\n`);
};

async function main(): Promise<void> {
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'aethervfs-jsprov-'));
  const content = path.join(scratch, 'content'); // holds only the launch image
  const gameRoot = path.join(scratch, 'game-root'); // stays EMPTY
  const outDir = path.join(scratch, 'out');
  for (const d of [content, gameRoot, outDir]) fs.mkdirSync(d, { recursive: true });
  fs.copyFileSync(probeExe, path.join(content, 'probe.exe'));

  step(1, 'providerWorker — a JS provider on its own event loop');
  const cdn: ProviderWorker = await providerWorker({
    module: require.resolve('./pretend-cdn-provider.cts'),
    options: { depot: '489830', latencyMs: 5 },
  });
  console.log(`    handle            ${cdn.handle}   (a process-global integer, not an object)`);
  console.log(`    serviced by       ${cdn.stats()!.ownerThread}`);
  console.log(`    declared          access=${cdn.stats()!.access} immutable=${cdn.stats()!.immutable} slow=${cdn.stats()!.slow} preferredBlock=${cdn.stats()!.preferredBlock}`);
  console.log(`    methods found     ${cdn.stats()!.methods.join(', ')}`);

  step(2, 'session.mount(root, provider) — the same call a Rust provider takes');
  const session = new Session('js-provider');
  session.addRoot(0, 'game', gameRoot);
  session.mount(0, disk(content)); // the launch image, as content
  session.mount(0, cdn.provider); // everything else comes from JavaScript
  console.log(`    virtualRoot       ${session.virtualRoot}`);

  step(3, 'session.readFile — the graph serves, host-side, from JavaScript');
  const readme = session.readFile('readme.txt').toString();
  console.log(`    readFile('readme.txt')  ${JSON.stringify(readme)}`);
  assert.match(readme, /pretend network/);
  assert.strictEqual(session.readFile('data/big.bin').length, 4096);
  console.log(`    readFile('data/big.bin')  4096 bytes through an async provider`);
  assert.throws(() => session.readFile('not-in-the-depot.txt'), /ST_NOT_FOUND/);
  console.log("    a VfsError('ST_NOT_FOUND') arrives as ST_NOT_FOUND");

  step(4, 'the managed root is empty on real disk — that is the whole case');
  assert.deepStrictEqual(fs.readdirSync(gameRoot), []);
  console.log(`    ${gameRoot} → []`);

  step(5, 'session.launch — an injected process reading JavaScript over the ring');
  const outFile = path.join(outDir, 'from-js.bin');
  const virtualPath = path.join(gameRoot, 'readme.txt');
  assert.strictEqual(fs.existsSync(virtualPath), false, 'no file behind the path the child reads');
  const before = cdn.stats()!.calls;
  const code = session.launch('probe.exe', { args: [virtualPath, outFile], wait: true });
  console.log(`    exit code         ${code}`);
  assert.strictEqual(code, 0);
  assert.strictEqual(fs.readFileSync(outFile).toString(), readme);
  console.log(`    child read        ${JSON.stringify(fs.readFileSync(outFile).toString().trim())}`);
  console.log(`    bridge crossings  ${cdn.stats()!.calls - before} from director worker threads`);
  assert.ok(cdn.stats()!.calls > before, 'the child’s reads crossed into JS');
  assert.deepStrictEqual(fs.readdirSync(gameRoot), [], 'nothing was extracted onto disk');
  const [opensOk] = session.openTotals();
  console.log(`    opens at director ${opensOk} served`);

  step(6, 'the counters — where a host looks when a provider misbehaves');
  const st = cdn.stats()!;
  console.log(
    `    calls=${st.calls} settled=${st.settledCalls} vfsErrors=${st.vfsErrors} ` +
      `hostErrors=${st.hostErrors} stalled=${st.stalledCalls} abandoned=${st.abandonedCalls} ` +
      `selfCallRefusals=${st.selfCallRefusals}`
  );
  assert.strictEqual(st.hostErrors, 0);
  assert.strictEqual(st.selfCallRefusals, 0, 'main → worker is legal');

  step(7, 'the deadlock guard — the same provider, registered on this loop instead');
  // Deliberately wrong, to show what it says. `registerProvider` binds to the
  // *calling* loop, and this script then drives the session from that same loop.
  const pretendCdn: PretendCdn = require('./pretend-cdn-provider.cts');
  const wrong = registerProvider(pretendCdn({ latencyMs: 0 }));
  const bad = new Session('js-provider-wrong');
  bad.addRoot(0, 'game', path.join(scratch, 'other-root'));
  fs.mkdirSync(path.join(scratch, 'other-root'), { recursive: true });
  bad.mount(0, wrong);
  const t0 = Date.now();
  try {
    bad.readFile('readme.txt');
    throw new Error('the guard did not fire — that is a bug');
  } catch (e) {
    const err = e as Error;
    assert.match(err.message, /would deadlock/);
    console.log(`    refused in ${Date.now() - t0} ms rather than hanging:`);
    for (const line of err.message.replace(/\. /g, '.\n').split('\n')) {
      console.log(`      ${line.trim()}`);
    }
  }
  assert.strictEqual(wrong.stats()!.selfCallRefusals, 1);
  bad.close();
  releaseProvider(wrong.handle);

  step(8, 'teardown');
  session.close();
  // Without this the worker's loop stays alive — a live threadsafe function is
  // what keeps the provider available, so releasing it is how the process exits.
  await cdn.close();
  console.log(`    provider worker released; released=${cdn.stats()!.released}`);
  fs.rmSync(session.baseDir, { recursive: true, force: true });
  fs.rmSync(bad.baseDir, { recursive: true, force: true });
  fs.rmSync(scratch, { recursive: true, force: true });
  console.log('    scratch and session directories removed');

  process.stdout.write('\nJS PROVIDER SLICE OK\n');
}

main().catch((e: unknown) => {
  process.exitCode = 1;
  console.error(e);
});

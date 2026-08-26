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
// This file is ESM as of the ESM migration's task 2, so it uses real `import`
// statements rather than the `require` + type-annotation workaround `.cts`
// needed for node's type stripping. The package load below tries two different
// specifiers at runtime, which a static `import` cannot express, so it is a
// dynamic `import()` instead. `pretend-cdn-provider.mts` is ESM too, as of
// task 3 — it is loaded inside a provider worker via
// `await import(pathToFileURL(data.module).href)`, and here (for the
// deadlock-guard step) via an ordinary static `import`.

import assert from 'node:assert';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

import type { ProviderWorker } from '../index.mjs';
import pretendCdn from './pretend-cdn-provider.mts';

const pkgName = 'aethervfs';
let mod: typeof import('../index.mjs');
try {
  // `pkgName`, not the string literal: TypeScript resolves a dynamic `import()`'s
  // type from a literal specifier, and `aethervfs` is not on this machine's
  // module path (it is loaded in-tree, below) — a literal here would be a
  // standing TS2307 rather than the fallback this `try` exists to take.
  mod = await import(pkgName);
} catch (e) {
  if ((e as NodeJS.ErrnoException).code !== 'ERR_MODULE_NOT_FOUND') throw e;
  mod = await import(pathToFileURL(path.join(import.meta.dirname, '..', 'index.mjs')).href);
}
const { Session, disk, providerWorker, registerProvider, releaseProvider } = mod;

const probeExe = path.join(import.meta.dirname, '..', 'fixtures', 'vfs-probe.exe');
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
    module: path.join(import.meta.dirname, 'pretend-cdn-provider.mts'),
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
  // Staging puts the image in the managed root at its own vpath, so that what
  // the child resolves relative to its module path stays inside the VFS. The
  // promise is not that nothing is written here, but that nothing outlives the
  // session — asserted after `close()` in step 8.
  assert.deepStrictEqual(
    fs.readdirSync(gameRoot),
    ['probe.exe'],
    'the launched image must be staged into the managed root at its vpath'
  );
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
  // `pretendCdn` is the same factory imported at the top of this file — a real
  // `import`, not a second load through `require`.
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
  // Closing drops the staged image with the session — the surviving half of
  // "nothing was extracted onto disk".
  assert.deepStrictEqual(
    fs.readdirSync(gameRoot),
    [],
    'staging must not outlive the session'
  );
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
  console.error(e);
  // A live provider worker keeps this loop alive, so setting `exitCode` alone
  // leaves a failed run hanging rather than failing — which in CI is a job that
  // burns its timeout instead of reporting the assertion. Exit outright.
  process.exit(1);
});

#!/usr/bin/env node

// The task-6 vertical slice: `import('aethervfs')` through napi-rs and
// `vfs-embed` to a real injected process, with Rust primitives only.
//
// The shape mirrors `vfs-embed`'s own
// `an_image_only_the_provider_graph_holds_launches_from_an_empty_managed_root`,
// because that is the case the Node path has to hit rather than the refusal
// beside it: the managed root is **empty on real disk**, the executable exists
// only in the provider graph, and `launch` has to stage it out with its PE
// import closure before `CreateProcess` can see it. A relative image that the
// managed root *does* hold would launch without ever touching that code.
//
// `vfs-probe.exe` is the stand-in executable — ten lines: read argv[1], write
// those bytes to argv[2]. Its output file is what proves the child's read went
// through the ring to the graph rather than to a real file, because there is no
// real file: `hello.txt` has no on-disk existence under the managed root.
//
// This file is ESM as of the ESM migration's task 2, so node runs it with real
// `import` statements rather than the `require` + type-annotation workaround
// `.cts` needed. The package load two lines down tries two different specifiers
// at runtime, which a static `import` cannot express, so that one is a dynamic
// `import()` instead — no `require()` of the package survives anywhere in this
// file.

import assert from 'node:assert';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

import type { Provider } from '../index.mjs';

// `import('aethervfs')` if this file is somehow resolved from outside the
// package, and the in-tree path otherwise — which is the normal case, because
// Node resolves a script's real path before walking `node_modules`, so an
// example *inside* the package always resolves in-tree even when the package is
// linked. Both go through `index.mjs`, so both are the same load. That a
// consumer can import the package **by name** is a separate claim and needs a
// script outside the package to make it; there is one in the task-6 report.
let entry = 'aethervfs';
let mod: typeof import('../index.mjs');
try {
  // `entry`, not the string literal `'aethervfs'`: TypeScript resolves a dynamic
  // `import()`'s type from a literal specifier, and `aethervfs` is not on this
  // machine's module path (it is loaded in-tree, below) — a literal here would be
  // a standing TS2307 rather than the fallback this `try` exists to take.
  mod = await import(entry);
} catch (e) {
  if ((e as NodeJS.ErrnoException).code !== 'ERR_MODULE_NOT_FOUND') throw e;
  entry = '.. (in-tree, as expected for an in-package example)';
  mod = await import(pathToFileURL(path.join(import.meta.dirname, '..', 'index.mjs')).href);
}
const { Session, Provider: ProviderClass, disk, version, packageDir } = mod;

const probeExe = path.join(import.meta.dirname, '..', 'fixtures', 'vfs-probe.exe');
if (!fs.existsSync(probeExe)) {
  throw new Error(`${probeExe} is missing — run \`pnpm build\` first`);
}

function step(n: number, what: string): void {
  process.stdout.write(`\n[${n}] ${what}\n`);
}

// `Array<string | Buffer>` and not `string[]`: `readdirSync(dir, { recursive:
// true })` with no encoding resolves to `string[] | Buffer[]`, and every caller
// here already goes through `String(e)`. Narrowing it by passing an encoding
// would change the call rather than describe it.
function listing(dir: string): Array<string | Buffer> {
  try {
    return fs.readdirSync(dir, { recursive: true });
  } catch {
    return ['<missing>'];
  }
}

// ---------------------------------------------------------------------------

step(0, 'the addon is loaded');
console.log(`    aethervfs ${version()}`);
console.log(`    import(${JSON.stringify(entry)})`);
console.log(`    packageDir            ${packageDir()}`);

// A scratch tree for everything this script creates. Not the session's own
// directories — those are the session's business and it names them itself.
const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'aethervfs-slice-'));
const contentDir = path.join(scratch, 'content'); // what the graph serves
const docsDir = path.join(scratch, 'docs'); // a second root's content
const gameRoot = path.join(scratch, 'game-root'); // the managed root: stays EMPTY
const docsRoot = path.join(scratch, 'docs-root');
const outDir = path.join(scratch, 'out'); // outside every managed root
for (const d of [contentDir, docsDir, gameRoot, docsRoot, outDir]) {
  fs.mkdirSync(d, { recursive: true });
}

// The executable and the file it will read are both provider content.
fs.copyFileSync(probeExe, path.join(contentDir, 'probe.exe'));
fs.writeFileSync(path.join(contentDir, 'hello.txt'), 'served-from-the-graph');
fs.writeFileSync(path.join(docsDir, 'Skyrim.ini'), '[Display]\nsTest=1\n');

step(1, 'new Session(name)');
const session = new Session('vertical-slice');
console.log(`    name                  ${session.name}`);
console.log(`    baseDir               ${session.baseDir}`);
console.log(`    virtualRoot (default) ${session.virtualRoot}`);

step(2, 'session.addRoot(id, name, path)');
session.addRoot(0, 'game', gameRoot);
session.addRoot(1, 'docs', docsRoot);
console.log(`    virtualRoot (root 0)  ${session.virtualRoot}`);
console.table(session.roots());
assert.strictEqual(session.roots().length, 2);
assert.strictEqual(session.virtualRoot, gameRoot, 'addRoot(0, ...) must repoint the managed root');

step(3, 'disk(path) → session.mount(root, provider)');
const content: Provider = disk(contentDir);
const docs: Provider = disk(docsDir);
console.log(`    disk(${path.basename(contentDir)}) handle   ${content.handle}`);
console.log(`    disk(${path.basename(docsDir)}) handle      ${docs.handle}`);
session.mount(0, content);
session.mount(1, docs);

// The handle is the value and the object is only a wrapper: a worker that has
// the integer can rebuild a usable Provider without the object crossing.
const rebuilt = ProviderClass.fromHandle(content.handle);
assert.strictEqual(rebuilt.handle, content.handle);
session.mount(0, rebuilt); // idempotent: same provider, mounted again
console.log('    Provider.fromHandle round-trips, so a handle can cross an isolate');

// disk() refuses a path that is not there, instead of serving nothing quietly.
assert.throws(() => disk(path.join(scratch, 'no-such-dir')), /not an existing directory/);
console.log('    disk() refuses a missing directory');

step(4, 'session.readFile — the graph serves, host-side, before anything launches');
const hello = session.readFile('hello.txt');
console.log(`    readFile('hello.txt')  ${JSON.stringify(hello.toString())} (${hello.length} bytes)`);
assert.strictEqual(hello.toString(), 'served-from-the-graph');
assert.strictEqual(session.readFile('probe.exe').length, fs.statSync(probeExe).size);

step(5, 'session.serve() — the ring the injected child talks over');
session.serve();
assert.strictEqual(session.isServing(), true);
console.log(`    stateDir              ${session.stateDir}`);
console.log(`    overlayLayerDir(0)    ${session.overlayLayerDir(0)}`);

step(6, 'the DLLs an addon cannot discover for itself');
const info = session.shimInfo();
console.log(`    shimDll               ${info.shimDll}`);
console.log(`                          ${info.shimSize} bytes, ${new Date(info.shimModifiedMs).toISOString()}`);
console.log(`    payloadDll            ${info.payloadDll}`);
console.log(`                          ${info.payloadSize} bytes, ${new Date(info.payloadModifiedMs).toISOString()}`);
assert.strictEqual(path.dirname(info.shimDll), packageDir(), 'resolved from the package directory');

step(7, 'the managed root is empty on real disk — that is the whole case');
assert.deepStrictEqual(fs.readdirSync(gameRoot), [], 'the managed root must be empty before launch');
console.log(`    ${gameRoot} → []`);

step(8, "session.launch(exe) — an image only the provider graph holds");
const outFile = path.join(outDir, 'probe-out.bin');
const virtualHello = path.join(gameRoot, 'hello.txt'); // no such file on disk
assert.strictEqual(fs.existsSync(virtualHello), false, 'the path the child reads must not exist on disk');

const t0 = Date.now();
const code = session.launch('probe.exe', {
  args: [virtualHello, outFile],
  wait: true,
});
console.log(`    exit code             ${code}  (${Date.now() - t0} ms)`);
assert.strictEqual(code, 0, 'the child must exit cleanly');

step(9, 'what the child actually read');
const got = fs.readFileSync(outFile).toString();
console.log(`    ${outFile}`);
console.log(`      → ${JSON.stringify(got)}`);
assert.strictEqual(
  got,
  'served-from-the-graph',
  "the child's read of a path with no file behind it went through the ring to the graph"
);

assert.deepStrictEqual(
  fs.readdirSync(gameRoot),
  [],
  'nothing may be extracted into the managed root'
);
console.log(`    managed root still empty: ${gameRoot} → []`);

const staged = path.join(session.stateDir, 'stage');
console.log(`    staged into           ${staged}`);
console.log(`      ${JSON.stringify(listing(staged))}`);
assert.ok(listing(staged).some((e) => String(e).endsWith('probe.exe')), 'the image was staged');

// `openTotals()` is a `Vec<u64>` on the Rust side and therefore a `number[]` in
// the declaration, so the pair has to be named rather than destructured blind —
// `noUncheckedIndexedAccess` is right that an array index can be missing, and the
// tuple shape is the addon's contract rather than the type's.
const [opensOk, opensFailed] = session.openTotals() as [number, number];
console.log(`    opens at the director  ${opensOk} served, ${opensFailed} failed`);
assert.ok(opensOk > 0, 'the child must have reached the director');

step(10, 'the second root — addRoot(1, ...) + mount(1, ...) served end to end');
// Root 1 is a separate managed directory, empty on disk, served by its own
// provider. Without this the second `addRoot` would only be proving that the
// binding keeps a list.
assert.deepStrictEqual(fs.readdirSync(docsRoot), [], 'root 1 must be empty on disk too');
const out2 = path.join(outDir, 'ini-out.bin');
const virtualIni = path.join(docsRoot, 'Skyrim.ini');
const code2 = session.launch('probe.exe', { args: [virtualIni, out2], wait: true });
assert.strictEqual(code2, 0);
const iniGot = fs.readFileSync(out2).toString();
console.log(`    child read ${virtualIni}`);
console.log(`      → ${JSON.stringify(iniGot)}`);
assert.strictEqual(
  iniGot,
  '[Display]\nsTest=1\n',
  'root 1 must serve its own graph, from a directory that is empty on disk'
);

step(11, 'session.rejectedWrites()');
console.log(`    after two read-only launches: ${JSON.stringify(session.rejectedWrites())}`);

// A third launch, this time writing to a path *under* the managed root.
//
// This comes out **empty, and the reason is the finding**: `disk()` is the only
// primitive this task exposes and `DiskProvider` declares
// `Access::ReadWrite`, so every write under root 0 is served rather than
// refused. `rejectedWrites()` counts writes no read-write provider would take,
// which needs a read-only source in the graph — `readonly`, `zip`, `inline` —
// and those are task 8. So the assertion below is about *where the write went*,
// which is checkable, rather than about a counter that has nothing to count.
const underRoot = path.join(gameRoot, 'written-by-the-child.bin');
session.resetRejectedWrites();
const code3 = session.launch('probe.exe', { args: [virtualHello, underRoot], wait: true });
console.log(`    write-under-root launch exit code ${code3}`);
assert.strictEqual(code3, 0, "the child's write must succeed: DiskProvider is ReadWrite");
console.log(`    rejectedWrites()      ${JSON.stringify(session.rejectedWrites())}`);
console.log(`    managed root on disk  ${JSON.stringify(fs.readdirSync(gameRoot))}`);
console.log(`    overlay layer dir     ${JSON.stringify(listing(session.overlayLayerDir(0)))}`);
console.log(`    content dir on disk   ${JSON.stringify(fs.readdirSync(contentDir))}`);
// The write was routed into the mounted provider's real directory, not into the
// managed root and not into the shim's local overlay. That is what makes the
// empty `rejectedWrites()` an explanation instead of a shrug.
assert.deepStrictEqual(
  fs.readdirSync(gameRoot),
  [],
  'a write under the managed root must not land in the managed root'
);
assert.strictEqual(
  fs.readFileSync(path.join(contentDir, 'written-by-the-child.bin')).toString(),
  'served-from-the-graph',
  "the child's write was served by the mounted DiskProvider, which is why nothing was rejected"
);
console.log('    → the write was served by the mounted disk provider (hence 0 rejections)');
console.log(`    readFile sees it too: ${JSON.stringify(session.readFile('written-by-the-child.bin').toString())}`);

step(12, 'teardown');
session.close();
assert.throws(() => session.isServing(), /session is closed/);
assert.throws(() => session.readFile('hello.txt'), /session is closed/);
session.close(); // idempotent
console.log('    close() stopped the ring; further calls throw rather than no-op');
console.log(`    baseDir left for inspection: ${session.baseDir}`);

fs.rmSync(session.baseDir, { recursive: true, force: true });
fs.rmSync(scratch, { recursive: true, force: true });
console.log('    scratch and session directories removed by the script');

process.stdout.write('\nVERTICAL SLICE OK\n');

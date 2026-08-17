#!/usr/bin/env node

// # Spec §8's example, end to end, in TypeScript
//
// This is stage 4's acceptance evidence. Spec §8 writes the example in Python
// and states what it is for:
//
// > ```python
// > session = vfs.Session("skyrim")
// > session.add_root(0, "game", r"C:\Games\Skyrim")
// > session.add_root(1, "docs", r"C:\Users\me\Documents\My Games\Skyrim")
// >
// > base = vfs.cached(vfs.seekable(SteamCdn(depot="489830")),
// >                   ram="512MiB", disk="C:/cache")
// > inis = vfs.memory({"Skyrim.ini": ini_bytes})
// >
// > session.mount(0, vfs.layered(vfs.readonly(base), vfs.disk(r"C:\mods\SkyUI")))
// > session.mount(1, vfs.router({"*.ini": inis},
// >                            default=vfs.overlay(vfs.disk(docs),
// >                                                upper=vfs.disk(scratch))))
// >
// > with session.launch("SkyrimSE.exe") as proc:
// >     proc.wait()
// >
// > print(session.rejected_writes())
// > print(inis.read("Skyrim.ini"))     # what the game actually wrote
// > ```
// >
// > Everything except `SteamCdn` is a Rust primitive. **That is the test of
// > whether §6 succeeded.**
//
// So the load-bearing property of this file is not that it runs — it is that
// only the *leaf* is JavaScript. Step 3 makes that machine-checked rather than
// claimed: it walks both roots' graphs through `Provider.kind`/`.children` and
// asserts there is exactly one `js` node in the whole composition.
//
// **What this does not do: launch Skyrim.** That is stage 5. `SkyrimSE.exe`
// here is `vfs-probe.exe` renamed inside the depot — a real Windows executable,
// really injected, really served through the ring, that reads argv[1] and writes
// argv[2]. Everything about the chain is real except the size and complexity of
// the process at the end of it.
//
// ## The one place a host must compensate for a missing primitive
//
// Spec §6's catalog lists `casefold` and Rust does not implement it (§6b). The
// shim folds every vpath component before it crosses the ring, host-side reads
// do not fold, and `memory()` is case-sensitive by design. So §8's own last line
// — `inis.read("Skyrim.ini")` — reads back **the host's seed, not the game's
// write**, with no error anywhere. This example does not paper over that:
//
//  * step 7 does the round trip with **folded keys**, which is what a host must
//    do today, and reads the game's bytes back;
//  * step 8 does it exactly as §8 spells it and **demonstrates the silent wrong
//    answer**, printing both entries side by side.
//
// ## TypeScript, and what that is worth here
//
// Real TypeScript — annotations, `import type` — run directly by `node`, which
// strips types natively (Node 22.6+, unflagged since 23.6; this is v24). `.cts`
// because the package is CommonJS and type stripping does not rewrite module
// syntax. **The types are erased, not checked**: there is no `tsc` in this
// package's toolchain, so the annotations document the surface a host codes
// against and would catch a mistake under a checker the host runs. They are not
// themselves a gate.

import type { Provider, ProviderWorker, RejectedWrite, RootInfo } from '../index.cjs';

const assert = require('node:assert') as typeof import('node:assert');
const { spawn, spawnSync } = require('node:child_process') as typeof import('node:child_process');
const fs = require('node:fs') as typeof import('node:fs');
const os = require('node:os') as typeof import('node:os');
const path = require('node:path') as typeof import('node:path');

// `require('aethervfs')` when the package is installed, the in-tree entry
// otherwise. Both go through `index.cjs`, so both are the same load.
let mod: typeof import('../index.cjs');
try {
  mod = require('aethervfs');
} catch (e) {
  if ((e as NodeJS.ErrnoException).code !== 'MODULE_NOT_FOUND') throw e;
  mod = require('..');
}
const {
  Session,
  Provider: ProviderClass,
  disk,
  memory,
  readonly,
  seekable,
  cached,
  layered,
  overlay,
  router,
  providerWorker,
  version,
} = mod as any;

const PROBE = path.join(__dirname, '..', 'fixtures', 'vfs-probe.exe');
if (!fs.existsSync(PROBE)) {
  throw new Error(`${PROBE} is missing — run \`npm run build\` first`);
}

// Held at module scope so the failure path can release them too. That is not
// tidiness: while writing this file an assertion in step 6 failed, the `catch`
// at the bottom set an exit code, and **the process then hung forever** — the
// worker's threadsafe function was still live, so its loop never ended and node
// had nothing to exit with. An example that hangs when it fails hides the
// failure it just found, so the release below is part of the demonstration
// rather than housekeeping.
let openWorker: ProviderWorker | undefined;
let openSession: { close(): void } | undefined;

const step = (n: string, what: string): void => process.stdout.write(`\n[${n}] ${what}\n`);
const show = (...parts: unknown[]): void => console.log('   ', ...parts);

/** Print a provider graph as a tree, and return every `kind` in it. */
function describeGraph(handle: number, indent = '    '): string[] {
  const p: Provider = ProviderClass.fromHandle(handle);
  const note = p.kind === 'js' ? '   <-- the only JavaScript in this file' : '';
  console.log(`${indent}${p.kind} #${handle}${note}`);
  const kinds = [p.kind as string];
  for (const child of p.children) kinds.push(...describeGraph(child, `${indent}  `));
  return kinds;
}

async function main(): Promise<void> {
  step('0', 'the addon');
  show(`aethervfs ${version()} on node ${process.version}`);

  // A scratch tree. The two managed roots stay EMPTY on real disk: everything
  // the child sees under them comes from the provider graph.
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'aethervfs-spec8-'));
  const dirs = {
    gameRoot: path.join(scratch, 'Games', 'Skyrim'), //          root 0, empty
    docsRoot: path.join(scratch, 'Documents', 'My Games', 'Skyrim'), // root 1, empty
    skyUI: path.join(scratch, 'mods', 'SkyUI'), //               vfs.disk(r"C:\mods\SkyUI")
    docs: path.join(scratch, 'docs-source'), //                  vfs.disk(docs)
    upper: path.join(scratch, 'scratch'), //                     upper=vfs.disk(scratch)
    blocks: path.join(scratch, 'cache'), //                      disk="C:/cache"
    out: path.join(scratch, 'out'), //                           outside every root
  };
  for (const d of Object.values(dirs)) fs.mkdirSync(d, { recursive: true });

  // The mod directory overrides one depot file and adds one of its own.
  fs.mkdirSync(path.join(dirs.skyUI, 'Data'), { recursive: true });
  fs.writeFileSync(
    path.join(dirs.skyUI, 'Data', 'Skyrim.ini'),
    '[General]\nuGridsToLoad=7\n; from SkyUI\n'
  );
  fs.writeFileSync(path.join(dirs.skyUI, 'Data', 'skyui.swf'), 'mod-only');
  // Root 1's real Documents content: everything that is not an INI.
  fs.mkdirSync(path.join(dirs.docs, 'Saves'), { recursive: true });
  fs.writeFileSync(path.join(dirs.docs, 'plugins.txt'), 'Skyrim.esm\nSkyUI.esp\n');
  fs.writeFileSync(path.join(dirs.docs, 'Saves', 'quicksave.ess'), 'a save game');

  // -------------------------------------------------------------------------
  step('1', 'SteamCdn — the one provider a host writes, on its own worker loop');
  // -------------------------------------------------------------------------
  // A dedicated worker is the shape §8c measured as the only one immune to a
  // busy main loop (1449 MiB/s against 3.8 for a main-loop provider under ~1 ms
  // of work per turn). The session below is driven from the main thread, which
  // is safe *because* this provider is not serviced by that loop: the rule is
  // loop identity, not thread role.
  const cdn: ProviderWorker = await providerWorker({
    module: require.resolve('./steam-cdn-provider.cjs'),
    options: { depot: '489830', latencyMs: 1, exeSource: PROBE },
  });
  openWorker = cdn;
  const st = cdn.stats()!;
  show(`handle          ${cdn.handle}  (a process-global integer, not a JS object)`);
  show(`serviced by     ${st.ownerThread}`);
  show(
    `declares        access=${st.access} immutable=${st.immutable} slow=${st.slow} ` +
      `preferredBlock=${st.preferredBlock}`
  );
  show(`methods found   ${st.methods.join(', ')}`);
  assert.strictEqual(st.access, 'seqread', "spec §8's SteamCdn is forward-only");
  assert.ok(!st.methods.includes('readAt'), 'and has no readAt at all');

  // -------------------------------------------------------------------------
  step('2', "the graph — spec §8's five lines, translated");
  // -------------------------------------------------------------------------
  const session = new Session('skyrim');
  openSession = session;
  session.addRoot(0, 'game', dirs.gameRoot);
  session.addRoot(1, 'docs', dirs.docsRoot);
  console.table(session.roots() as RootInfo[]);
  assert.strictEqual(session.virtualRoot, dirs.gameRoot, 'addRoot(0) repoints the managed root');

  // base = cached(seekable(SteamCdn(...)), ram=..., disk=...)
  const base: Provider = cached(seekable(cdn.provider), {
    ramBytes: 512 * 1024 * 1024,
    diskDir: dirs.blocks,
  });

  // inis = memory({"Skyrim.ini": ini_bytes})
  //
  // **Folded key, deliberately.** §8 writes `"Skyrim.ini"`; the child's write
  // arrives as `skyrim.ini` and would land beside it. Step 8 is that failure,
  // demonstrated on a second file rather than argued about.
  const inis: Provider = memory({ 'skyrim.ini': '[General]\nuGridsToLoad=5\n; the seed\n' });

  // session.mount(0, layered(readonly(base), disk(r"C:\mods\SkyUI")))
  const graph0: Provider = layered(readonly(base), disk(dirs.skyUI));
  session.mount(0, graph0);

  // session.mount(1, router({"*.ini": inis}, default=overlay(disk(docs), upper=disk(scratch))))
  const graph1: Provider = router({ '*.ini': inis }, overlay(disk(dirs.docs), disk(dirs.upper)));
  session.mount(1, graph1);

  // A `seqread` provider mounted bare is a hard error, not a runtime surprise —
  // §6's flag table, first row. Shown here because it is the mistake this graph
  // exists to avoid, and the message names the fix.
  assert.throws(
    () => session.mount(0, cdn.provider),
    (e: Error) => {
      show(`mount(bare seqread) refused: ${e.message.split('.')[0]}.`);
      return /seekable\(provider\)/.test(e.message);
    }
  );

  // -------------------------------------------------------------------------
  step('3', '§6\'s claim, machine-checked: is the leaf the only JavaScript?');
  // -------------------------------------------------------------------------
  console.log('    root 0:');
  const kinds0 = describeGraph(graph0.handle, '      ');
  console.log('    root 1:');
  const kinds1 = describeGraph(graph1.handle, '      ');
  const all = [...kinds0, ...kinds1];
  show(`nodes           ${all.length}   (${all.join(', ')})`);
  assert.deepStrictEqual(
    kinds0,
    ['layered', 'readonly', 'cached', 'seekable', 'js', 'disk'],
    "root 0 is spec §8's stack exactly"
  );
  assert.deepStrictEqual(
    kinds1,
    ['router', 'overlay', 'disk', 'disk', 'memory'],
    "root 1 is spec §8's stack exactly (router lists its default first)"
  );
  assert.strictEqual(
    all.filter((k) => k === 'js').length,
    1,
    'EXACTLY ONE node in this example is JavaScript — that is what §6 claims'
  );
  assert.deepStrictEqual(graph0.jsLeaves(), [cdn.handle], 'and the addon agrees which one');
  assert.deepStrictEqual(graph1.jsLeaves(), [], 'root 1 is Rust all the way down');
  show(`js leaves       root 0 ${JSON.stringify(graph0.jsLeaves())}, root 1 ${JSON.stringify(graph1.jsLeaves())}`);

  // And the coverage claim, checked rather than asserted in prose: **every
  // primitive this addon exposes appears in this one composition.** §6's catalog
  // has nine entries; the ninth is `casefold`, which does not exist in Rust, and
  // step 8 is what its absence costs.
  const exposed = ['cached', 'disk', 'layered', 'memory', 'overlay', 'readonly', 'router', 'seekable'];
  assert.deepStrictEqual(
    [...new Set(all)].filter((k) => k !== 'js').sort(),
    exposed,
    'every primitive the addon exposes is in this graph'
  );
  show(`primitives      ${exposed.length}/${exposed.length} exposed, all used; casefold is the 9th and is not implemented`);

  // §6's capability recomputation, on the graph that was just built: `seekable`
  // promoted the leaf so it can be mounted, and `cached` answered `slow` so
  // mount() has nothing to warn about.
  const cb = base.capabilities();
  show(`base caps       access=${cb.access} immutable=${cb.immutable} slow=${cb.slow}`);
  assert.strictEqual(cb.access, 'read', 'seekable promoted seqread → read');
  assert.strictEqual(cb.slow, false, 'cached answered slow');

  // -------------------------------------------------------------------------
  step('4', 'the graph serves, host-side, before anything is launched');
  // -------------------------------------------------------------------------
  // Root 0, through readonly(cached(seekable(js))) and the mod layer above it.
  const modIni = session.readFile('Data/Skyrim.ini').toString();
  show(`root 0 Data/Skyrim.ini   ${JSON.stringify(modIni)}`);
  assert.match(modIni, /from SkyUI/, 'layered: the later argument wins on a shared path');
  assert.strictEqual(session.readFile('Data/skyui.swf').toString(), 'mod-only');

  // 4 KiB of `i % 251` out of the depot, compared whole: a mis-counted skip in
  // `seekable`'s cursor returns plausible bytes at the wrong offsets, which a
  // length check would not see.
  const bsa = session.readFile('Data/textures.bsa');
  assert.deepStrictEqual(
    bsa,
    Buffer.from(Array.from({ length: 4096 }, (_, i) => i % 251)),
    'every byte at its right offset, through a provider with no readAt'
  );
  show(`root 0 Data/textures.bsa 4096 bytes, every one at its right offset`);

  // Root 1, and this is the call spec §8's last line needs: `readFile` used to
  // be root 0 only, so a host could list a second root's graph and never read a
  // byte out of it. See the report.
  const plugins = session.readFile('plugins.txt', 1).toString();
  show(`root 1 plugins.txt       ${JSON.stringify(plugins)}   (router → default → overlay → disk)`);
  assert.match(plugins, /SkyUI\.esp/);
  assert.match(session.readFile('skyrim.ini', 1).toString(), /the seed/, 'router → memory');

  // The cache, measured rather than assumed: re-read and require hits with no
  // new bytes from the depot. Before `cacheStats()` a host could not tell
  // `cached(p)` from `p`.
  const c0 = base.cacheStats()!;
  for (let i = 0; i < 5; i += 1) session.readFile('Data/textures.bsa');
  const c1 = base.cacheStats()!;
  show(
    `cache           hits ${c0.hits}→${c1.hits}  misses ${c0.misses}→${c1.misses}  ` +
      `block ${c1.blockSize}  fromSource ${c1.bytesFromSource}`
  );
  assert.strictEqual(c1.misses, c0.misses, 'five re-reads, no new misses');
  assert.strictEqual(c1.bytesFromSource, c0.bytesFromSource, 'the depot was not read again');
  assert.strictEqual(c1.blockSize, 65536, "the leaf's preferredBlock chose the block size, not the 1 MiB default");

  // -------------------------------------------------------------------------
  step('5', 'both managed roots are empty on real disk — that is the whole case');
  // -------------------------------------------------------------------------
  assert.deepStrictEqual(fs.readdirSync(dirs.gameRoot), [], 'root 0 empty');
  assert.deepStrictEqual(fs.readdirSync(dirs.docsRoot), [], 'root 1 empty');
  show(`${dirs.gameRoot} → []`);
  show(`${dirs.docsRoot} → []`);
  const shim = session.shimInfo();
  show(`shim            ${shim.shimDll} (${shim.shimSize} bytes, ${new Date(shim.shimModifiedMs).toISOString()})`);

  // -------------------------------------------------------------------------
  step('6', 'session.launch("SkyrimSE.exe") — an image only a JS function holds');
  // -------------------------------------------------------------------------
  // `SkyrimSE.exe` exists nowhere on disk. `launch` resolves it through root 0's
  // graph — which bottoms out in a `readNext` in another isolate — stages it out
  // with its PE import closure, and runs that copy.
  assert.strictEqual(fs.existsSync(path.join(dirs.gameRoot, 'SkyrimSE.exe')), false);
  const readPath = path.join(dirs.gameRoot, 'Data', 'Skyrim.ini'); // root 0, no file behind it
  const writePath = path.join(dirs.docsRoot, 'Skyrim.ini'); //        root 1, no file behind it
  const t0 = Date.now();
  const code: number = session.launch('SkyrimSE.exe', { args: [readPath, writePath], wait: true });
  show(`exit code       ${code}  (${Date.now() - t0} ms)`);
  assert.strictEqual(code, 0, 'the child must exit cleanly');

  // How much of the image came out of JavaScript: all of it, to the byte. The
  // cache counters are the instrument, because "it launched" is also what a
  // stale copy of the exe sitting somewhere would look like.
  const cStaged = base.cacheStats()!;
  const exeBytes = fs.statSync(PROBE).size;
  show(
    `pulled from the depot to stage it: ${cStaged.bytesFromSource - c1.bytesFromSource} bytes ` +
      `(SkyrimSE.exe is ${exeBytes})`
  );
  assert.strictEqual(
    cStaged.bytesFromSource - c1.bytesFromSource,
    exeBytes,
    'the whole image came through readNext in another isolate'
  );
  const [opensOk, opensFailed] = session.openTotals();
  show(`opens at the director  ${opensOk} served, ${opensFailed} failed`);
  assert.ok(opensOk > 0, 'the child must have reached the director');
  const staged = fs.readdirSync(path.join(session.stateDir, 'stage'), { recursive: true });
  show(`staged image    ${JSON.stringify(staged.map(String))}`);
  assert.ok(
    staged.some((f) => /SkyrimSE\.exe$/i.test(String(f))),
    'the image was staged out of the graph'
  );
  assert.deepStrictEqual(fs.readdirSync(dirs.gameRoot), [], 'nothing extracted into the managed root');

  // -------------------------------------------------------------------------
  step('7', "spec §8's last two lines — what the game actually wrote");
  // -------------------------------------------------------------------------
  const rejected: RejectedWrite[] = session.rejectedWrites();
  show(`rejectedWrites()  ${JSON.stringify(rejected)}`);
  assert.deepStrictEqual(rejected, [], 'memory() is ReadWrite, so nothing was refused');

  // `print(inis.read("Skyrim.ini"))`, with the key folded.
  const wrote = session.readFile('skyrim.ini', 1).toString();
  show(`inis["skyrim.ini"]  ${JSON.stringify(wrote)}`);
  assert.match(wrote, /from SkyUI/, 'the game wrote the mod INI it read, and the host reads it back');
  assert.doesNotMatch(wrote, /the seed/, "this must NOT be the host's own seed");
  // Nothing touched disk on either side of the round trip.
  assert.deepStrictEqual(fs.readdirSync(dirs.docsRoot), [], 'root 1 still empty on disk');
  assert.deepStrictEqual(fs.readdirSync(dirs.upper), [], "the overlay's upper took nothing: the router sent it to memory()");
  show('the INI never existed under either root on disk — mod layer over the depot → child → memory() → host');

  // -------------------------------------------------------------------------
  step('8', 'the same round trip spelled as §8 spells it — and why it lies');
  // -------------------------------------------------------------------------
  // Seeded under the capitalised name spec §8 uses, on its own prefix so step 7
  // stays untouched. Nothing below fails, throws or is refused: that is the
  // finding.
  const capitalised: Provider = memory({ 'SkyrimPrefs.ini': '[Display]\niSize H=1080\n; the seed\n' });
  session.mount(1, capitalised, 'asspecwritesit');
  const cPre = base.cacheStats()!;
  const code2: number = session.launch('SkyrimSE.exe', {
    args: [
      path.join(dirs.gameRoot, 'Data', 'SkyrimPrefs.ini'), // depot-only, so the JS leaf serves it
      path.join(dirs.docsRoot, 'asspecwritesit', 'SkyrimPrefs.ini'),
    ],
    wait: true,
  });
  show(`exit code       ${code2}   (the child believes it wrote)`);
  assert.strictEqual(code2, 0);
  assert.deepStrictEqual(session.rejectedWrites(), [], 'and nothing refused it');

  const asSpecWritesIt = session.readFile('asspecwritesit/SkyrimPrefs.ini', 1).toString();
  const asItLanded = session.readFile('asspecwritesit/skyrimprefs.ini', 1).toString();
  show(`inis.read("SkyrimPrefs.ini")  ${JSON.stringify(asSpecWritesIt)}`);
  show(`inis.read("skyrimprefs.ini")  ${JSON.stringify(asItLanded)}`);
  assert.match(asSpecWritesIt, /the seed/, "§8's own spelling returns the HOST'S SEED");
  assert.match(asItLanded, /from the depot/, 'the write landed beside it, under the folded name');
  show('two entries, one file, no error anywhere — spec §6b, and why `casefold` matters');

  // The child's read came out of the depot, which also proves the JS provider
  // served an **injected process** and not only a host-side call: this path is in
  // no other layer, and it had never been read host-side, so it could not be a
  // cache hit. Measured across this launch alone rather than since step 4 —
  // otherwise the number would be dominated by the image the first launch staged.
  const c2 = base.cacheStats()!;
  const depotBytes = c2.bytesFromSource - cPre.bytesFromSource;
  show(
    `this launch pulled from the depot: ${depotBytes} bytes, ` +
      `misses ${cPre.misses}→${c2.misses}  (nothing else re-read: the image was cached)`
  );
  assert.strictEqual(
    depotBytes,
    asItLanded.length,
    "exactly the INI the child read — the JavaScript leaf served an injected process"
  );

  // §6 specifies `router`'s readdir as a union across the default plus every
  // route; `RouterProvider` returns only the answering child's listing. So an
  // INI served by a route is readable by name and invisible to enumeration —
  // stated here because a game that enumerates its Documents folder to find its
  // INIs will not see one.
  const listed = (session.readdir('', 1) as Array<{ name: string }>).map((e) => e.name).sort();
  show(`readdir('', root 1)  ${JSON.stringify(listed)}   (no *.ini — §6 gap, task 8's finding)`);

  // -------------------------------------------------------------------------
  step('9', 'releaseProvider is mandatory, and here is what it costs to forget');
  // -------------------------------------------------------------------------
  // A live threadsafe function keeps its loop alive, so a worker never exits
  // until its provider is released. Demonstrated with two child processes doing
  // nothing but registering a provider — one that releases it, one that does not.
  const leakProbe = path.join(scratch, 'leak-probe.cjs');
  fs.writeFileSync(
    leakProbe,
    `const p = require(${JSON.stringify(path.join(__dirname, '..', 'index.cjs'))});\n` +
      `p.providerWorker({ module: ${JSON.stringify(require.resolve('./steam-cdn-provider.cjs'))},\n` +
      `                   options: { exeSource: ${JSON.stringify(PROBE)} } })\n` +
      `  .then(async (w) => { if (process.argv[2] === 'release') await w.close(); });\n`
  );
  const PATIENCE_MS = 3000;
  for (const mode of ['release', 'leak']) {
    const r = await runLeakProbe(leakProbe, mode, PATIENCE_MS);
    show(
      `${mode.padEnd(8)} exit=${r.status} after ${r.ms} ms` +
        (r.killed ? '   <-- still running; killed. It was never going to exit.' : '')
    );
    if (mode === 'release') {
      assert.strictEqual(r.status, 0, 'a released provider lets node exit');
      assert.ok(!r.killed, `and it exits promptly (${r.ms} ms)`);
    } else {
      assert.ok(r.killed, `a leaked provider hangs on exit (waited ${PATIENCE_MS} ms)`);
    }
  }

  // Which is why this example ends here and not one line earlier.
  show(`jsLeaves to release: ${JSON.stringify(graph0.jsLeaves())}`);
  const sessionDir = session.baseDir;
  await release();
  fs.rmSync(sessionDir, { recursive: true, force: true });
  fs.rmSync(scratch, { recursive: true, force: true });

  console.log('\nOK — spec §8, end to end, with one JavaScript provider and eight Rust primitives.');
  console.log('   (What stage 5 still owes: the process at the end of it is 165 KB, not Skyrim.)');
}

/**
 * Run the leak probe and report whether it exited on its own.
 *
 * `spawnSync(..., { timeout })` was the obvious way to write this and it is the
 * wrong one: its timeout sends `SIGTERM`, and a leaked child that survives it
 * keeps `aethervfs.node` mapped — after which the next `scripts/build.cjs`
 * fails to copy the addon with `EBUSY`, which is precisely the family of trap
 * this project has been bitten by before (a stale native artifact that reports
 * success). So the kill is `taskkill /F /T`, by pid, and this waits for the
 * exit rather than assuming it.
 */
function runLeakProbe(
  script: string,
  mode: string,
  patienceMs: number
): Promise<{ status: number | null; ms: number; killed: boolean }> {
  return new Promise((resolve) => {
    const started = Date.now();
    let killed = false;
    const child = spawn(process.execPath, [script, mode], { stdio: 'ignore' });
    const timer = setTimeout(() => {
      killed = true;
      spawnSync('taskkill', ['/F', '/T', '/PID', String(child.pid)], { stdio: 'ignore' });
    }, patienceMs);
    child.on('exit', (status) => {
      clearTimeout(timer);
      resolve({ status, ms: Date.now() - started, killed });
    });
  });
}

/** Idempotent, and called on both paths — see the comment at `openWorker`. */
async function release(): Promise<void> {
  const w = openWorker;
  const s = openSession;
  openWorker = undefined;
  openSession = undefined;
  if (w) await w.close();
  try {
    s?.close();
  } catch {
    /* already closed */
  }
}

main().catch(async (e) => {
  console.error(e);
  process.exitCode = 1;
  await release();
});

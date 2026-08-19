// Task 8: spec §6's primitive catalog, composed from TypeScript.
//
// The claim under test is §6's own, quoted in the brief: *"Everything except
// [the host provider] is a Rust primitive. That is the test of whether §6
// succeeded."* So there is exactly one JavaScript provider in this file — a
// forward-only, slow, immutable "CDN" (`test/cdn-provider.cts`) — and every
// other node in every graph below is a Rust type reached through the addon.
//
// Written as spec §8's composition, translated from its Python:
//
//   base = cached(seekable(SteamCdn(depot="489830")), ram=..., disk=...)
//   inis = memory({"Skyrim.ini": ini_bytes})
//   session.mount(0, layered(readonly(base), disk("C:/mods/SkyUI")))
//   session.mount(1, router({"*.ini": inis},
//                           default=overlay(disk(docs), upper=disk(scratch))))
//
// ## Why this file is TypeScript, and what that now buys
//
// It is real TypeScript — interfaces, annotations, `import` — run by vitest,
// which transforms it through vite's esbuild. `.mts` because the package is ESM
// as of the ESM migration's task 2; the fixture module beside this file
// (`test/cdn-provider.cts`) stays CommonJS because it is loaded by **node**
// inside a provider worker, whose type stripping does not rewrite module syntax.
//
// **The types are checked, as of task 3.** They were not before: `tsconfig.json`'s
// `include` stopped short of `test/**` because
// `require('node:assert') as typeof import('node:assert')` produced 134 × TS2775
// — an assertion function needs an explicit type *annotation* on the name it is
// called on, and a cast is not one. A real `import` erases every one of them, the
// `include` now covers this directory, and `tsc --noEmit` is a step in
// `pnpm test`. So the annotations here are a gate and not only documentation —
// which is worth saying plainly, because for two tasks they were the latter and
// the header said so.

import { test } from 'vitest';
import assert from 'node:assert';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

import { teardown, type TestTeardown } from './teardown.mts';
import type { Provider, ProviderCapabilities, ProviderWorker, RejectedWrite } from '../index.mjs';
import * as aether from '../index.mjs';

const {
  Session,
  disk,
  memory,
  readonly,
  seekable,
  cached,
  layered,
  overlay,
  router,
  providerWorker,
  releaseProvider,
  KIND,
} = aether;

const CDN_MODULE: string = path.join(import.meta.dirname, 'cdn-provider.cts');
const PROBE: string = path.join(import.meta.dirname, '..', 'fixtures', 'vfs-probe.exe');

// ---------------------------------------------------------------------------
// Scaffolding, same conventions as task 7's suite: a scratch tree and a session
// per test, both removed afterwards.
// ---------------------------------------------------------------------------

function scratch(t: TestTeardown, name: string): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), `aethervfs-t8-${name}-`));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

function dirWith(root: string, name: string, files: Record<string, string>): string {
  const dir = path.join(root, name);
  for (const [rel, body] of Object.entries(files)) {
    const p = path.join(dir, rel);
    fs.mkdirSync(path.dirname(p), { recursive: true });
    fs.writeFileSync(p, body);
  }
  fs.mkdirSync(dir, { recursive: true });
  return dir;
}

function newSession(t: TestTeardown, name: string) {
  const s = new Session(`t8-${name}`);
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

/** The one JS-authored provider in this file, on its own worker loop. */
async function cdn(t: TestTeardown, options: Record<string, unknown> = {}): Promise<ProviderWorker> {
  const pw: ProviderWorker = await providerWorker({ module: CDN_MODULE, options });
  t.after(() => pw.close());
  return pw;
}

function caps(p: Provider): ProviderCapabilities {
  return p.capabilities();
}

function names(entries: Array<{ name: string }>): string[] {
  return entries.map((e) => e.name).sort();
}

// ---------------------------------------------------------------------------
// 1. Spec §8's composition, host-side, through every primitive at once.
// ---------------------------------------------------------------------------

test("1. spec §8's graph composes from Rust primitives over one JS leaf", async () => {
  const t = teardown();
  const scr = scratch(t, 'compose');
  const mods = dirWith(scr, 'SkyUI', {
    'shared.txt': 'from-the-mod',
    'interface/skyui.swf': 'mod-only',
  });

  const pw = await cdn(t);

  // The leaf, as it declares itself. Forward-only, expensive, unchanging — the
  // three facts that decide what has to be wrapped around it.
  assert.deepStrictEqual(
    { ...caps(pw.provider) },
    { access: 'seqread', immutable: true, slow: true, preferredBlock: 65536 },
    'the JS provider declares exactly what spec §8 has SteamCdn declare'
  );

  const s = newSession(t, 'compose');
  s.addRoot(0, 'game', dirWith(scr, 'game-root', {}));

  // §6's flag table, first row: a SeqRead provider is a hard error at mount,
  // not a runtime surprise. Refused *before* the graph is composed, so there is
  // no half-built session left behind.
  assert.throws(
    () => s.mount(0, pw.provider),
    (e: Error) => {
      assert.match(e.message, /seqread/);
      assert.match(e.message, /seekable\(provider\)/);
      console.log(`    mount(seqread) refused: ${e.message.split('.')[0]}.`);
      return true;
    }
  );

  // cached(seekable(cdn)) — the spec's `base`, built out of two Rust
  // combinators, neither of which existed as a Rust type before this task.
  const base: Provider = cached(seekable(pw.provider), {
    ramBytes: 8 * 1024 * 1024,
    diskDir: path.join(scr, 'blocks'),
  });
  assert.strictEqual(seekable(pw.provider).kind, 'seekable');
  assert.strictEqual(base.kind, 'cached');

  // §6's capability recomputation, two rules at once: `seekable` promoted
  // SeqRead to Read, and `cached` answered `slow`.
  assert.strictEqual(caps(base).access, 'read', 'seekable promoted the leaf');
  assert.strictEqual(caps(base).slow, false, 'cached answered slow');
  assert.strictEqual(caps(base).immutable, true, 'immutability survives both wrappers');

  const graph: Provider = layered(readonly(base), disk(mods));
  assert.strictEqual(caps(readonly(base)).access, 'read');
  // Strongest child, not weakest: the mod directory is writable, so the stack
  // can serve a write — to the mod directory, which is the only child that can
  // take one.
  assert.strictEqual(caps(graph).access, 'readwrite');
  assert.strictEqual(caps(graph).immutable, false, 'a disk child is not immutable');

  s.mount(0, graph);

  // Reads. `vanilla/data.bin` exists only inside a JavaScript closure in another
  // isolate, and reaches here through seekable → cached → readonly → layered.
  const readme = s.readFile('vanilla/readme.txt').toString();
  assert.strictEqual(readme, 'depot 489830: fetched over a pretend network');

  // 4096 bytes of `i % 251`, read whole: this is the assertion that `seekable`'s
  // cursor arithmetic is right, because a mis-counted skip would return
  // plausible-looking bytes at the wrong offsets rather than fail.
  const data = s.readFile('vanilla/data.bin');
  assert.strictEqual(data.length, 4096);
  const expected = Buffer.from(Array.from({ length: 4096 }, (_, i) => i % 251));
  assert.deepStrictEqual(data, expected, 'every byte at its right offset');

  // `layered`: the mod wins on a path both serve, and the base is still there
  // for a path only it has.
  assert.strictEqual(s.readFile('shared.txt').toString(), 'from-the-mod');
  assert.strictEqual(s.readFile('interface/skyui.swf').toString(), 'mod-only');

  // `layered`'s readdir unions, top-wins per name — the other half of §6's rule
  // and not visible from `readFile`.
  const listing = names(s.readdir(''));
  assert.ok(listing.includes('shared.txt'), `union listing: ${listing.join(', ')}`);
  assert.ok(listing.includes('vanilla'), 'the base-only directory is in the union');
  assert.ok(listing.includes('interface'), 'the mod-only directory is in the union');
  const shared = s.readdir('').find((e: { name: string }) => e.name === 'shared.txt')!;
  assert.strictEqual(shared.size, 'from-the-mod'.length, 'top layer supplies the stat too');

  // `cached` is doing something, which is otherwise an act of faith: without a
  // way to read the counters, "I added a cache" and "I added nothing" look
  // identical from JavaScript.
  const beforeCalls = pw.stats()!.calls;
  const before = base.cacheStats()!;
  for (let i = 0; i < 5; i += 1) {
    assert.strictEqual(s.readFile('vanilla/data.bin').length, 4096);
  }
  const st = base.cacheStats()!;
  assert.strictEqual(st.hits, before.hits + 5, 'five re-reads, five cache hits');
  assert.strictEqual(st.misses, before.misses, 'and not one new miss');
  assert.strictEqual(
    st.bytesFromSource,
    before.bytesFromSource,
    'the CDN was not read again — this is the assertion that the cache is real'
  );
  assert.strictEqual(st.blockSize, 65536, "the leaf's preferredBlock chose the block size");
  console.log(
    `    cache: hits=${st.hits} misses=${st.misses} block=${st.blockSize} ` +
      `fromCache=${st.bytesFromCache} fromSource=${st.bytesFromSource}`
  );

  // What the cache does *not* eliminate, said out loud because the number
  // surprises: `readFile` is getattr + open + close + read, and `cached` only
  // absorbs the read. Five re-reads still cost fifteen bridge crossings, and a
  // host looking to cut those needs a different mechanism (an open cache), not a
  // bigger block cache.
  const perRead = (pw.stats()!.calls - beforeCalls) / 5;
  console.log(`    bridge crossings: ${beforeCalls} → ${pw.stats()!.calls} = ${perRead} per re-read`);
  assert.strictEqual(perRead, 3, 'getattr + open + close per readFile, and no read');

  // The composition tree, as a host can print it.
  console.log(
    `    graph: ${graph.kind}(${graph.children.join(', ')}) over leaf ${pw.handle} (js)`
  );
});

// ---------------------------------------------------------------------------
// 2. router + memory + overlay — the other half of the spec's graph.
// ---------------------------------------------------------------------------

test('2. router dispatches by glob to a memory provider over an overlay default', async () => {
  const t = teardown();
  const scr = scratch(t, 'router');
  const docs = dirWith(scr, 'docs', {
    'prefs.txt': 'on-real-disk',
    'Logs/last.log': 'a log line',
  });
  const upper = dirWith(scr, 'scratch', {});

  const inis: Provider = memory({
    'Skyrim.ini': '[General]\nsLanguage=ENGLISH\n',
    'SkyrimPrefs.ini': '[Display]\niSize H=1080\n',
  });
  assert.strictEqual(caps(inis).access, 'readwrite', 'memory() is writable — that is its point');

  const def: Provider = overlay(disk(docs), disk(upper));
  assert.strictEqual(
    caps(def).access,
    'readwrite',
    'overlay reports ReadWrite regardless of base — copy-up is what makes that true'
  );

  const r: Provider = router({ '*.ini': inis }, def);
  assert.strictEqual(r.kind, 'router');
  assert.deepStrictEqual(r.children, [def.handle, inis.handle], 'default first, then routes');

  const s = newSession(t, 'router');
  // Spec §8 puts this on root 1. It is on root 0 under a prefix here because
  // `vfs_embed::Session::read_file` reads root 0 only — a §4 seam gap, noted in
  // the report rather than worked around with a second surface.
  s.addRoot(0, 'docs', dirWith(scr, 'docs-root', {}));
  s.mount(0, r, 'mygames');

  // The route wins for a matching path. Capitalised names are fine *here*
  // because a host-side `readFile` hands the graph the path as written — it is
  // only the injected child's path that the shim folds to lower case on its way
  // over the ring, which is test 6b's finding.
  assert.match(s.readFile('mygames/Skyrim.ini').toString(), /sLanguage=ENGLISH/);
  assert.match(s.readFile('mygames/SkyrimPrefs.ini').toString(), /iSize H=1080/);
  // ...and everything else goes to the default, through the overlay's base.
  assert.strictEqual(s.readFile('mygames/prefs.txt').toString(), 'on-real-disk');
  assert.strictEqual(s.readFile('mygames/Logs/last.log').toString(), 'a log line');

  // `*` does not cross a `/`: a nested .ini is *not* the route's, so it comes
  // from the default and is absent there.
  fs.writeFileSync(path.join(docs, 'Logs', 'nested.ini'), 'from-the-default');
  assert.strictEqual(s.readFile('mygames/Logs/nested.ini').toString(), 'from-the-default');

  // The documented Stage-1 gap, asserted so a future fix flips a failing test
  // rather than changing behaviour silently: §6 specifies `readdir` as a union
  // across the default plus every contributing route, and `RouterProvider`
  // returns only the answering child's listing. So the INIs are readable by
  // name and invisible to enumeration.
  const listed = names(s.readdir('mygames'));
  assert.ok(listed.includes('prefs.txt'), `default listing: ${listed.join(', ')}`);
  assert.ok(
    !listed.includes('Skyrim.ini'),
    'router readdir is single-dispatch today — see the report; a union would list it'
  );
  console.log(`    router readdir('mygames') = [${listed.join(', ')}]  (no *.ini — §6 gap)`);
});

// ---------------------------------------------------------------------------
// 3. Spec §7's rejected-write discovery, which needs `readonly` to exist.
// ---------------------------------------------------------------------------

test('3. a write refused by a readonly layer appears in rejectedWrites()', async () => {
  const t = teardown();
  const scr = scratch(t, 'rejected');
  const vanilla = dirWith(scr, 'vanilla', { 'vanilla.ini': '[General]\nuGridsToLoad=5\n' });
  const content = dirWith(scr, 'content', {});
  fs.copyFileSync(PROBE, path.join(content, 'probe.exe'));
  const src = path.join(scr, 'src.txt');
  fs.writeFileSync(src, 'what the game tried to write');

  const root = dirWith(scr, 'game-root', {});
  const s = newSession(t, 'rejected');
  s.addRoot(0, 'game', root);
  s.mount(0, disk(content));
  s.mount(0, readonly(disk(vanilla)), 'data');

  // Task 6 found `rejectedWrites()` could not be made non-empty from Node,
  // because `disk()` is ReadWrite. It is the readonly wrapper that makes the
  // director's own pre-check fire.
  s.resetRejectedWrites();
  assert.deepStrictEqual(s.rejectedWrites(), [], 'starting from a clean table');

  const target = path.join(root, 'data', 'vanilla.ini');
  const code: number = s.launch('probe.exe', { args: [src, target], wait: true });

  // The child's `fs::write` failed, so it exited non-zero. That is the *game's*
  // experience of a read-only layer, which is what §7's workflow starts from.
  assert.notStrictEqual(code, 0, 'the child could not write through a read-only layer');

  const rejected: RejectedWrite[] = s.rejectedWrites();
  console.log(`    rejectedWrites(): ${JSON.stringify(rejected)}  (child exit ${code})`);
  const hit = rejected.find((r) => r.path.replace(/\\/g, '/').endsWith('vanilla.ini'));
  assert.ok(hit, `the refused write is discoverable by path; got ${JSON.stringify(rejected)}`);
  assert.ok(hit!.count >= 1);

  // Nothing was written anywhere: not through the read-only layer, not into the
  // writable sibling mount, not onto the managed root.
  assert.strictEqual(
    fs.readFileSync(path.join(vanilla, 'vanilla.ini')).toString(),
    '[General]\nuGridsToLoad=5\n',
    'the protected file is byte-for-byte unchanged'
  );
  assert.deepStrictEqual(
    fs.readdirSync(content).sort(),
    ['probe.exe'],
    'and the write did not leak into the writable mount beside it'
  );

  // The control, and it is the half that makes this test mean anything: the same
  // launch against the same directory *without* the readonly wrapper succeeds
  // and records no new rejection. Otherwise "count >= 1" could come from
  // anywhere — the table is process-wide.
  const writable = dirWith(scr, 'writable', { 'vanilla.ini': 'original' });
  const s2 = newSession(t, 'rejected-control');
  s2.addRoot(0, 'game', dirWith(scr, 'game-root-2', {}));
  s2.mount(0, disk(content));
  s2.mount(0, disk(writable), 'data');
  s.resetRejectedWrites();
  const code2: number = s2.launch('probe.exe', {
    args: [src, path.join(s2.virtualRoot, 'data', 'vanilla.ini')],
    wait: true,
  });
  assert.strictEqual(code2, 0, 'the control write succeeds');
  assert.deepStrictEqual(
    s2.rejectedWrites(),
    [],
    'a writable layer refuses nothing — so §7 needed readonly() to be demonstrable at all'
  );
  assert.strictEqual(
    fs.readFileSync(path.join(writable, 'vanilla.ini')).toString(),
    'what the game tried to write',
    'and the control really did write through the director'
  );
});

// ---------------------------------------------------------------------------
// 4. Composition must not lose the handle a host has to release.
// ---------------------------------------------------------------------------

test('4. a composed graph still knows which handle has a loop to release', async () => {
  const t = teardown();
  const pw = await cdn(t);
  const composed: Provider = cached(readonly(seekable(pw.provider)));

  // Three new handles, none of them the one `releaseProvider` takes. That is the
  // trap: a live threadsafe function keeps its worker's loop alive, so a host
  // that releases the wrong handle has a process that will not exit.
  assert.notStrictEqual(composed.handle, pw.handle);
  assert.strictEqual(composed.stats(), null, 'a Rust combinator has no bridge and no counters');
  assert.throws(
    () => releaseProvider(composed.handle),
    (e: Error) => {
      assert.match(e.message, /not a JS-backed provider/);
      console.log(`    releaseProvider(composed) refused: ${e.message.split('.')[0]}.`);
      return true;
    }
  );

  // And this is how the host finds the right one.
  assert.deepStrictEqual(composed.jsLeaves(), [pw.handle]);
  assert.strictEqual(pw.provider.stats()!.handle, pw.handle);

  // A graph with no JS in it says so, rather than guessing.
  const allRust: Provider = layered(memory({ 'a.txt': 'a' }), memory({ 'b.txt': 'b' }));
  assert.deepStrictEqual(allRust.jsLeaves(), []);

  // Two JS leaves under one graph are both reported, in argument order.
  const pw2 = await cdn(t, { depot: '22330' });
  const two: Provider = layered(seekable(pw.provider), seekable(pw2.provider));
  assert.deepStrictEqual(two.jsLeaves(), [pw.handle, pw2.handle]);
  console.log(`    jsLeaves of layered(seekable(a), seekable(b)) = [${two.jsLeaves().join(', ')}]`);
});

// ---------------------------------------------------------------------------
// 5. §6's capability recomputation table, every row, from JavaScript.
// ---------------------------------------------------------------------------

test('5. every capability recomputation rule in §6 holds', async () => {
  const t = teardown();
  const scr = scratch(t, 'caps');
  const d: Provider = disk(dirWith(scr, 'd', { 'x.txt': 'x' }));
  const m: Provider = memory({ 'y.txt': 'y' });
  const pw = await cdn(t);

  // readonly clamps ReadWrite → Read, and leaves anything weaker alone. The
  // second half matters: a `readonly(seqread)` reporting Read would tell the
  // director it may issue positional reads against a provider with no readAt.
  assert.strictEqual(caps(d).access, 'readwrite');
  assert.strictEqual(caps(readonly(d)).access, 'read');
  assert.strictEqual(caps(readonly(pw.provider)).access, 'seqread');

  // seekable promotes SeqRead → Read and is a passthrough otherwise.
  assert.strictEqual(caps(seekable(pw.provider)).access, 'read');
  assert.strictEqual(caps(seekable(d)).access, 'readwrite');

  // cached passes access through and clears slow — and `slow` surviving is
  // exactly how mount() knows to warn, so this is load-bearing, not cosmetic.
  assert.strictEqual(caps(pw.provider).slow, true);
  assert.strictEqual(caps(cached(pw.provider)).slow, false);
  assert.strictEqual(caps(cached(d)).access, 'readwrite');
  assert.strictEqual(caps(cached(pw.provider)).access, 'seqread', 'cached does not promote');

  // overlay reports ReadWrite regardless of base, and refuses a read-only upper
  // at construction rather than at the first write.
  assert.strictEqual(caps(overlay(readonly(d), m)).access, 'readwrite');
  assert.throws(
    () => overlay(d, readonly(m)),
    (e: Error) => {
      assert.match(e.message, /upper must declare Access::ReadWrite/);
      console.log(`    overlay(base, readonly(upper)) refused: ${e.message.split('.')[0]}.`);
      return true;
    }
  );

  // layered: strongest access (any writable child can take a write), immutable
  // only if every child is, slow if any child is.
  assert.strictEqual(caps(layered(readonly(d), d)).access, 'readwrite');
  assert.strictEqual(caps(layered(readonly(d), readonly(d))).access, 'read');
  assert.strictEqual(caps(layered(m, d)).immutable, false);
  assert.strictEqual(
    caps(layered(readonly(pw.provider), readonly(pw.provider))).immutable,
    true,
    'two immutable children make an immutable stack'
  );
  assert.strictEqual(caps(layered(seekable(pw.provider), d)).slow, true, 'slow if any child is');

  // router: weakest across the default and every route.
  assert.strictEqual(caps(router({ '*.ini': m }, d)).access, 'readwrite');
  assert.strictEqual(caps(router({ '*.ini': readonly(m) }, d)).access, 'read');

  // A one-provider stack is refused rather than silently returned as itself: a
  // list of one is far more often a bug in how the list was built.
  assert.throws(() => layered(d), /at least two providers/);
  assert.throws(() => layered([]), /at least two providers/);

  // A primitive takes a Provider or its raw integer handle, and nothing else —
  // the integer is what actually crosses an isolate boundary.
  assert.strictEqual(caps(readonly(d.handle)).access, 'read');
  assert.throws(() => readonly({} as Provider), /needs a Provider/);
  assert.throws(() => readonly(null as unknown as Provider), /needs a Provider/);

  // Every handle knows what made it, which is what makes a graph of integers
  // printable.
  assert.deepStrictEqual(
    [d, m, pw.provider, readonly(d), seekable(d), cached(d), layered(d, m), overlay(d, m), router({}, d)].map(
      (p: Provider) => p.kind
    ),
    ['disk', 'memory', 'js', 'readonly', 'seekable', 'cached', 'layered', 'overlay', 'router']
  );

  // KIND is exported from Rust so a readdir consumer holds no second copy of the
  // numbers.
  assert.strictEqual(typeof KIND.KIND_DIR, 'number');
});

// ---------------------------------------------------------------------------
// 6. memory(): the round trip spec §8 asks for — a game writes, the host reads.
// ---------------------------------------------------------------------------

/** The shared setup for the two halves below: a seeded `memory()` under a prefix. */
function memoryRoundTrip(t: TestTeardown, name: string) {
  const scr = scratch(t, name);
  const content = dirWith(scr, 'content', {});
  fs.copyFileSync(PROBE, path.join(content, 'probe.exe'));
  const src = path.join(scr, 'new.ini');
  fs.writeFileSync(src, '[General]\nuGridsToLoad=7\n');

  const inis: Provider = memory({ 'skyrim.ini': '[General]\nuGridsToLoad=5\n' });
  const root = dirWith(scr, `game-root-${name}`, {});
  const s = newSession(t, name);
  s.addRoot(0, 'game', root);
  s.mount(0, disk(content));
  s.mount(0, inis, 'mygames');
  return { s, root, src, inis };
}

test('6. the game writes an INI into memory() and the host reads back what it wrote', async () => {
  const t = teardown();
  // **Every vpath here is lower-case, and that is not a style choice.** The shim
  // folds each vpath component with `vfs_core::fold` before it crosses the ring,
  // and `MemoryProvider` keys a plain `HashMap` on the exact string it is given.
  // So a write from an injected process to `Skyrim.ini` arrives as `skyrim.ini`
  // and lands in a *different* entry from the one a host seeded. Test 6b is that
  // case, and §6's answer to it — `casefold(p)` — does not exist in Rust. See
  // the report.
  const { s, root, src } = memoryRoundTrip(t, 'memory');

  // Seeded content is readable through the graph before anything runs.
  assert.match(s.readFile('mygames/skyrim.ini').toString(), /uGridsToLoad=5/);

  const code: number = s.launch('probe.exe', {
    args: [src, path.join(root, 'mygames', 'custom.ini')],
    wait: true,
  });
  assert.strictEqual(code, 0, "the child's write reached the memory provider");

  // Read back through the graph. Nothing touched disk: the managed root is
  // empty and the bytes live in a Rust `HashMap`.
  assert.match(
    s.readFile('mygames/custom.ini').toString(),
    /uGridsToLoad=7/,
    'the host reads back exactly what the game wrote'
  );
  assert.deepStrictEqual(fs.readdirSync(root), [], 'and the managed root is still empty on disk');
  assert.deepStrictEqual(s.rejectedWrites(), [], 'a writable provider refuses nothing');
  console.log(`    memory() round trip: ${s.readFile('mygames/custom.ini').toString().trim()}`);

  // And the overwrite works when no folding is involved, which is what isolates
  // 6b's cause to the case difference and not to "writes to an existing path".
  const code2: number = s.launch('probe.exe', {
    args: [src, path.join(root, 'mygames', 'skyrim.ini')],
    wait: true,
  });
  assert.strictEqual(code2, 0);
  assert.match(
    s.readFile('mygames/skyrim.ini').toString(),
    /uGridsToLoad=7/,
    'an all-lower-case path the provider already holds is overwritten correctly'
  );
});

// The same round trip on a path with a capital letter in it — which is every INI
// the spec's own example names (`Skyrim.ini`, `SkyrimPrefs.ini`).
//
// **The mechanism, measured rather than guessed.** The shim folds vpath
// components to lower case before they cross the ring; `MemoryProvider` is
// case-sensitive by design (spec §10: *"a case-sensitive provider gains
// correctness from the `casefold` combinator rather than from every provider
// reimplementing folding"*). So the child's write to `Skyrim.ini` creates
// `skyrim.ini` beside the host's `Skyrim.ini`, the host reading back with the
// original case gets its own stale bytes, and **nothing anywhere reports an
// error**. `readdir` shows one entry rather than two only because the director
// dedupes listings by folded name.
//
// `casefold(p)` is spec §6's ninth primitive and the fix; it does not exist in
// Rust, so this is not something the binding can compose its way out of. The
// assertion states the behaviour a host is entitled to, so a fix turns it green
// for the right reason, while a passing test asserting today's behaviour would
// freeze the bug in place.
//
// ## `test.fails`, and the two things not to do to it
//
// This was `{ todo: … }` under node:test. In vitest it is **`test.fails`, never
// `skip` and never `todo`** — a skip deletes exactly the evidence this test
// exists to preserve and leaves a green name behind. `test.fails` is also a
// *stricter* contract than node's `todo`: node tolerates a todo that passes,
// while `test.fails` goes red with `Expected test to fail` the day `casefold`
// lands. That is the direction to be strict in.
//
// **Do not add a `throw` to the wrapper below for the "it started passing" case.**
// Task 1 did, and it silently defeated the whole mechanism: a throw inside a
// `.fails` body is precisely what `.fails` is looking for, so a passing todo
// reported green. Verified then by a two-todo probe. The `catch` here only logs
// and rethrows, because vitest prints nothing for a `.fails` test that duly
// failed and the failing assertion is the entire point of this one.
test.fails(
  '6b. a capitalised path in memory() (known-failing — §6 casefold does not exist)',
  async () => {
    const t = teardown();
    const why =
      'the shim folds the vpath; memory() is case-sensitive; the write lands beside the seed';
    try {
      const { s, root, src } = memoryRoundTrip(t, 'memory-fold');
      // Seed under a capitalised name, exactly as spec §8's example does.
      s.mount(0, memory({ 'Skyrim.ini': '[General]\nuGridsToLoad=5\n' }), 'caps');
      const code: number = s.launch('probe.exe', {
        args: [src, path.join(root, 'caps', 'Skyrim.ini')],
        wait: true,
      });
      assert.strictEqual(code, 0, 'the child believes it wrote');
      assert.deepStrictEqual(s.rejectedWrites(), [], 'and nothing refused it');
      // The evidence, printed whether the assertion below holds or not.
      console.log(`    readdir('caps') = ${JSON.stringify(names(s.readdir('caps')))}`);
      assert.match(
        s.readFile('caps/Skyrim.ini').toString(),
        /uGridsToLoad=7/,
        'the host must read back what the game wrote, under the name it seeded'
      );
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      console.log(
        `    known-failing (node:test todo -> test.fails): ${why}\n` +
          `    the assertion still fails, as intended: ${message}`
      );
      throw err;
    }
  }
);

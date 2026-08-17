// **`using` makes release structural instead of remembered.**
//
// `releaseProvider` is the one teardown in this API whose omission has no
// diagnostic: a live threadsafe function holds a ref on the loop that services
// it, so a leaked handle is a thread that never exits — and nothing that runs
// "on the way out" ever runs, because the loop never drains. That is why the
// disposables are worth having and why they are tested with the real `using`
// syntax rather than by calling `[Symbol.dispose]()` by hand: what matters is
// that the language invokes them, including on the throwing path.
//
// **The `using` declarations below are now transformed rather than native.** Under
// `node --test` this file was `.cjs` and node's own Explicit Resource Management
// implementation ran it; as a `.cts` under vitest, vite's esbuild lowers `using`
// to `__addDisposableResource`/`__disposeResources`. That is a real change of
// mechanism, and it is checked by the only thing that could check it — these
// tests, which assert that the dispose actually happened, including out of a
// `throw`. A broken lowering fails them rather than passing quietly.

import { test } from 'vitest';
import assert from 'node:assert';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

import type { ProviderObject, Session } from '../index.cjs';

const vfs: typeof import('../index.cjs') = require('..');

/** A minimal conforming read-only provider; nothing here is under test. */
function stubProvider(): ProviderObject {
  return {
    capabilities: { access: 'read' },
    getattr: () => null,
    readdir: () => [],
    open: () => {
      throw new vfs.VfsError('ST_NOT_FOUND');
    },
    close: () => {},
    readAt: () => Buffer.alloc(0),
  };
}

test('using releases a JS provider at the end of the block', () => {
  let handle: number | undefined;
  {
    using p = vfs.registerProvider(stubProvider());
    handle = p.handle;
    assert.strictEqual(p.stats()!.released, false, 'released before the block ended');
  }
  const after = vfs.Provider.fromHandle(handle!).stats()!;
  assert.strictEqual(after.released, true, '`using` did not release the provider');
});

test('using releases even when the block throws — the case a finally is forgotten in', () => {
  let handle: number | undefined;
  assert.throws(() => {
    using p = vfs.registerProvider(stubProvider());
    handle = p.handle;
    throw new Error('boom');
  }, /boom/);
  assert.strictEqual(
    vfs.Provider.fromHandle(handle!).stats()!.released,
    true,
    'a throw skipped the release'
  );
});

test('disposing a composed handle releases its JS leaves, not the wrapper', () => {
  let leaf: number | undefined;
  {
    const inner = vfs.registerProvider(stubProvider());
    leaf = inner.handle;
    using composed = vfs.cached(vfs.seekable(inner), { ramBytes: 1 << 20 });
    // The trap this exists for: the composed handle is not the one to release,
    // and asking Rust to release it correctly refuses.
    assert.throws(() => vfs.releaseProvider(composed.handle), /not a JS-backed provider/);
    assert.deepStrictEqual(composed.jsLeaves(), [leaf]);
  }
  assert.strictEqual(
    vfs.Provider.fromHandle(leaf!).stats()!.released,
    true,
    'disposing the composition did not release the JS leaf underneath it'
  );
});

test('disposing a graph of Rust primitives is a no-op rather than an error', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'aethervfs-dispose-'));
  {
    using p = vfs.layered(vfs.disk(dir), vfs.memory({ 'a.txt': 'x' }));
    assert.deepStrictEqual(p.jsLeaves(), [], 'a Rust graph has no leaves to release');
  }
});

test('using closes a Session at the end of the block', () => {
  let s: Session | undefined;
  {
    using session = new vfs.Session('dispose-session');
    s = session;
    assert.strictEqual(session.isServing(), false);
  }
  // A closed session throws on every accessor that needs the Rust side.
  assert.throws(() => s!.isServing(), /closed/);
});

test('releaseProvider is idempotent and accepts a Provider as well as a handle', () => {
  const p = vfs.registerProvider(stubProvider());
  vfs.releaseProvider(p);
  vfs.releaseProvider(p.handle);
  assert.strictEqual(p.stats()!.released, true);
});

// **A handle is an index, so a near-miss is a different live provider.**
//
// Every other wrapper in `index.cts` takes its argument through `handleOf`, which
// requires a non-negative integer. `releaseProvider` did not, and it is the one
// where skipping the check is destructive rather than merely wrong: Rust coerces
// the number, so `releaseProvider(1.7)` released handle **1** and
// `releaseProvider(NaN)` released handle **0** — a live provider belonging to
// something else, with no error anywhere and no way to notice until a later call
// on it fails with a released-loop status.
test('releaseProvider refuses a non-integer handle instead of releasing its neighbour', () => {
  const keep = vfs.registerProvider(stubProvider());
  try {
    for (const bad of [keep.handle + 0.7, NaN, Infinity, -1, -0.5]) {
      assert.throws(
        () => vfs.releaseProvider(bad),
        /non-negative integer/,
        `releaseProvider(${bad}) was accepted`
      );
      assert.strictEqual(
        keep.stats()!.released,
        false,
        `releaseProvider(${bad}) released handle ${keep.handle}, which nobody asked it to touch`
      );
    }
  } finally {
    vfs.releaseProvider(keep.handle);
  }
  assert.strictEqual(keep.stats()!.released, true, 'the valid release stopped working');
});

test('a leaked provider is named on exit, in a child so the warning can be observed', () => {
  const { spawnSync }: typeof import('node:child_process') = require('node:child_process');
  const pkg = path.resolve(__dirname, '..');
  const r = spawnSync(
    process.execPath,
    [
      '-e',
      `const vfs = require(${JSON.stringify(pkg)});
       vfs.registerProvider({
         capabilities: { access: 'read' },
         getattr: () => null, readdir: () => [],
         open: () => { throw new vfs.VfsError('ST_NOT_FOUND'); },
         close: () => {}, readAt: () => Buffer.alloc(0),
       });
       // Never released. Without process.exit() this child would hang forever,
       // which is the whole point — and is also why the warning can only be
       // emitted on an exit that someone asks for.
       process.exit(0);`,
    ],
    { encoding: 'utf8', cwd: pkg }
  );
  assert.strictEqual(r.status, 0);
  assert.match(r.stderr, /AetherVfsProviderLeak/, `stderr was: ${r.stderr}`);
  assert.match(r.stderr, /1 JS provider\(s\) were never released/);
});

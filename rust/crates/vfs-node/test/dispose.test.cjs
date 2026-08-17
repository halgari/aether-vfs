'use strict';

// **`using` makes release structural instead of remembered.**
//
// `releaseProvider` is the one teardown in this API whose omission has no
// diagnostic: a live threadsafe function holds a ref on the loop that services
// it, so a leaked handle is a thread that never exits — and nothing that runs
// "on the way out" ever runs, because the loop never drains. That is why the
// disposables are worth having and why they are tested with the real `using`
// syntax rather than by calling `[Symbol.dispose]()` by hand: what matters is
// that the language invokes them, including on the throwing path.

const assert = require('node:assert');
const { test } = require('node:test');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const vfs = require('..');

/** A minimal conforming read-only provider; nothing here is under test. */
function stubProvider() {
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
  let handle;
  {
    using p = vfs.registerProvider(stubProvider());
    handle = p.handle;
    assert.strictEqual(p.stats().released, false, 'released before the block ended');
  }
  const after = vfs.Provider.fromHandle(handle).stats();
  assert.strictEqual(after.released, true, '`using` did not release the provider');
});

test('using releases even when the block throws — the case a finally is forgotten in', () => {
  let handle;
  assert.throws(() => {
    using p = vfs.registerProvider(stubProvider());
    handle = p.handle;
    throw new Error('boom');
  }, /boom/);
  assert.strictEqual(
    vfs.Provider.fromHandle(handle).stats().released,
    true,
    'a throw skipped the release'
  );
});

test('disposing a composed handle releases its JS leaves, not the wrapper', () => {
  let leaf;
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
    vfs.Provider.fromHandle(leaf).stats().released,
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
  let s;
  {
    using session = new vfs.Session('dispose-session');
    s = session;
    assert.strictEqual(session.isServing(), false);
  }
  // A closed session throws on every accessor that needs the Rust side.
  assert.throws(() => s.isServing(), /closed/);
});

test('releaseProvider is idempotent and accepts a Provider as well as a handle', () => {
  const p = vfs.registerProvider(stubProvider());
  vfs.releaseProvider(p);
  vfs.releaseProvider(p.handle);
  assert.strictEqual(p.stats().released, true);
});

test('a leaked provider is named on exit, in a child so the warning can be observed', () => {
  const { spawnSync } = require('node:child_process');
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

'use strict';

// The providers the task-7 tests mount. One module with a factory, because
// `providerWorker({ module })` resolves a **module path inside the worker** — the
// constraint spec §8c records, since isolates share no JS objects — and one
// factory is easier to keep honest than eight files.
//
// `make(options)` is the default export, so the worker's `mod.provider ??
// mod.default ?? mod` picks it and calls it with `workerData.options`. The same
// function is required directly by the test file for the cases that must be
// serviced by the main loop (the deadlock guard), so every provider here is
// exercised on both kinds of loop.

const path = require('path');

const aether = require(path.join(__dirname, '..', 'index.cjs'));
const { VfsError, OPEN } = aether;

/** Sleep as a promise, so `async` provider methods are genuinely late. */
const after = (ms, value) => new Promise((r) => setTimeout(() => r(value), ms));

/**
 * A read-only provider over an object of `{ name: string|Buffer }`.
 *
 * `hooks` is where each test's one interesting behaviour goes, keyed by path, so
 * the boring 90% of a provider is written once: a hook is called instead of the
 * ordinary `readAt` and may return bytes, return a promise, or throw.
 */
function memoryProvider(files, { access = 'read', hooks = {}, writable = false } = {}) {
  const content = new Map(
    Object.entries(files).map(([k, v]) => [k, Buffer.isBuffer(v) ? v : Buffer.from(String(v))])
  );
  const open = new Map();
  let nextHandle = 1;

  const provider = {
    capabilities: {
      access,
      // A `ReadWrite` provider may not declare `immutable` — `Capabilities::validate`
      // refuses the pair, and that check fires *before* the missing-method one, so
      // getting this wrong here would make `readWriteWithoutWriteAt` prove the
      // wrong thing.
      immutable: access !== 'readwrite' && !writable,
      slow: true,
      preferredBlock: 65536,
    },

    getattr(root, p) {
      if (p === '') return { kind: 'dir', size: 0 };
      const b = content.get(p);
      if (b === undefined) return null;
      return { kind: 'file', size: b.length, mtime: 0 };
    },

    readdir(root, p) {
      if (p !== '') return [];
      return [...content].map(([name, b]) => ({ name, kind: 'file', size: b.length }));
    },

    open(root, p, flags) {
      let b = content.get(p);
      if (b === undefined) {
        const creating = writable && (flags & OPEN.OPEN_CREATE) !== 0;
        if (!creating) throw new VfsError('ST_NOT_FOUND', `no such path ${JSON.stringify(p)}`);
        b = Buffer.alloc(0);
        content.set(p, b);
      } else if (writable && (flags & OPEN.OPEN_TRUNC) !== 0) {
        b = Buffer.alloc(0);
        content.set(p, b);
      }
      const h = nextHandle++;
      open.set(h, p);
      return { handle: h, size: b.length, isDir: false };
    },

    close(h) {
      open.delete(h);
    },

    readAt(h, offset, len) {
      const p = open.get(h);
      if (p === undefined) throw new VfsError('ST_BAD_FH');
      const hook = hooks[p];
      if (hook) return hook(offset, len);
      return content.get(p).subarray(offset, offset + len);
    },
  };

  if (writable) {
    provider.writeAt = (h, offset, data) => {
      const p = open.get(h);
      if (p === undefined) throw new VfsError('ST_BAD_FH');
      const old = content.get(p) ?? Buffer.alloc(0);
      const end = Math.max(old.length, offset + data.length);
      const next = Buffer.alloc(end);
      old.copy(next);
      data.copy(next, offset);
      content.set(p, next);
      return data.length;
    };
    provider.setLen = (h, len) => {
      const p = open.get(h);
      const old = content.get(p) ?? Buffer.alloc(0);
      const next = Buffer.alloc(Number(len));
      old.copy(next, 0, 0, Math.min(old.length, next.length));
      content.set(p, next);
    };
    provider.flush = () => {};
    // So a test can read back what the injected child wrote.
    provider.$content = content;
  }

  return provider;
}

const KINDS = {
  /** Plain, synchronous, read-only. The baseline: bytes through the director. */
  bytes: () =>
    memoryProvider({
      'js-served.txt': 'bytes-from-javascript',
      'ok.txt': 'ok',
      'small.bin': Buffer.alloc(64, 0xab),
    }),

  /** Every read is a promise that resolves late. Spec §8b's `async` provider. */
  async: ({ delayMs = 150 } = {}) =>
    memoryProvider(
      { 'late.txt': 'resolved-after-a-delay', 'ok.txt': 'ok' },
      {
        hooks: {
          'late.txt': (offset, len) =>
            after(delayMs, Buffer.from('resolved-after-a-delay').subarray(offset, offset + len)),
        },
      }
    ),

  /** Each path fails a different way, so the mapping in §8b rule 3 is visible. */
  errors: () =>
    memoryProvider(
      {
        'readonly.txt': 'x',
        'nospace.txt': 'x',
        'bogus.txt': 'x',
        'badshape.txt': 'x',
        'okerror.txt': 'x',
        'boom.txt': 'x',
        'rejected.txt': 'x',
        'ok.txt': 'ok',
      },
      {
        hooks: {
          // A deliberate status, by name and by number.
          'readonly.txt': () => {
            throw new VfsError('ST_READ_ONLY', 'this source is read-only');
          },
          'nospace.txt': () => {
            throw new VfsError(aether.STATUS.ST_NO_SPACE);
          },
          // A status the workspace does not define. Rust must clamp it rather
          // than let a host inject an arbitrary code into the director.
          'bogus.txt': () => {
            const e = new Error('a status nobody defined');
            e.vfsStatus = 12345;
            throw e;
          },
          // A `VfsError` naming success. Failing with ST_OK would be read as "the
          // call worked and returned nothing", so the `threw` flag has to survive
          // the crossing independently of the status.
          'okerror.txt': () => {
            throw new VfsError('ST_OK', 'a failure that claims to be a success');
          },
          // Returns something that is not bytes at all. The throw comes from the
          // dispatcher's own coercion rather than from the host, and must be
          // caught in the same place — an exception escaping a
          // threadsafe-function callback kills the process, not the call.
          'badshape.txt': () => 42,
          // Not a VfsError: ST_IO_ERROR, with the stack logged.
          'boom.txt': () => {
            throw new Error('boom — a plain throw from a JS provider');
          },
          // The same, arriving as a rejection rather than a throw.
          'rejected.txt': () =>
            Promise.reject(new Error('rejected — a promise that says no')),
        },
      }
    ),

  /** One path never settles; everything else works. */
  never: () =>
    memoryProvider(
      { 'never.txt': 'x', 'ok.txt': 'ok' },
      {
        hooks: {
          // eslint-disable-next-line no-empty-function
          'never.txt': () => new Promise(() => {}),
        },
      }
    ),

  /** Settles, but only after the stall threshold has passed. */
  slow: ({ delayMs = 250 } = {}) =>
    memoryProvider(
      { 'slow.txt': 'eventually' },
      { hooks: { 'slow.txt': () => after(delayMs, Buffer.from('eventually')) } }
    ),

  /** Declares ReadWrite and has no `writeAt`. Registration must refuse it. */
  readWriteWithoutWriteAt: () => {
    const p = memoryProvider({ 'a.txt': 'a' }, { access: 'readwrite' });
    delete p.writeAt;
    return p;
  },

  /** A real ReadWrite provider: the positive control for the rule above. */
  readWrite: () => memoryProvider({ 'seed.txt': 'seed' }, { access: 'readwrite', writable: true }),

  /** Declares SeqRead but only has `readAt`. The same rule, other access tier. */
  seqReadWithoutReadNext: () => memoryProvider({ 'a.txt': 'a' }, { access: 'seqread' }),
};

module.exports = function make(options = {}) {
  const kind = options.kind ?? 'bytes';
  const factory = KINDS[kind];
  if (!factory) {
    throw new Error(
      `test/providers.cjs: no provider kind ${JSON.stringify(kind)}; have ${Object.keys(KINDS).join(', ')}`
    );
  }
  return factory(options);
};

module.exports.KINDS = KINDS;
module.exports.memoryProvider = memoryProvider;

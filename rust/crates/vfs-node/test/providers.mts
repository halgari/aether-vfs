// The providers the task-7 tests mount. One module with a factory, because
// `providerWorker({ module })` resolves a **module path inside the worker** — the
// constraint spec §8c records, since isolates share no JS objects — and one
// factory is easier to keep honest than eight files.
//
// `make(options)` is the default export, so the worker's loader
// (`data.export ? mod[data.export] : (mod.provider ?? mod.default)`) picks it up
// and calls it with `workerData.options`. The same function is imported directly
// by the test file for the cases that must be serviced by the main loop (the
// deadlock guard), so every provider here is exercised on both kinds of loop.
//
// ## ESM, as of task 3
//
// This module is loaded by **node**: inside a provider worker via
// `await import(pathToFileURL(data.module).href)`, and by the test file and
// `self-call-worker.mts` through an ordinary static `import`. There is no
// `require` left anywhere in this file, and no CommonJS shape (`module.exports`)
// to keep in sync with a hand-written type — a real `import` is what makes
// `assert.ok` an assertion function to TypeScript (TS2775), which
// `require(...) as typeof import(...)` never was.

import type { ProviderDirEntry, ProviderObject, ProviderStat } from '../index.mjs';

import * as aether from '../index.mjs';

const { VfsError, OPEN } = aether;

// `OPEN` is a `Record<string, number>` read from Rust at load, so under
// `noUncheckedIndexedAccess` every lookup is `number | undefined`. Naming the two
// bits this fixture uses once keeps the `&` expressions below readable.
const OPEN_CREATE: number = OPEN.OPEN_CREATE!;
const OPEN_TRUNC: number = OPEN.OPEN_TRUNC!;

/** Anything the fixture accepts as file content. */
type Bytes = string | Buffer | Uint8Array;

/**
 * A per-path override for `readAt`.
 *
 * The return type is `unknown` on purpose: one hook (`badshape.txt`) returns a
 * number, which is not bytes at all, and that is the case task 7's rule 4 exists
 * to prove the dispatcher catches. A hook typed as returning bytes could not
 * express it.
 */
type Hook = (offset: number, len: number) => unknown;

interface MemoryProviderOptions {
  access?: 'read' | 'readwrite' | 'seqread';
  hooks?: Record<string, Hook>;
  writable?: boolean;
}

/** Options `make()` understands. `kind` selects a factory from `KINDS`. */
export interface MakeOptions {
  kind?: string;
  delayMs?: number;
}

/**
 * A provider this fixture builds.
 *
 * `$content` is not part of the provider contract — it is how a test reads back
 * what an injected child wrote, from inside the isolate that holds the bytes.
 */
export interface TestProvider extends ProviderObject {
  $content?: Map<string, Buffer>;
}

// Sleep as a promise, so `async` provider methods are genuinely late.
//
// A declaration rather than the arrow this was, because `<T>(…) =>` in a `.cts`
// file is TS7060: the extension reserves that syntax, and `<T,>` is the
// alternative. A named function is the readable half of that choice.
function after<T>(ms: number, value: T): Promise<T> {
  return new Promise((r) => setTimeout(() => r(value), ms));
}

/**
 * A read-only provider over an object of `{ name: string|Buffer }`.
 *
 * `hooks` is where each test's one interesting behaviour goes, keyed by path, so
 * the boring 90% of a provider is written once: a hook is called instead of the
 * ordinary `readAt` and may return bytes, return a promise, or throw.
 */
function memoryProvider(
  files: Record<string, Bytes>,
  { access = 'read', hooks = {}, writable = false }: MemoryProviderOptions = {}
): TestProvider {
  const content = new Map<string, Buffer>(
    Object.entries(files).map(([k, v]) => [k, Buffer.isBuffer(v) ? v : Buffer.from(String(v))])
  );
  const open = new Map<number, string>();
  let nextHandle = 1;

  const provider: TestProvider = {
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

    getattr(root, p): ProviderStat | null {
      if (p === '') return { kind: 'dir', size: 0 };
      const b = content.get(p);
      if (b === undefined) return null;
      return { kind: 'file', size: b.length, mtime: 0 };
    },

    readdir(root, p): ProviderDirEntry[] {
      if (p !== '') return [];
      return [...content].map(([name, b]): ProviderDirEntry => ({ name, kind: 'file', size: b.length }));
    },

    open(root, p, flags) {
      let b = content.get(p);
      if (b === undefined) {
        const creating = writable && (flags & OPEN_CREATE) !== 0;
        if (!creating) throw new VfsError('ST_NOT_FOUND', `no such path ${JSON.stringify(p)}`);
        b = Buffer.alloc(0);
        content.set(p, b);
      } else if (writable && (flags & OPEN_TRUNC) !== 0) {
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
      // The cast is the type system agreeing with the test: `badshape.txt`'s hook
      // returns a number, TypeScript would have refused to write it, and what is
      // under test is the **runtime** coercion in `index.mts` that turns it into
      // `ST_IO_ERROR` instead of killing the process.
      if (hook) return hook(offset, len) as Uint8Array;
      return content.get(p)!.subarray(offset, offset + len);
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
    // The `!`s are the faithful translation of the JavaScript, which did not
    // check the handle here either: `setLen` is only ever reached through an
    // `open` this provider issued. Guarding it would change runtime behaviour,
    // which a conversion is not for.
    provider.setLen = (h, len) => {
      const p = open.get(h)!;
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

type ProviderFactory = (options: MakeOptions) => TestProvider;

const KINDS: Record<string, ProviderFactory> = {
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
            throw new VfsError(aether.STATUS.ST_NO_SPACE!);
          },
          // A status the workspace does not define. Rust must clamp it rather
          // than let a host inject an arbitrary code into the director.
          'bogus.txt': () => {
            const e = new Error('a status nobody defined') as Error & { vfsStatus?: number };
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
          'rejected.txt': () => Promise.reject(new Error('rejected — a promise that says no')),
        },
      }
    ),

  /** One path never settles; everything else works. */
  never: () =>
    memoryProvider(
      { 'never.txt': 'x', 'ok.txt': 'ok' },
      {
        hooks: {
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

function make(options: MakeOptions = {}): TestProvider {
  const kind = options.kind ?? 'bytes';
  const factory = KINDS[kind];
  if (!factory) {
    throw new Error(
      `test/providers.mts: no provider kind ${JSON.stringify(kind)}; have ${Object.keys(KINDS).join(', ')}`
    );
  }
  return factory(options);
}

/** What a default import of this module gives back — the shape the provider
 * worker's `data.export ? mod[data.export] : (mod.provider ?? mod.default)`
 * picks up. `KINDS` and `memoryProvider` are named exports for anything that
 * wants the pieces rather than the assembled factory. */
export default make;
export { KINDS, memoryProvider };

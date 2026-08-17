// The providers task 9's conformance tests run: one honest, two that lie.
//
// **The reference tree comes from Rust**, via `conformanceFixture()`. Hard-coding
// `a.txt` / `sub/b.txt` here would put a second copy of the contract in a place
// nothing keeps in step with the first — `FIXTURE_FILES` gains a file and a
// hard-coded provider fails with "readdir of the root listed …" and no clue why.
//
// The honest one is deliberately **minimal**: it is what spec §8 says a host
// writes and no more — `capabilities`, `getattr`, `readdir`, `open`, `close`,
// `readNext`. No `readAt`, which is not an omission but the declaration: a
// forward-only source must *refuse* positional reads, and the suite checks that
// it does. The bridge answers `ST_NOT_SUPPORTED` for a method the object does not
// have, without a round trip, exactly as the trait's own default does for Rust.
//
// The liars exist because a conformance runner that passes everything is
// indistinguishable from no runner. Both are shaped to get **past** task 7's
// registration-time validation, which already refuses the easy lies (`readwrite`
// with no `writeAt`, `seqread` with no `readNext`). What is left is the class only
// running the suite can catch: a provider whose methods are all present and whose
// behaviour does not match what it declared.
//
// `require` and not `import`, and an annotation rather than a cast: see the header
// of `providers.cts`. This module is resolved and loaded by node — on the main
// loop by the test file, and inside a worker by `providerWorker({ module })`.

import type { ProviderDirEntry, ProviderObject, ProviderStat } from '../index.cjs';

const path: typeof import('node:path') = require('path');

const aether: typeof import('../index.cjs') = require(path.join(__dirname, '..', 'index.cjs'));
const { VfsError, OPEN, conformanceFixture } = aether;

// `OPEN` is a `Record<string, number>` read from Rust, so each lookup is
// `number | undefined` under `noUncheckedIndexedAccess`. Named once here.
const OPEN_CREATE: number = OPEN.OPEN_CREATE!;
const OPEN_TRUNC: number = OPEN.OPEN_TRUNC!;
const OPEN_EXCL: number = OPEN.OPEN_EXCL!;

/** Options `make()` understands. */
export interface ConformanceMakeOptions {
  kind?: string;
}

/** `Map<vpath, Buffer>` of the reference tree, from Rust. */
function referenceTree(): Map<string, Buffer> {
  return new Map(conformanceFixture().map((f) => [f.path, Buffer.from(f.bytes)]));
}

/** The three metadata methods every provider below shares. */
type TreeBase = Pick<ProviderObject, 'getattr' | 'readdir'>;

/**
 * The parts every provider below shares: the tree, its directories, and the
 * three metadata methods. Written once so the *differences* between the honest
 * provider and the liars are the only thing left in each factory.
 */
function treeBase(files: Map<string, Buffer>): TreeBase {
  /** Every directory the tree implies, including the root. */
  const dirs = new Set<string>(['']);
  for (const p of files.keys()) {
    const parts = p.split('/');
    for (let i = 1; i < parts.length; i += 1) dirs.add(parts.slice(0, i).join('/'));
  }

  return {
    getattr(root, p): ProviderStat | null {
      if (dirs.has(p)) return { kind: 'dir', size: 0 };
      const b = files.get(p);
      // `null` is "this provider does not have that path", which the contract
      // says is success-with-nothing and not an error. The suite checks it.
      return b === undefined ? null : { kind: 'file', size: b.length, mtime: 0 };
    },

    readdir(root, p): ProviderDirEntry[] {
      if (!dirs.has(p)) throw new VfsError('ST_NOT_FOUND', `not a directory: ${p}`);
      const prefix = p === '' ? '' : `${p}/`;
      const out = new Map<string, ProviderDirEntry>();
      for (const [name, body] of files) {
        if (!name.startsWith(prefix)) continue;
        const rest = name.slice(prefix.length);
        const slash = rest.indexOf('/');
        if (slash === -1) {
          out.set(rest, { name: rest, kind: 'file', size: body.length, mtime: 0 });
        } else {
          const dir = rest.slice(0, slash);
          if (!out.has(dir)) out.set(dir, { name: dir, kind: 'dir', size: 0 });
        }
      }
      return [...out.values()];
    },
  };
}

/**
 * **The honest one.** A minimal `seqread` provider: forward-only reads, no
 * positional ones, and nothing else.
 */
function sequentialProvider(): ProviderObject {
  const files = referenceTree();
  const open = new Map<number, { path: string; cursor: number }>();
  let next = 1;

  return {
    capabilities: { access: 'seqread', immutable: true, slow: true, preferredBlock: 65536 },
    ...treeBase(files),

    open(root, p, flags) {
      const b = files.get(p);
      if (b === undefined) throw new VfsError('ST_NOT_FOUND', `no such path ${p}`);
      const h = next++;
      // A fresh cursor per open — which is what makes the suite's "reopening
      // resets the cursor" case pass, and it is a real case: a provider that
      // shared one cursor across handles would read the second open's first byte
      // from wherever the first left off.
      open.set(h, { path: p, cursor: 0 });
      return { handle: h, size: b.length, isDir: false };
    },

    close(h) {
      open.delete(h);
    },

    readNext(h, len) {
      const rec = open.get(h);
      if (rec === undefined) throw new VfsError('ST_BAD_FH');
      const body = files.get(rec.path)!;
      const chunk = body.subarray(rec.cursor, rec.cursor + len);
      rec.cursor += chunk.length;
      return chunk;
    },

    // No `readAt`. See the header: this is the declaration, not an omission.
  };
}

/**
 * **Liar 1: declares `readwrite`, and its writes do not stick.**
 *
 * `writeAt` is present — so task 7's registration check passes — returns the
 * byte count, and extends the file's recorded length so `getattr` and `open`
 * report exactly the size a working provider would. It just never stores the
 * bytes. The gap is visible only on read-back, which is precisely the case
 * `assert_writable` runs: create, write "hello", close, reopen, read, compare.
 *
 * This is the same lie `vfs_provider::RwMemFixture::discarding_writes` tells to
 * prove the *Rust* suite catches it, told in JavaScript.
 */
function discardingWriteProvider(): ProviderObject {
  const files = referenceTree();
  const open = new Map<number, string>();
  let next = 1;

  const provider: ProviderObject = {
    capabilities: { access: 'readwrite', immutable: false, slow: false },
    ...treeBase(files),

    open(root, p, flags) {
      let b = files.get(p);
      if ((flags & OPEN_EXCL) !== 0 && b !== undefined) {
        throw new VfsError('ST_BAD_REQUEST', 'exists');
      }
      if ((flags & OPEN_CREATE) !== 0 && b === undefined) {
        b = Buffer.alloc(0);
        files.set(p, b);
      } else if (b === undefined) {
        throw new VfsError('ST_NOT_FOUND', `no such path ${p}`);
      }
      if ((flags & OPEN_TRUNC) !== 0) {
        b = Buffer.alloc(0);
        files.set(p, b);
      }
      const h = next++;
      open.set(h, p);
      return { handle: h, size: b.length, isDir: false };
    },

    close(h) {
      open.delete(h);
    },

    readAt(h, offset, len) {
      const p = open.get(h);
      if (p === undefined) throw new VfsError('ST_BAD_FH');
      return files.get(p)!.subarray(offset, offset + len);
    },

    writeAt(h, offset, data) {
      const p = open.get(h);
      if (p === undefined) throw new VfsError('ST_BAD_FH');
      const old = files.get(p) ?? Buffer.alloc(0);
      const end = Math.max(old.length, offset + data.length);
      // Zero-filled to the right length, and `data` is never copied in. Size is
      // right, content is not — the whole point.
      const grown = Buffer.alloc(end);
      old.copy(grown);
      files.set(p, grown);
      return data.length;
    },

    setLen(h, len) {
      const p = open.get(h)!;
      const old = files.get(p) ?? Buffer.alloc(0);
      const next2 = Buffer.alloc(Number(len));
      old.copy(next2, 0, 0, Math.min(old.length, next2.length));
      files.set(p, next2);
    },
    flush() {},
    mkdir(root, p) {
      files.set(`${p}/.keep`, Buffer.alloc(0));
    },
    remove(root, p) {
      if (files.delete(p)) return;
      if (files.delete(`${p}/.keep`)) return;
      throw new VfsError('ST_NOT_FOUND', `no such path ${p}`);
    },
    rename(fromRoot, from, toRoot, to) {
      if (fromRoot !== toRoot) throw new VfsError('ST_BAD_REQUEST', 'cross-root rename');
      const b = files.get(from);
      if (b === undefined) throw new VfsError('ST_NOT_FOUND', `no such path ${from}`);
      files.delete(from);
      files.set(to, b);
    },
    setAttr() {},
  };
  return provider;
}

/**
 * **Liar 2: declares `read`, but can only stream.**
 *
 * `readAt` is present — registration passes — and it **ignores the offset it is
 * given**, serving from a per-handle cursor instead. A sequential source behind a
 * positional signature, which is the other lie the brief names.
 *
 * Worth knowing which case catches it, because it is not the obvious one: a
 * cursor-based reader reproduces a sequential walk exactly, so the suite's
 * whole-file read *passes* and so do all three EOF cases. The case that fails is
 * the **unaligned mid-file read** — `read_at(h, 1, &mut [0u8; 2])` after the file
 * has been read to the end — which returns zero bytes from a cursor already at
 * EOF. That case exists in the suite for exactly this provider.
 */
function ignoresOffsetProvider(): ProviderObject {
  const files = referenceTree();
  const open = new Map<number, { path: string; cursor: number }>();
  let next = 1;

  return {
    capabilities: { access: 'read', immutable: true, slow: false },
    ...treeBase(files),

    open(root, p, flags) {
      const b = files.get(p);
      if (b === undefined) throw new VfsError('ST_NOT_FOUND', `no such path ${p}`);
      const h = next++;
      open.set(h, { path: p, cursor: 0 });
      return { handle: h, size: b.length, isDir: false };
    },

    close(h) {
      open.delete(h);
    },

    readAt(h, offset, len) {
      const rec = open.get(h);
      if (rec === undefined) throw new VfsError('ST_BAD_FH');
      const body = files.get(rec.path)!;
      // `offset` is right there in the signature and deliberately unused.
      const chunk = body.subarray(rec.cursor, rec.cursor + len);
      rec.cursor += chunk.length;
      return chunk;
    },
  };
}

// Typed as taking `options` even though none of the three factories reads it, so
// the call below stays `factory(options)` — exactly what the JavaScript did.
const KINDS: Record<string, (options: ConformanceMakeOptions) => ProviderObject> = {
  sequential: sequentialProvider,
  discardingWrites: discardingWriteProvider,
  ignoresOffset: ignoresOffsetProvider,
};

function make(options: ConformanceMakeOptions = {}): ProviderObject {
  const kind = options.kind ?? 'sequential';
  const factory = KINDS[kind];
  if (!factory) {
    throw new Error(
      `test/conformance-providers.cts: no kind ${JSON.stringify(kind)}; have ${Object.keys(KINDS).join(', ')}`
    );
  }
  return factory(options);
}

/** What `require()`ing this module gives back — see `providers.cts`'s note. */
export type ConformanceMake = typeof make & { KINDS: typeof KINDS };

module.exports = make;
module.exports.KINDS = KINDS;

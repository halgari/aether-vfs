// The provider `examples/js-provider.mts` mounts. It stands in for spec §8's
// `SteamCdn` sketch: content that lives somewhere slow and asynchronous, exposed
// as an ordinary provider.
//
// It is a **factory**, and a separate module, because `providerWorker({ module })`
// resolves a module path *inside the worker* — isolates share no JS objects, so a
// provider instance cannot be handed across one (spec §8c). Being a factory also
// means the object is constructed on the loop its methods will run on.
//
// `require` and not `import`, and an annotation rather than a cast: this file is
// loaded by **node** — inside a provider worker, and directly by
// `js-provider.mts` for its deadlock-guard step. Node's type stripping erases
// annotations but does not rewrite module syntax, so an `import` statement here
// would be a runtime `SyntaxError`. The annotation is what a cast is not: an
// explicit type annotation, which is what TypeScript requires before it will
// treat `assert.ok` as an assertion function (TS2775).

import type { ProviderDirEntry, ProviderObject } from '../index.mjs';

const path: typeof import('node:path') = require('path');

const { VfsError }: typeof import('../index.mjs') = require(path.join(__dirname, '..', 'index.mjs'));

/** Options `pretendCdn()` understands. */
export interface PretendCdnOptions {
  depot?: string;
  latencyMs?: number;
}

/** A "fetch" that takes a while, so `async` is doing real work here. */
const fetchLatency = (ms: number): Promise<void> => new Promise((r) => setTimeout(r, ms));

function pretendCdn({ depot = '489830', latencyMs = 5 }: PretendCdnOptions = {}): ProviderObject {
  const catalog = new Map<string, Buffer>([
    ['readme.txt', Buffer.from(`depot ${depot}: fetched over a pretend network\n`)],
    ['Skyrim.ini', Buffer.from('[Display]\nsTest=1\n')],
    ['data/big.bin', Buffer.alloc(4096, 0x5a)],
  ]);
  const open = new Map<number, string>();
  let nextHandle = 1;

  return {
    // `slow` says caching is *warranted*; `immutable` says it is *safe*. Only the
    // pair justifies persisting blocks across sessions — and §8c measured 64 KiB
    // as the best block size tested, against a 1 MiB default that is 60× worse.
    capabilities: { access: 'read', immutable: true, slow: true, preferredBlock: 65536 },

    getattr(root, p) {
      if (p === '' || p === 'data') return { kind: 'dir', size: 0 };
      const b = catalog.get(p);
      return b ? { kind: 'file', size: b.length, mtime: 0 } : null;
    },

    readdir(root, p) {
      const prefix = p === '' ? '' : `${p}/`;
      const out: ProviderDirEntry[] = [];
      const dirs = new Set<string>();
      for (const [name, b] of catalog) {
        if (!name.startsWith(prefix)) continue;
        const rest = name.slice(prefix.length);
        const slash = rest.indexOf('/');
        if (slash === -1) out.push({ name: rest, kind: 'file', size: b.length });
        else dirs.add(rest.slice(0, slash));
      }
      for (const d of dirs) out.push({ name: d, kind: 'dir', size: 0 });
      return out;
    },

    open(root, p, flags) {
      const b = catalog.get(p);
      if (!b) throw new VfsError('ST_NOT_FOUND', `depot ${depot} has no ${p}`);
      const h = nextHandle++;
      open.set(h, p);
      return { handle: h, size: b.length };
    },

    close(handle) {
      open.delete(handle);
    },

    // `async`, and the director thread parks until it settles — spec §8b rule 2.
    async readAt(handle, offset, length) {
      const p = open.get(handle);
      if (p === undefined) throw new VfsError('ST_BAD_FH');
      await fetchLatency(latencyMs);
      return catalog.get(p)!.subarray(offset, offset + length);
    },
  };
}

/** What `require()`ing this module gives back. See `test/providers.cts`'s note. */
export type PretendCdn = typeof pretendCdn;

module.exports = pretendCdn;

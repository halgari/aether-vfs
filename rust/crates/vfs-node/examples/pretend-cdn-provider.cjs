'use strict';

// The provider `examples/js-provider.cjs` mounts. It stands in for spec §8's
// `SteamCdn` sketch: content that lives somewhere slow and asynchronous, exposed
// as an ordinary provider.
//
// It is a **factory**, and a separate module, because `providerWorker({ module })`
// resolves a module path *inside the worker* — isolates share no JS objects, so a
// provider instance cannot be handed across one (spec §8c). Being a factory also
// means the object is constructed on the loop its methods will run on.

const path = require('path');

const { VfsError } = require(path.join(__dirname, '..', 'index.cjs'));

/** A "fetch" that takes a while, so `async` is doing real work here. */
const fetchLatency = (ms) => new Promise((r) => setTimeout(r, ms));

module.exports = function pretendCdn({ depot = '489830', latencyMs = 5 } = {}) {
  const catalog = new Map([
    ['readme.txt', Buffer.from(`depot ${depot}: fetched over a pretend network\n`)],
    ['Skyrim.ini', Buffer.from('[Display]\nsTest=1\n')],
    ['data/big.bin', Buffer.alloc(4096, 0x5a)],
  ]);
  const open = new Map();
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
      const out = [];
      const dirs = new Set();
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
      return catalog.get(p).subarray(offset, offset + length);
    },
  };
};

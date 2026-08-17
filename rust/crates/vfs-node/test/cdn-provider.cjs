'use strict';

// The one thing a host writes.
//
// Spec §6's claim is that a host language contributes **novel data sources** and
// composes everything else out of Rust primitives, and spec §8's illustration of
// that claim is a Steam CDN client:
//
//   class SteamCdn(vfs.Provider):
//       caps = vfs.Capabilities(access=vfs.Access.SEQ_READ, immutable=True,
//                               slow=True, preferred_block=1 << 20)
//       def read_next(self, handle, n) -> bytes: ...
//
// This is that provider, in JavaScript, with the network replaced by a byte
// generator so the test is deterministic. Everything about it that matters is
// the *shape*: it is **forward-only** (`readNext`, no `readAt`), it declares
// `slow` and `immutable`, and it names a preferred block size. Which means it
// cannot be mounted at all until `seekable` promotes it, and it should not be
// read directly until `cached` is in front of it — the two primitives task 8
// exists to expose, and the two spec §6 flag-table rules that fire when they are
// missing.
//
// `fetches` counts `readNext` calls, so a test can prove the cache above it is
// actually absorbing reads rather than merely being present.

const path = require('path');

const aether = require(path.join(__dirname, '..', 'index.cjs'));
const { VfsError } = aether;

/** The depot's contents. Recognisable bytes, so a mis-seek is visible. */
function depotFiles(depot) {
  return new Map([
    ['vanilla/data.bin', Buffer.from(Array.from({ length: 4096 }, (_, i) => i % 251))],
    ['vanilla/readme.txt', Buffer.from(`depot ${depot}: fetched over a pretend network`)],
    // The path a mod layer above this one overrides, so `layered` has something
    // to win at.
    ['shared.txt', Buffer.from('from-the-base-game')],
  ]);
}

module.exports = function makeCdn({ depot = '489830', preferredBlock = 65536 } = {}) {
  const files = depotFiles(depot);
  /** handle → { path, cursor } — a forward-only cursor, which is the whole point. */
  const open = new Map();
  let nextHandle = 1;
  const counters = { opens: 0, fetches: 0, bytesFetched: 0 };

  return {
    capabilities: {
      access: 'seqread',
      immutable: true,
      slow: true,
      preferredBlock,
    },

    getattr(root, p) {
      if (p === '' || p === 'vanilla') return { kind: 'dir', size: 0 };
      const b = files.get(p);
      return b === undefined ? null : { kind: 'file', size: b.length, mtime: 0 };
    },

    readdir(root, p) {
      const prefix = p === '' ? '' : `${p}/`;
      const seen = new Map();
      for (const [name, body] of files) {
        if (!name.startsWith(prefix)) continue;
        const rest = name.slice(prefix.length);
        const slash = rest.indexOf('/');
        if (slash === -1) {
          seen.set(rest, { name: rest, kind: 'file', size: body.length });
        } else {
          const dir = rest.slice(0, slash);
          if (!seen.has(dir)) seen.set(dir, { name: dir, kind: 'dir', size: 0 });
        }
      }
      return [...seen.values()];
    },

    open(root, p, flags) {
      const b = files.get(p);
      if (b === undefined) throw new VfsError('ST_NOT_FOUND', `depot has no ${JSON.stringify(p)}`);
      const h = nextHandle++;
      open.set(h, { path: p, cursor: 0 });
      counters.opens += 1;
      return { handle: h, size: b.length, isDir: false };
    },

    close(h) {
      open.delete(h);
    },

    // Forward-only, and there is deliberately **no `readAt`**. A positional read
    // through this provider is served by `seekable`, which tracks the cursor and
    // reopens on a backward seek; if it were served here the test would be
    // proving nothing about the primitive.
    readNext(h, len) {
      const rec = open.get(h);
      if (rec === undefined) throw new VfsError('ST_BAD_FH');
      const body = files.get(rec.path);
      const chunk = body.subarray(rec.cursor, rec.cursor + len);
      rec.cursor += chunk.length;
      counters.fetches += 1;
      counters.bytesFetched += chunk.length;
      return chunk;
    },

    // Local to the worker's isolate, so the main thread cannot read it — which
    // is the same constraint that makes a provider cross as an integer. A test
    // on the main thread counts bridge crossings with `provider.stats().calls`
    // instead; this is here for a host debugging inside the worker.
    $counters: counters,
  };
};

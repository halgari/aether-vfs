// **The only thing a host writes.** Spec §8's example is one class and eight
// combinators, and §6's claim is that the class is the host's whole
// contribution:
//
//   class SteamCdn(vfs.Provider):
//       caps = vfs.Capabilities(access=vfs.Access.SEQ_READ, immutable=True,
//                               slow=True, preferred_block=1 << 20)
//       def read_next(self, handle, n) -> bytes: ...
//
// This is that class, in JavaScript, with the network replaced by a byte
// catalog so an example is deterministic. What is *not* replaced is the shape,
// and every part of the shape has a consequence downstream:
//
//  - **forward-only** (`readNext`, no `readAt`): cannot be mounted at all until
//    `seekable` promotes it, and `session.mount` says so with the fix in the
//    message;
//  - **`slow`**: `mount` warns unless something above it caches;
//  - **`immutable`**: what makes the on-disk block tier safe to keep;
//  - **`preferredBlock: 64 KiB`**: overrides `cached`'s 1 MiB default, which
//    §8c measured at 24 MiB/s against 1094 for 64 KiB.
//
// A factory, and a separate module, because `providerWorker({ module })`
// resolves the path *inside the worker*: isolates share no JS objects, so what
// crosses is a process-global integer (§8c). Being a factory also means the
// object is built on the loop its methods will run on.
//
// ESM, as of task 3: node loads this file inside a provider worker via
// `await import(pathToFileURL(data.module).href)`, so it is a real `import`.
//
// ## Why the lookups fold, and why that is a finding rather than a detail
//
// The shim folds every vpath component with `vfs_core::fold` before it crosses
// the ring, so an injected child's read of `Data\Skyrim.ini` arrives here as
// `data/skyrim.ini`. A host-side `session.readFile(...)` and `launch`'s own
// image resolution do **not** fold — they hand the graph the path as written.
// So a provider keyed on the exact string it is given answers one of those two
// callers and not the other, with no error on the path that misses.
//
// Spec §6's answer is a `casefold(p)` combinator, and it does not exist in Rust
// (§6b). Until it does, **every host-authored provider must fold its own
// lookups**, which is what `find()` below does. That is a real cost of the
// missing primitive: it is not a convenience, it is correctness, and it is
// re-implemented per provider and per binding until the combinator exists.
// `memory()` is the case a host *cannot* fix this way, because it is a Rust
// primitive — see `spec-8-example.mts`, step 8.

import fs from 'node:fs';

import type { ProviderDirEntry, ProviderObject } from '../index.mjs';
import { VfsError } from '../index.mjs';

/** Options `steamCdn()` understands. */
export interface SteamCdnOptions {
  depot?: string;
  latencyMs?: number;
  preferredBlock?: number;
  /** The bytes the depot serves as `SkyrimSE.exe`. The stand-in for stage 5. */
  exeSource?: string;
}

interface CdnCounters {
  opens: number;
  fetches: number;
  bytesFetched: number;
}

/** The provider this module builds, with its worker-local counters attached. */
export interface SteamCdnProvider extends ProviderObject {
  $counters: CdnCounters;
}

/** A "fetch" that takes a while, so `slow` and `async` are not decorative. */
const fetchLatency = (ms: number): Promise<void> | null =>
  ms > 0 ? new Promise((r) => setTimeout(r, ms)) : null;

function steamCdn({
  depot = '489830',
  latencyMs = 1,
  preferredBlock = 65536,
  exeSource,
}: SteamCdnOptions = {}): SteamCdnProvider {
  /** vpath → bytes, exactly as the depot holds them (mixed case included). */
  const catalog = new Map<string, Buffer>([
    // The game executable. `launch('SkyrimSE.exe')` resolves this through the
    // graph, stages it out with its PE import closure, and runs that copy —
    // so the image the process runs comes out of a JavaScript function.
    ['SkyrimSE.exe', fs.readFileSync(exeSource!)],
    // The vanilla INI a mod layer above this one overrides, so `layered` has
    // something to actually win at.
    ['Data/Skyrim.ini', Buffer.from('[General]\nuGridsToLoad=5\n; from the depot\n')],
    // Served by nothing else, so a read of this proves the depot itself
    // answered rather than the mod directory beside it.
    ['Data/SkyrimPrefs.ini', Buffer.from('[Display]\niSize H=1440\n; from the depot\n')],
    // 4 KiB of `i % 251`: recognisable bytes at known offsets, so a
    // mis-counted skip inside `seekable` is visible rather than plausible.
    ['Data/textures.bsa', Buffer.from(Array.from({ length: 4096 }, (_, i) => i % 251))],
  ]);

  /** Folded vpath → the key the catalog actually holds. See the header. */
  const folded = new Map<string, string>([...catalog.keys()].map((k) => [k.toLowerCase(), k]));
  const dirs = new Set<string>(['', 'data']);

  /** handle → { key, cursor } — one forward-only cursor, which is the point. */
  const open = new Map<number, { bytes: Buffer; cursor: number }>();
  let nextHandle = 1;
  const counters: CdnCounters = { opens: 0, fetches: 0, bytesFetched: 0 };

  const find = (p: string): Buffer | undefined => {
    const key = folded.get(p.toLowerCase());
    return key === undefined ? undefined : catalog.get(key);
  };

  return {
    capabilities: { access: 'seqread', immutable: true, slow: true, preferredBlock },

    getattr(root, p) {
      if (dirs.has(p.toLowerCase())) return { kind: 'dir', size: 0 };
      const b = find(p);
      return b === undefined ? null : { kind: 'file', size: b.length, mtime: 0 };
    },

    readdir(root, p) {
      const prefix = p === '' ? '' : `${p.toLowerCase()}/`;
      const seen = new Map<string, ProviderDirEntry>();
      for (const [key, body] of catalog) {
        const lower = key.toLowerCase();
        if (!lower.startsWith(prefix)) continue;
        const rest = key.slice(prefix.length);
        const slash = rest.indexOf('/');
        if (slash === -1) seen.set(rest, { name: rest, kind: 'file', size: body.length, mtime: 0 });
        else {
          const dir = rest.slice(0, slash);
          if (!seen.has(dir)) seen.set(dir, { name: dir, kind: 'dir', size: 0, mtime: 0 });
        }
      }
      return [...seen.values()];
    },

    open(root, p, flags) {
      const b = find(p);
      if (b === undefined) {
        throw new VfsError('ST_NOT_FOUND', `depot ${depot} has no ${JSON.stringify(p)}`);
      }
      const h = nextHandle++;
      open.set(h, { bytes: b, cursor: 0 });
      counters.opens += 1;
      return { handle: h, size: b.length, isDir: false };
    },

    close(h) {
      open.delete(h);
    },

    // Forward-only, and there is deliberately **no `readAt`**: a positional read
    // through this provider is served by `seekable`'s cursor, and if it were
    // served here the example would prove nothing about the primitive.
    //
    // `async`, so the director thread parks until the promise settles — spec
    // §8b rule 2, and the reason `slow` is a declaration rather than a comment.
    async readNext(h, len) {
      const rec = open.get(h);
      if (rec === undefined) throw new VfsError('ST_BAD_FH');
      await fetchLatency(latencyMs);
      const chunk = rec.bytes.subarray(rec.cursor, rec.cursor + len);
      rec.cursor += chunk.length;
      counters.fetches += 1;
      counters.bytesFetched += chunk.length;
      return chunk;
    },

    /** Worker-local; the main thread counts crossings with `provider.stats()`. */
    $counters: counters,
  };
}

export default steamCdn;

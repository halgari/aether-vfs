// §1 — what the JavaScript layer itself costs.
//
// `index.mjs` is a wrapper around `native.mjs`, and the wrapper is not free: it
// coerces a `Provider` (or a `ProviderWorker`) down to the integer handle Rust
// wants, and it re-exports the addon with `export * from './native.mjs'`, which
// `tsc` emits as accessors. Both are load-bearing — `scripts/check-types.mts`
// leg 4 exists because losing a wrapper would send an object where Rust wants a
// `u32` — so what they cost is worth knowing.
//
// Nothing here touches disk or a provider graph. This is the floor: if these
// numbers are large, everything above them is too.

import * as path from 'node:path';
import { pathToFileURL } from 'node:url';

import {
  PKG_DIR,
  assertAtMostNs,
  assertAtMostRatio,
  assertExact,
  benchRequire,
  fmtNs,
  heading,
  measure,
  sink,
  table,
} from '../harness.mts';

type Vfs = typeof import('../../index.mjs');
type Native = typeof import('../../native.mjs');

export async function run(): Promise<void> {
  const vfs = (await import(pathToFileURL(path.join(PKG_DIR, 'index.mjs')).href)) as Vfs;
  const native = benchRequire(path.join(PKG_DIR, 'native.mjs')) as Native;

  heading('1. the JavaScript layer');

  // One provider, reused. These calls read from it rather than creating anything,
  // so the loop does not grow the process-global registry.
  const mem = vfs.memory({ 'a.txt': 'x' });

  measure('property read: vfs.disk        (forwarded, getter)', () => sink(vfs.disk));
  measure('property read: vfs.memory      (shadowed, own value)', () => sink(vfs.memory));
  measure('addon call:    version()', () => sink(vfs.version()));
  measure('addon call:    statusName(2)', () => sink(vfs.statusName(2)));
  measure('addon getter:  provider.handle', () => sink(mem.handle));
  measure('addon getter:  provider.kind', () => sink(mem.kind));
  measure('addon call:    provider.capabilities()', () => sink(mem.capabilities().access));
  measure('addon call:    provider.jsLeaves()', () => sink(mem.jsLeaves().length));
  const bare = measure('addon call:    version() [for the ceiling]', () => sink(vfs.version()));
  table();

  assertAtMostNs(
    'a bare addon call stays well under a microsecond',
    bare.nsPerOp,
    2_000,
    'the floor for everything else; a regression here is an N-API or binding problem, not a provider one'
  );

  // The wrapper's coercion, isolated. Both sides construct one provider per
  // iteration, so the difference is the `ProviderLike -> handle` step and nothing
  // else. Iterations are **pinned rather than calibrated**, because each call adds
  // an entry to a process-global registry and a calibrated loop would decide to
  // make millions of them.
  heading('1b. the wrapper against the primitive it wraps');
  const opts = { iterations: 2000, samples: 3, warmup: 20 };
  const viaWrapper = measure('readonly(provider)  — index.mjs wrapper', () => sink(vfs.readonly(mem).handle), opts);
  const viaNative = measure(
    'readonly(handle)    — native.mjs primitive',
    () => sink(native.readonly(mem.handle).handle),
    opts
  );
  table();
  process.stdout.write(
    `  coercion cost: ${fmtNs(Math.max(viaWrapper.nsPerOp - viaNative.nsPerOp, 0))} per call\n`
  );

  assertExact(
    'index.mjs still shadows the primitive',
    vfs.readonly === native.readonly ? 1 : 0,
    0,
    'if the shadow were lost, a host passing a Provider would reach Rust with an object — and this ' +
      'benchmark would silently be measuring the same function twice'
  );
  assertAtMostRatio(
    'the wrapper does not dominate the primitive it wraps',
    viaWrapper.nsPerOp / viaNative.nsPerOp,
    2.5,
    'accepting a Provider instead of an integer is a convenience; it must not be an expensive one'
  );
}

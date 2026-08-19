// A provider that registers cleanly and *then* dies.
//
// The fixture for the second half of the teardown defect: `providerWorker()`
// resolves, so the promise that would have rejected is already gone, and only
// then does the worker throw. Before the fix the `once('error', fail)` handler
// returned early in exactly this case and nothing reported the death anywhere —
// no rejection, no unhandled `'error'` event, nothing on stderr.
//
// ESM, as of task 3: this module is loaded by **node** (inside a provider
// worker) via `await import(pathToFileURL(data.module).href)`, so it is a real
// `import` — see the header of `providers.mts`.

import type { ProviderObject } from '../index.mjs';

import * as aether from '../index.mjs';

/** How long after registering to die. Short, but after the `{ ok: true }` reply. */
interface LateThrowOptions {
  afterMs?: number;
}

function make({ afterMs = 100 }: LateThrowOptions = {}): ProviderObject {
  // Thrown from a timer callback rather than from `make`, so registration
  // succeeds first: a throw here would be reported as `{ ok: false }` and the
  // `providerWorker()` promise would reject, which is the case that already
  // worked and not the one under test.
  setTimeout(() => {
    throw new Error('late-throw — a provider worker dying after it registered');
  }, afterMs);

  return {
    capabilities: { access: 'read' },
    getattr: () => null,
    readdir: () => [],
    open: () => {
      throw new aether.VfsError('ST_NOT_FOUND');
    },
    close: () => {},
    readAt: () => Buffer.alloc(0),
  };
}

export default make;

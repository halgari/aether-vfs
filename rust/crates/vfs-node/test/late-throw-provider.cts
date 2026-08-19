// A provider that registers cleanly and *then* dies.
//
// The fixture for the second half of the teardown defect: `providerWorker()`
// resolves, so the promise that would have rejected is already gone, and only
// then does the worker throw. Before the fix the `once('error', fail)` handler
// returned early in exactly this case and nothing reported the death anywhere —
// no rejection, no unhandled `'error'` event, nothing on stderr.
//
// `require` and a type annotation rather than `import`, for the reason recorded in
// the header of `providers.cts`: this module is loaded by **node** (inside a
// provider worker), and node's type stripping erases annotations without
// rewriting module syntax, so an `import` statement in a `.cts` file is a runtime
// `SyntaxError`.

import type { ProviderObject } from '../index.mjs';

const path: typeof import('node:path') = require('path');

const aether: typeof import('../index.mjs') = require(path.join(__dirname, '..', 'index.mjs'));

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

/** What `require()`ing this module gives back — see `providers.cts`. */
export type MakeLateThrow = typeof make;

module.exports = make;

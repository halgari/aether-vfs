// Takes ~300 ms to evaluate at module scope, so a `release` message posted
// immediately after the worker starts reliably lands *during* the
// `await import(...)` in `provider-host.mts` rather than after it.
//
// This is the fixture for the race the release handler exists to close: if
// `provider-host.mts` did not record an early release and act on it once the
// load completes, this module registering successfully would leave the
// worker's threadsafe function held forever, and the worker would never exit.
// 300 ms is long enough to land mid-load reliably on this machine and short
// enough that the one test using it does not slow the suite down.

import { setTimeout as delay } from 'node:timers/promises';

import type { ProviderObject } from '../../index.mjs';
import { VfsError } from '../../index.mjs';

await delay(300);

function make(): ProviderObject {
  return {
    capabilities: { access: 'read' },
    getattr: (_root, p) => (p === '' ? { kind: 'dir', size: 0 } : null),
    readdir: () => [],
    open: () => {
      throw new VfsError('ST_NOT_FOUND');
    },
    close: () => {},
    readAt: () => Buffer.alloc(0),
  };
}

export default make;

// The success case for `spec.export`: the module exports its factory under a
// name other than `default` or `provider`, so `providerWorker()` only finds it
// when told to look — `data.export ? mod[data.export] : ...` in
// `provider-host.mts`. A default export would pass even with that branch
// broken; this fixture is what actually exercises it.

import type { ProviderObject } from '../../index.mjs';
import { VfsError } from '../../index.mjs';

function makeProvider(): ProviderObject {
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

export { makeProvider };

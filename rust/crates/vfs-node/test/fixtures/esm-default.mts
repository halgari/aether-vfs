// The success case for a `default`-exported factory: `providerWorker()` with no
// `spec.export` picks `mod.provider ?? mod.default`, so this is what proves that
// half of the lookup rather than the `spec.export` half — see `esm-named.mts` for
// the other one.
//
// ESM, per the migration: loaded by **node**, inside a provider worker, via
// `await import(pathToFileURL(data.module).href)` — a real `import`, no
// CommonJS shape anywhere in this file.

import type { ProviderObject } from '../../index.mjs';
import { VfsError } from '../../index.mjs';

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

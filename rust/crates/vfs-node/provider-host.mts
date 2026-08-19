// The script `providerWorker()` runs. It exists because of the constraint spec
// §8c records: **a provider instance cannot cross an isolate boundary**, so
// registration is a module path resolved *inside* the worker rather than an
// object handed across. What comes back to the caller is the process-global
// integer handle, which `session.mount()` accepts from any thread.
//
// Being a separate file is also what makes the worker's loop the servicing loop:
// `registerProvider` binds the threadsafe function to whichever isolate calls
// it, and that is this one.
//
// Compiled to `provider-host.mjs`, which is the path `index.mts` hands to
// `new Worker(...)` and the name in `package.json`'s `files`.

import { pathToFileURL } from 'node:url';
import { parentPort, workerData } from 'node:worker_threads';

// `./index.mjs` and not the package name: this file *is* in the package, and
// resolving by name from inside it would depend on a node_modules link that need
// not exist. Loading through index.mjs (rather than the .node directly) is what
// records the package directory for DLL resolution.
import * as aether from './index.mjs';
import type { ProviderObject } from './index.mjs';
import type { ProviderOptions } from './native.mjs';

/** What `providerWorker()` puts in `workerData`. */
interface HostData {
  module: string;
  export: string | null;
  options: unknown;
  providerOptions: ProviderOptions;
}

/** A provider, or a factory that builds one on this loop. */
type Picked = ProviderObject | ((options: unknown) => ProviderObject);

// `parentPort` is `null` only in the main thread, and this file only ever runs as
// a worker entry point. Asserting it once here is what keeps every use below
// free of a null check that could never fire.
if (parentPort === null) {
  throw new Error(
    'aethervfs: provider-host.mjs is a worker entry point and was loaded on the ' +
      'main thread. Use providerWorker({ module }) instead of requiring it.'
  );
}
const port = parentPort;
const data = workerData as HostData;

let handle: number | null = null;
let releaseRequested = false;

// Registered *before* the load below, and not after it: once loading is
// `await`ed, a `release` arriving mid-load has to hit a listener. The old
// ordering — register after a synchronous load — was safe only because there
// was no `await` for a message to arrive during. `handle === null` here no
// longer means "nothing to do"; it means "record it, the loader releases on
// completion" (see `releaseRequested` below), because dropping it would leave
// this worker's threadsafe function held forever with nothing to say why.
port.on('message', (msg: unknown) => {
  const m = msg as { type?: unknown } | null;
  if (!m || m.type !== 'release') return;
  if (handle === null) {
    // The load has not finished. Record it; the loader releases on completion.
    releaseRequested = true;
    return;
  }
  // Releasing the threadsafe function is what lets this loop drain and the
  // worker exit; while it is held, the loop stays alive on purpose, because that
  // is what keeps the provider available for the session's lifetime.
  aether.releaseProvider(handle);
  port.close();
});

try {
  const mod = (await import(pathToFileURL(data.module).href)) as Record<string, unknown>;

  // Two accepted shapes: the export named by `spec.export`, else `provider`,
  // else `default`. A function is called with `options` and returns the
  // provider — a factory, so it is constructed on this loop, where its
  // methods will run.
  const picked = (data.export ? mod[data.export] : (mod.provider ?? mod.default)) as
    | Picked
    | undefined
    | null;

  if (picked === undefined || picked === null) {
    throw new Error(
      data.export
        ? `module has no export named ${JSON.stringify(data.export)}; it exports: ${Object.keys(mod).join(', ') || '(nothing)'}`
        : `module exported nothing usable as a provider or provider factory; it exports: ${Object.keys(mod).join(', ') || '(nothing)'}`
    );
  }

  const obj = typeof picked === 'function' ? picked(data.options) : picked;
  const provider = aether.registerProvider(obj, data.providerOptions);
  handle = provider.handle;
  port.postMessage({ ok: true, handle });
  if (releaseRequested) {
    aether.releaseProvider(handle);
    port.close();
  }
} catch (e) {
  port.postMessage({
    ok: false,
    message: e instanceof Error && e.message ? String(e.message) : String(e),
    stack: e instanceof Error && e.stack ? String(e.stack) : '',
  });
  port.close();
}

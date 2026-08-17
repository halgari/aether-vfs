'use strict';

// The script `providerWorker()` runs. It exists because of the constraint spec
// §8c records: **a provider instance cannot cross an isolate boundary**, so
// registration is a module path resolved *inside* the worker rather than an
// object handed across. What comes back to the caller is the process-global
// integer handle, which `session.mount()` accepts from any thread.
//
// Being a separate file is also what makes the worker's loop the servicing loop:
// `registerProvider` binds the threadsafe function to whichever isolate calls
// it, and that is this one.

const { parentPort, workerData } = require('worker_threads');

// `./index.cjs` and not the package name: this file *is* in the package, and
// resolving by name from inside it would depend on a node_modules link that need
// not exist. Loading through index.cjs (rather than the .node directly) is what
// records the package directory for DLL resolution.
const aether = require('./index.cjs');

let handle = null;

try {
  const mod = require(workerData.module);
  // Three accepted shapes, in order: a named export, a default/`provider`
  // export, or the module itself. A function is called with `options` and is
  // expected to return the provider — a factory, so the provider is constructed
  // on this loop, where its methods will run.
  const picked = workerData.export
    ? mod[workerData.export]
    : (mod.provider ?? mod.default ?? mod);
  if (picked === undefined || picked === null) {
    throw new Error(
      workerData.export
        ? `module has no export named ${JSON.stringify(workerData.export)}`
        : 'module exported nothing usable as a provider or provider factory'
    );
  }
  const obj = typeof picked === 'function' ? picked(workerData.options) : picked;
  const provider = aether.registerProvider(obj, workerData.providerOptions);
  handle = provider.handle;
  parentPort.postMessage({ ok: true, handle });
} catch (e) {
  parentPort.postMessage({
    ok: false,
    message: e && e.message ? String(e.message) : String(e),
    stack: e && e.stack ? String(e.stack) : '',
  });
  parentPort.close();
}

parentPort.on('message', (msg) => {
  if (!msg || msg.type !== 'release' || handle === null) return;
  // Releasing the threadsafe function is what lets this loop drain and the
  // worker exit; while it is held, the loop stays alive on purpose, because that
  // is what keeps the provider available for the session's lifetime.
  aether.releaseProvider(handle);
  parentPort.close();
});

// A provider hosted in its own `worker_threads` Worker: its own V8 isolate,
// its own event loop. Registration happens *here*, which is precisely the API
// consequence the addendum flags — the main script cannot hand an instance over
// an isolate boundary, so the provider has to be loaded inside the worker.
'use strict';

const { parentPort, workerData } = require('worker_threads');
const native = require(workerData.addon);
const { makeHandler, counter } = require('./provider.cjs');

const loads = native.noteIsolateLoad();
const bridgeId = native.registerProvider(makeHandler(native, workerData.mode));

parentPort.postMessage({
  ready: true,
  bridgeId,
  // Observed from inside the worker: if Rust statics were per-isolate this
  // would be 1 no matter how many bridges the process has registered.
  bridgeCountSeenInWorker: native.bridgeCount(),
  isolateLoadsSeenInWorker: loads,
});

parentPort.on('message', (m) => {
  if (m && m.cmd === 'probeSelf') {
    // Break the rule from inside the worker: a blocking provider call issued on
    // the very loop that services it.
    const r = native.probeBlockingRead(m.bridge, m.timeoutMs, m.len);
    parentPort.postMessage({ probeSelf: r });
  } else if (m && m.cmd === 'probeOther') {
    // Blocking call from this worker's loop into a *different* worker's loop.
    const r = native.probeBlockingRead(m.bridge, m.timeoutMs, m.len);
    parentPort.postMessage({ probeOther: r });
  } else if (m && m.cmd === 'pinSab') {
    // Same SharedArrayBuffer, a different isolate. If the address matches the
    // one the main thread pinned, one region can serve both.
    const id = native.pinSharedBuffer(new Uint8Array(m.sab));
    parentPort.postMessage({ pinSab: { id, pointer: native.pinnedPointer(id) } });
  } else if (m && m.cmd === 'writeSab') {
    new Uint8Array(m.sab).fill(m.value);
    parentPort.postMessage({ writeSab: true });
  } else if (m && m.cmd === 'stats') {
    parentPort.postMessage({ stats: { ...counter, threadId: require('worker_threads').threadId } });
  } else if (m && m.cmd === 'exit') {
    process.exit(0);
  }
});

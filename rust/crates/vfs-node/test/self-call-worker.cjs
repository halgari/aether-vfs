'use strict';

// The half of the deadlock guard that spec §8b's *first* wording got wrong.
//
// The original rule forbade provider calls from "the host's main thread". Task 5
// measured that `worker A → worker A's own loop` deadlocks exactly as hard as
// `main → main-loop` — so a host that read the rule literally, moved its
// provider into a worker and then called synchronously into Rust from that
// worker would hang, believing itself safe.
//
// This worker does precisely that: registers a provider on its own loop, mounts
// it in its own session, and drives a read from the same loop. Nothing about it
// is the main thread. The guard must still refuse it.

const { parentPort, workerData } = require('worker_threads');
const path = require('path');

const aether = require(path.join(__dirname, '..', 'index.cjs'));
const make = require(path.join(__dirname, 'providers.cjs'));

const result = { threw: false, message: null, elapsedMs: null, stats: null, ownerThread: null };

try {
  const provider = aether.registerProvider(make({ kind: 'bytes' }));
  const session = new aether.Session('worker-self-call');
  session.addRoot(0, 'game', workerData.gameRoot);
  session.mount(0, provider);

  result.ownerThread = provider.stats().ownerThread;
  const t0 = Date.now();
  try {
    session.readFile('js-served.txt');
  } catch (e) {
    result.threw = true;
    result.message = String(e.message ?? e);
  }
  result.elapsedMs = Date.now() - t0;
  result.stats = provider.stats();

  session.close();
  aether.releaseProvider(provider.handle);
} catch (e) {
  result.setupError = String(e.stack ?? e);
}

parentPort.postMessage(result);
parentPort.close();

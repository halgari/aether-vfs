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
//
// ## What has to keep working across the vitest migration
//
// The guard compares the calling thread against the **threadsafe function's
// owning thread**, captured where `registerProvider` runs — which is this file,
// on this worker's loop. So the thing that makes this test mean anything is that
// `ownerThread` is genuinely *this* worker and not the process's main loop, and
// `ownerThread` is reported back to the test alongside the refusal so the test
// can check that the message names the same thread. Under `pool: 'forks'` this
// worker is a real `worker_threads` worker inside vitest's per-file child
// process, exactly as it was under `node --test`; under `pool: 'threads'` it
// would not be, which is why `vitest.config.ts` pins the pool.
//
// ESM, as of task 3: node loads this file as a worker entry point
// (`new Worker('…/self-call-worker.mts')`, which node's type stripping accepts
// the same way it did for `.cts`), so this is a real `import` throughout — no
// `require` left anywhere in the file.

import { parentPort, workerData } from 'node:worker_threads';

import type { ProviderStats } from '../index.mjs';
import * as aether from '../index.mjs';
import make from './providers.mts';

/** What this worker posts back. The test file imports the type. */
export interface SelfCallResult {
  threw: boolean;
  message: string | null;
  elapsedMs: number | null;
  stats: ProviderStats | null;
  /** The thread the provider's threadsafe function is bound to — this worker. */
  ownerThread: string | null;
  setupError?: string;
}

const port = parentPort!;
const data = workerData as { gameRoot: string };

const result: SelfCallResult = {
  threw: false,
  message: null,
  elapsedMs: null,
  stats: null,
  ownerThread: null,
};

try {
  const provider = aether.registerProvider(make({ kind: 'bytes' }));
  const session = new aether.Session('worker-self-call');
  session.addRoot(0, 'game', data.gameRoot);
  session.mount(0, provider);

  result.ownerThread = provider.stats()!.ownerThread;
  const t0 = Date.now();
  try {
    session.readFile('js-served.txt');
  } catch (e) {
    result.threw = true;
    result.message = String((e as Error).message ?? e);
  }
  result.elapsedMs = Date.now() - t0;
  result.stats = provider.stats();

  session.close();
  aether.releaseProvider(provider.handle);
} catch (e) {
  result.setupError = String((e as Error).stack ?? e);
}

port.postMessage(result);
port.close();

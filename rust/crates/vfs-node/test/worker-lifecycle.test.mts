// **`ProviderWorker.close()` must settle whatever state the worker is in.**
//
// `close()` is the teardown that `await using` invokes, and it is the one this
// package documents as mandatory: a live threadsafe function holds a ref on the
// loop that services it, so a provider that is never released is a thread that
// never exits. A `close()` that *hangs* is therefore strictly worse than one that
// throws — it is indistinguishable from the leak it exists to prevent, and it
// takes the host's teardown down with it.
//
// The defect these tests pin: `close()` subscribed to `once('exit')` at the moment
// it was called. Node emits `'exit'` exactly once, so a worker that had already
// exited — terminated, crashed, or released by anything other than this method —
// left that promise pending forever. Measured before the fix: `terminate()` then
// `close()` did not settle in 8 s.
//
// ## Why each of these is a separate case
//
//   1. **after a normal exit** — the worker was released by someone else, so the
//      `'exit'` this object is waiting for happened before it started waiting;
//   2. **after `terminate()`** — the same, arrived at the way a host actually
//      arrives at it (a timeout, a failed session, a `SIGINT` handler);
//   3. **twice** — the second call must not resolve while the first is still
//      bringing the worker down, which a bare `if (this._closed) return` flag did;
//   4. **never started cleanly** — a `ProviderWorker` wrapping a worker whose
//      `'exit'` event is already spent, which no listener registered afterwards
//      can ever observe. This is the case a constructor-registered listener alone
//      does not cover, and it is why `close()` also asks the worker whether its
//      thread is gone.
//
// ## The timeout is the assertion
//
// Every `close()` here goes through `withTimeout`, which **rejects**. The failure
// mode under test is a hang, and a test that hangs reports nothing and blocks the
// suite behind it — vitest's own 120 s per-test budget would turn one regression
// into a two-minute stall per case. Five seconds is ~1500x the observed cost of a
// clean close on this machine and well inside the 2 s backstop, so it separates
// "hung" from "slow" without being flaky.

import { test } from 'vitest';
import assert from 'node:assert';
import path from 'node:path';

import type { ProviderWorker } from '../index.mjs';
import * as vfs from '../index.mjs';

const FIXTURE: string = path.join(import.meta.dirname, 'providers.cts');
const LATE_THROW: string = path.join(import.meta.dirname, 'late-throw-provider.cts');

/** A provider on its own worker loop. Nothing about the provider is under test. */
function worker(): Promise<ProviderWorker> {
  return vfs.providerWorker({ module: FIXTURE, options: { kind: 'bytes' } });
}

/**
 * `p`, or a rejection naming what failed to settle.
 *
 * The point of the whole file: a hang has to become a *failure*, and it has to do
 * so long before the runner's own timeout would notice.
 */
async function withTimeout<T>(p: Promise<T>, what: string, ms = 5000): Promise<T> {
  let timer: NodeJS.Timeout | undefined;
  try {
    return await Promise.race([
      p,
      new Promise<never>((_resolve, reject) => {
        timer = setTimeout(
          () =>
            reject(
              new Error(
                `${what} did not settle within ${ms} ms. That is the teardown hang, not a ` +
                  'slow machine: a clean close costs single-digit milliseconds here.'
              )
            ),
          ms
        );
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

/** Resolves when the worker's thread is gone, however it went. */
function exited(pw: ProviderWorker): Promise<void> {
  if (pw.worker.threadId === -1) return Promise.resolve();
  return new Promise<void>((r) => pw.worker.once('exit', () => r()));
}

test('close() resolves on a worker that already exited normally', async () => {
  const pw = await worker();

  // Drive the normal shutdown *without* close(), which is what leaves close()
  // facing a spent 'exit' event. This is not a contrived route: it is exactly
  // what provider-host.mjs does on the release message.
  pw.worker.postMessage({ type: 'release' });
  await withTimeout(exited(pw), 'the worker exiting on the release message');
  assert.strictEqual(pw.worker.threadId, -1, 'the worker should be gone before close() is called');

  await withTimeout(pw.close(), 'close() after a normal exit');
});

test('close() resolves on a worker that was terminated', async () => {
  const pw = await worker();
  await withTimeout(pw.worker.terminate(), 'terminate()');

  // The measured defect, verbatim: before the fix this did not settle in 8 s.
  await withTimeout(pw.close(), 'close() after terminate()');
});

test('close() twice resolves both times, and the second waits for the first', async () => {
  const pw = await worker();

  const first = pw.close();
  const second = pw.close();

  // **Await only the second.** A flag-guarded `close()` hands the second caller an
  // already-resolved promise, so awaiting it says "teardown is done" while the
  // worker is still up — and `Promise.all([first, second])` would hide that
  // behind the first call's real work. Whoever holds the second reference is
  // usually `await using`, which is precisely the caller that must not proceed
  // early.
  await withTimeout(second, 'the second of two concurrent close() calls');
  assert.strictEqual(
    pw.worker.threadId,
    -1,
    'the second close() resolved while the worker was still running — it returned ' +
      'early instead of awaiting the shutdown the first call started'
  );

  await withTimeout(first, 'the first of two concurrent close() calls');

  // And sequentially, after everything is already down.
  await withTimeout(pw.close(), 'a third close() after the worker is down');
});

test('close() resolves on a worker whose exit event is already spent', async () => {
  const dead = await worker();
  await withTimeout(dead.worker.terminate(), 'terminate()');
  await withTimeout(exited(dead), 'the terminated worker exiting');

  // Wrapping a worker that is *already* gone. Its 'exit' fired before this object
  // existed, so no listener this constructor registers can ever see it — the case
  // that a constructor-registered listener alone does not cover.
  const wrapped = new vfs.ProviderWorker(dead.worker, dead.handle);
  await withTimeout(wrapped.close(), 'close() on a worker wrapped after it died');

  await withTimeout(dead.close(), 'close() on the original wrapper');
});

test('a worker that dies after registering is reported rather than swallowed', async () => {
  const seen: string[] = [];
  const onWarning = (w: Error): void => {
    if (w.name === 'AetherVfsProviderWorkerError') seen.push(w.message);
  };
  process.on('warning', onWarning);

  try {
    // Registers cleanly — so `providerWorker()` resolves and the promise that
    // could have rejected is spent — and only then throws.
    const pw = await vfs.providerWorker({ module: LATE_THROW, options: { afterMs: 50 } });

    await withTimeout(
      new Promise<void>((resolve) => {
        const tick = setInterval(() => {
          if (seen.length > 0) {
            clearInterval(tick);
            resolve();
          }
        }, 20);
        tick.unref?.();
      }),
      'a warning about the worker that died after registering'
    );

    assert.match(
      seen[0]!,
      /late-throw/,
      `the warning should name the underlying error; got: ${seen[0]}`
    );
    assert.match(seen[0]!, /after registering/);

    // And the death still leaves close() settling, which is defect 1 again by
    // another route: this worker exited without anyone calling close().
    await withTimeout(pw.close(), 'close() after the worker died on its own');
  } finally {
    process.off('warning', onWarning);
  }
});

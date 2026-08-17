'use strict';

// The package entry point. It does two things the addon cannot do for itself.
//
// **One: tell Rust where the addon was loaded from.**
// `vfs_embed::LaunchOpts::shim_dll` falls back to searching next to
// `std::env::current_exe()`, which inside an addon is `node.exe` — wherever the
// user happens to have installed Node, and nowhere near the DLLs this package
// ships. `__dirname` is the answer and only JS has it, so it is handed over at
// load time. Requiring `aethervfs.node` directly skips this and produces a
// "vfs_shim_dll.dll not found" that names no candidate directories.
//
// **Two: turn a plain JS object into a provider.**
// `registerProvider(obj)` builds the dispatcher that a threadsafe function needs
// (an N-API threadsafe function is created from a *function*, not an object) and
// hands both to Rust. The dispatcher is the piece that runs the host's method,
// awaits it if it returned a thenable, and turns a throw into a status — and the
// piece that must never itself throw, because an exception escaping a
// threadsafe-function callback is fatal to the process rather than to the call.
// `providerWorker()` puts the whole thing on a dedicated worker loop, which is
// the configuration spec §8c recommends and the only one measured immune to a
// busy main loop.

const fs = require('fs');
const path = require('path');

const addonPath = path.join(__dirname, 'aethervfs.node');

if (!fs.existsSync(addonPath)) {
  throw new Error(
    `aethervfs: native addon not found at ${addonPath}. ` +
      'Build it with `npm run build` (or `npm run build:release`) in ' +
      `${__dirname}. That builds the addon, the shim DLL and the ` +
      'separate-workspace payload DLL, and places all three here.'
  );
}

const native = require(addonPath);

native.setPackageDir(__dirname);

// Read once, from Rust, so there is exactly one definition of each number in the
// process. A dispatcher and a Rust caller that disagreed about which integer
// means `readAt` would give a provider that answers the wrong question.
const STATUS = native.statusCodes();
const OP = native.providerOps();
const KIND = native.kinds();
const OPEN = native.openFlags();

// ---------------------------------------------------------------------------
// Errors the host raises on purpose.
// ---------------------------------------------------------------------------

/**
 * Fail a provider call with a specific `ST_*` status.
 *
 * `code` may be a number from `statusCodes()` or its name (`'ST_NOT_FOUND'`).
 * Anything else thrown from a provider method becomes `ST_IO_ERROR` with the
 * stack logged — spec §8b rule 3.
 */
class VfsError extends Error {
  constructor(code, message) {
    const num = typeof code === 'string' ? STATUS[code] : code;
    if (typeof num !== 'number') {
      // A typo'd status name would otherwise produce a VfsError with no code,
      // which the bridge classifies as an ordinary host error and reports as
      // ST_IO_ERROR — a wrong answer that looks like a working one. Refuse it
      // where the mistake is.
      throw new TypeError(
        `aethervfs: VfsError(${JSON.stringify(code)}) is not a status. Use a name ` +
          `from statusCodes() (${Object.keys(STATUS).join(', ')}) or its number.`
      );
    }
    super(
      message ??
        `VfsError(${typeof code === 'string' ? code : statusName(num)})`
    );
    this.name = 'VfsError';
    this.code = num;
    // Duck-typed as well as classed. `instanceof` is per-realm, and a provider
    // may well be loaded in a worker or a second copy of this module; a plain
    // property is what actually survives that.
    this.vfsStatus = num;
  }
}

function statusName(code) {
  for (const [name, value] of Object.entries(STATUS)) {
    if (value === code) return name;
  }
  return `status ${code}`;
}

/** The `ST_*` code an exception asks for, or `null` if it is not asking. */
function vfsStatusOf(e) {
  if (e === null || typeof e !== 'object') return null;
  if (typeof e.vfsStatus === 'number') return e.vfsStatus;
  if (e instanceof VfsError && typeof e.code === 'number') return e.code;
  return null;
}

// ---------------------------------------------------------------------------
// Result coercion. Each function names the method it is checking, because the
// message is the whole value of refusing a bad shape here.
// ---------------------------------------------------------------------------

function describe(v) {
  if (v === null) return 'null';
  if (v === undefined) return 'undefined';
  if (typeof v === 'object') return v.constructor?.name ?? 'object';
  return typeof v;
}

function toBuffer(v, what) {
  if (Buffer.isBuffer(v)) return v;
  if (ArrayBuffer.isView(v)) return Buffer.from(v.buffer, v.byteOffset, v.byteLength);
  if (v instanceof ArrayBuffer) return Buffer.from(v);
  if (typeof v === 'string') return Buffer.from(v, 'utf8');
  throw new TypeError(
    `aethervfs: ${what} must return a Buffer, a typed array or a string; got ${describe(v)}. ` +
      'A short read is legal — return fewer bytes than asked for, or an empty ' +
      'buffer at EOF — but the result has to be bytes.'
  );
}

function toKind(k, what) {
  if (typeof k === 'number') return k;
  const named = KIND[`KIND_${String(k).toUpperCase()}`];
  if (named === undefined) {
    throw new TypeError(
      `aethervfs: ${what} returned kind ${JSON.stringify(k)}; expected 'file', ` +
        `'dir', 'tombstone', or a number from kinds()`
    );
  }
  return named;
}

/**
 * `undefined`, not `null`, for an absent `mtime` — and that is a correctness
 * requirement rather than a style choice.
 *
 * napi-derive decodes an `Option<f64>` field of an `#[napi(object)]` with
 * `JsObject::get`, which returns `None` only when the property is **absent** and
 * otherwise converts the value to `f64`. A present `null` therefore fails with
 * "Failed to convert napi value Null into rust type `f64`", and it fails *inside
 * `completeCall`* — so the call never settles, one director thread parks
 * forever, and the symptom is a hang five seconds after a stall warning rather
 * than an error naming the field. `undefined` and an omitted key both decode to
 * `None`. See the `settle` fallback for the second half of this fix.
 */
function optionalNumber(v) {
  return v === undefined || v === null ? undefined : Number(v);
}

function toStat(v, what) {
  if (v === null || typeof v !== 'object') {
    throw new TypeError(
      `aethervfs: ${what} must return { kind, size, mtime? } or null; got ${describe(v)}`
    );
  }
  const src = v.stat ?? v;
  const kind = src.kind ?? (src.isDir ? 'dir' : 'file');
  return {
    kind: toKind(kind, what),
    size: Number(src.size ?? 0),
    mtime: optionalNumber(src.mtime),
  };
}

function toEntries(v, what) {
  if (v === undefined || v === null) return [];
  if (!Array.isArray(v)) {
    throw new TypeError(
      `aethervfs: ${what} must return an array of { name, kind, size } (or []); got ${describe(v)}`
    );
  }
  return v.map((e) => {
    if (typeof e === 'string') {
      throw new TypeError(
        `aethervfs: ${what} returned the bare name ${JSON.stringify(e)}. Each ` +
          'entry needs its stat too — the director merges directories by kind ' +
          'and size and cannot invent them.'
      );
    }
    const stat = toStat(e, what);
    return { name: String(e.name), kind: stat.kind, size: stat.size, mtime: stat.mtime };
  });
}

function toOpen(v, what) {
  if (v === null || typeof v !== 'object') {
    throw new TypeError(
      `aethervfs: ${what} must return { handle, size, isDir? }; got ${describe(v)}. ` +
        'Throw `new VfsError("ST_NOT_FOUND")` for a path this provider does not have.'
    );
  }
  if (typeof v.handle !== 'number') {
    throw new TypeError(
      `aethervfs: ${what} must return a numeric \`handle\`; got ${describe(v.handle)}. ` +
        'The handle is opaque to the director and is only ever handed back to ' +
        'this provider.'
    );
  }
  return { handle: v.handle, size: Number(v.size ?? 0), isDir: Boolean(v.isDir) };
}

function toCount(v, what, max) {
  if (typeof v !== 'number' || !Number.isFinite(v) || v < 0) {
    throw new TypeError(
      `aethervfs: ${what} must return the number of bytes written; got ${describe(v)}. ` +
        'Returning nothing would be read as "wrote 0 bytes", which the write ' +
        'path retries — so it is refused rather than guessed.'
    );
  }
  return Math.min(v, max);
}

// ---------------------------------------------------------------------------
// The dispatcher.
// ---------------------------------------------------------------------------

function invoke(obj, req) {
  switch (req.op) {
    case OP.getattr:
      return obj.getattr(req.root, req.path);
    case OP.readdir:
      return obj.readdir(req.root, req.path);
    case OP.open:
      return obj.open(req.root, req.path, req.flags);
    case OP.close:
      return obj.close(req.handle);
    case OP.readAt:
      return obj.readAt(req.handle, req.offset, req.len);
    case OP.readNext:
      return obj.readNext(req.handle, req.len);
    case OP.writeAt:
      return obj.writeAt(req.handle, req.offset, req.data);
    case OP.setLen:
      return obj.setLen(req.handle, req.size);
    case OP.flush:
      return obj.flush(req.handle);
    case OP.mkdir:
      return obj.mkdir(req.root, req.path);
    case OP.remove:
      return obj.remove(req.root, req.path);
    case OP.rename:
      return obj.rename(req.root, req.path, req.root2, req.path2);
    case OP.setAttr:
      return obj.setAttr(req.root, req.path, { mtime: req.mtime, size: req.size });
    default:
      throw new Error(`aethervfs: no dispatch for provider op ${req.op}`);
  }
}

function encode(req, value) {
  switch (req.op) {
    case OP.getattr:
      // `null`/`undefined` is "this provider does not have that path", which is
      // `Ok(None)` and not an error.
      return value === undefined || value === null
        ? { status: 0 }
        : { status: 0, stat: toStat(value, 'getattr') };
    case OP.readdir:
      return { status: 0, entries: toEntries(value, 'readdir') };
    case OP.open:
      return { status: 0, open: toOpen(value, 'open') };
    case OP.readAt:
      return { status: 0, bytes: toBuffer(value, 'readAt') };
    case OP.readNext:
      return { status: 0, bytes: toBuffer(value, 'readNext') };
    case OP.writeAt:
      return { status: 0, number: toCount(value, 'writeAt', req.data ? req.data.length : 0) };
    default:
      // close, setLen, flush, mkdir, remove, rename, setAttr: success is the
      // absence of a throw.
      return { status: 0 };
  }
}

/**
 * A rejection or throw, as a result. `VfsError` keeps its code; anything else is
 * `ST_IO_ERROR` with the stack attached so Rust can log and count it.
 */
function errorResult(e) {
  const code = vfsStatusOf(e);
  if (code !== null) return { status: code, threw: true };
  const stack = e instanceof Error && e.stack ? e.stack : String(e);
  return { status: STATUS.ST_IO_ERROR, threw: true, hostError: stack };
}

/**
 * Build the function the threadsafe function calls.
 *
 * **Nothing may escape this.** napi-rs's `ErrorStrategy::Fatal` means an
 * exception out of here is not a failed call, it is a failed process — and spec
 * §8b rule 3 is that no throw crosses the boundary uncaught. So every stage is
 * wrapped: the host method, the result coercion, the promise's two arms, and the
 * `completeCall` that reports them. The innermost fallback is to do nothing,
 * which leaves the call outstanding — counted as a stall, which is exactly what
 * the contract asks for.
 */
function makeDispatch(obj) {
  return function dispatch(req) {
    const settle = (result) => {
      try {
        native.completeCall(req.callId, result);
        return;
      } catch (e) {
        // A bug in this file, a released bridge, or a payload Rust could not
        // decode. Leaving the call outstanding was the old behaviour and it is
        // the worst one available: the director thread parks forever and the
        // host sees a stall warning five seconds later with no mention of the
        // field that failed. So report it *and* settle the call as a failure,
        // with a payload that has nothing optional in it to decode.
        process.emitWarning(
          `aethervfs: completeCall(${req.callId}) failed: ${e && e.stack ? e.stack : e}`
        );
        try {
          native.completeCall(req.callId, {
            status: STATUS.ST_IO_ERROR,
            threw: true,
            hostError: `aethervfs: the provider's result for \`${req.op}\` could not cross the ` +
              `boundary: ${e && e.message ? e.message : e}`,
          });
        } catch {
          /* nothing left to try; the call is counted as a stall */
        }
      }
    };
    try {
      let value;
      try {
        value = invoke(obj, req);
      } catch (e) {
        settle(errorResult(e));
        return;
      }
      if (value !== null && typeof value === 'object' && typeof value.then === 'function') {
        // Rule 2: the director thread is parked and will wait as long as this
        // takes. A promise that never settles is the case spec §8b names, and
        // it is counted on the Rust side rather than defended against here.
        value.then(
          (v) => {
            try {
              settle(encode(req, v));
            } catch (e) {
              settle(errorResult(e));
            }
          },
          (e) => settle(errorResult(e))
        );
        return;
      }
      settle(encode(req, value));
    } catch (e) {
      // Last resort. `encode` on the synchronous path, or anything unforeseen.
      try {
        settle(errorResult(e));
      } catch {
        /* nothing left to try; the call is counted as a stall */
      }
    }
  };
}

/**
 * Mount a JS object as a provider. Returns a `Provider` whose `handle` is a
 * process-global integer.
 *
 * **Call this on the thread that will service the provider** — its event loop is
 * where every method runs, and it is the one thread that may not drive a session
 * mounting this provider. `providerWorker()` is the recommended way to get that
 * right; this is the primitive underneath it, for a host already running on the
 * loop it wants (and for the deadlock guard's own tests).
 */
function registerProvider(obj, options) {
  if (obj === null || typeof obj !== 'object') {
    throw new TypeError(
      `aethervfs: registerProvider(obj) needs an object with provider methods; got ${describe(obj)}`
    );
  }
  return native.registerProvider(obj, makeDispatch(obj), options ?? {});
}

// ---------------------------------------------------------------------------
// Spec §6's primitive catalog.
//
// The Rust side of each of these takes a **handle** (a process-global integer),
// not a `Provider` object, because that is the only thing that means the same in
// two isolates — a graph composed on the main thread out of a provider
// registered in a worker is the arrangement task 7 made mandatory for a
// JS-authored leaf. These wrappers are what let a host write `readonly(base)`
// instead of `readonly(base.handle)`, accept a bare number just as happily, and
// turn the two collection-shaped primitives (`layered`, `router`) into the
// signatures spec §8 writes them with.
// ---------------------------------------------------------------------------

/** A `Provider`, a `ProviderWorker`, or a raw handle → the handle. */
function handleOf(p, what) {
  if (typeof p === 'number' && Number.isInteger(p) && p >= 0) return p;
  if (p !== null && typeof p === 'object' && typeof p.handle === 'number') return p.handle;
  throw new TypeError(
    `aethervfs: ${what} needs a Provider (or its numeric handle); got ${describe(p)}. ` +
      'Every primitive returns one, and `providerWorker()` exposes `.provider`.'
  );
}

/** `{ 'a.ini': bytes }` or `[{ path, bytes }]` → what Rust's `memory()` takes. */
function toMemoryFiles(files) {
  if (files === undefined || files === null) return [];
  const list = Array.isArray(files)
    ? files.map((f) => [f.path, f.bytes])
    : Object.entries(files);
  return list.map(([p, bytes]) => {
    if (typeof p !== 'string' || p.length === 0) {
      throw new TypeError(
        `aethervfs: memory() keys must be non-empty vpaths; got ${describe(p)}`
      );
    }
    return { path: p, bytes: toBuffer(bytes, `memory()['${p}']`) };
  });
}

/**
 * A read-write in-memory file tree (spec §6's `memory`).
 *
 * `memory({ 'Skyrim.ini': iniBytes })`. Nothing touches disk, and it declares
 * `ReadWrite`, so it works as an `overlay` upper or as the target of a `router`
 * route for exactly the paths a host wants writable. Read back what was written
 * through the graph — `session.readFile(vpath)`.
 */
function memory(files) {
  return native.memory(toMemoryFiles(files));
}

/** Demote a provider to read-only (spec §6's `readonly`). */
function readonly(provider) {
  return native.readonly(handleOf(provider, 'readonly(provider)'));
}

/** Positional reads over a forward-only provider (spec §6's `seekable`). */
function seekable(provider) {
  return native.seekable(handleOf(provider, 'seekable(provider)'));
}

/** A block cache in front of a provider (spec §6's `cached`). */
function cached(provider, options) {
  return native.cached(handleOf(provider, 'cached(provider)'), options ?? {});
}

/**
 * Stack providers so a **later** argument wins (spec §6's `layered`).
 *
 * `layered(readonly(base), disk(mods))` — the mod wins over the vanilla file,
 * which is the only ordering a mod manager can have. Accepts a spread or a
 * single array, because a host building the list programmatically has an array.
 */
function layered(...providers) {
  const list = providers.length === 1 && Array.isArray(providers[0]) ? providers[0] : providers;
  return native.layered(list.map((p, i) => handleOf(p, `layered() argument ${i}`)));
}

/** Copy-up writes and whiteouts over a base (spec §6's `overlay`). */
function overlay(base, upper) {
  return native.overlay(handleOf(base, 'overlay(base, …)'), handleOf(upper, 'overlay(…, upper)'));
}

/**
 * Dispatch by glob, falling back to `defaultProvider` (spec §6's `router`).
 *
 * `router({ '*.ini': inis }, overlay(disk(docs), disk(scratch)))`. Routes may be
 * an object — insertion order is match order, which JS guarantees for string
 * keys — or an array of `[pattern, provider]` pairs when a pattern would be a
 * duplicate key.
 *
 * `readdir` is single-dispatch, not the union spec §6 specifies: a file served
 * by a route is readable by name and **invisible to a directory listing**. See
 * the Rust doc comment on `router` for what to do about it.
 */
function router(routes, defaultProvider) {
  const entries = Array.isArray(routes)
    ? routes.map((r) => (Array.isArray(r) ? r : [r.pattern, r.provider]))
    : Object.entries(routes ?? {});
  return native.router(
    entries.map(([pattern, provider]) => ({
      pattern: String(pattern),
      provider: handleOf(provider, `router() route ${JSON.stringify(pattern)}`),
    })),
    handleOf(defaultProvider, 'router(routes, defaultProvider)')
  );
}

/**
 * Run the workspace's conformance suite against a provider — stage 4's gate.
 *
 * Not a TypeScript reimplementation: this calls
 * `vfs_provider::assert_conformance`, the same function every Rust provider is
 * held to. Resolves to a report, rejects with the failing case's message.
 *
 * The suite runs on a libuv pool thread, so `await` works for a provider on this
 * loop as well as one in a worker — awaiting is what leaves the servicing loop
 * free to run the callbacks. Do not block the loop while awaiting it.
 */
function assertConformance(provider) {
  return native.assertConformance(handleOf(provider, 'assertConformance(provider)'));
}

// ---------------------------------------------------------------------------
// The recommended shape: a provider on its own worker loop.
// ---------------------------------------------------------------------------

/**
 * Load a provider module in a dedicated worker and register it there.
 *
 * `spec.module` is an **absolute path** to a CommonJS module — not an object.
 * That is not an ergonomic preference, it is the constraint spec §8c records:
 * isolates share no JS objects, so a provider instance cannot be handed across
 * one. What crosses is the integer handle, which `mount()` accepts from any
 * thread.
 *
 * The worker's loop is the one that services every call, so:
 *
 *  - a session on the main thread (or on any other worker) may call into it
 *    freely — task 5 measured main → worker at 47 µs;
 *  - the worker itself must not drive a session that mounts it, and the deadlock
 *    guard refuses that rather than hanging;
 *  - the main thread doing UI work costs the provider nothing, which is the
 *    difference between 1449 and 3.8 MiB/s in §8c's loaded-main-loop row.
 */
function providerWorker(spec) {
  const { Worker } = require('worker_threads');
  if (spec === null || typeof spec !== 'object' || typeof spec.module !== 'string') {
    throw new TypeError(
      'aethervfs: providerWorker({ module, options?, export?, provider? }) needs ' +
        '`module`: an absolute path to the provider module. Use require.resolve().'
    );
  }
  if (!path.isAbsolute(spec.module)) {
    throw new TypeError(
      `aethervfs: providerWorker module ${JSON.stringify(spec.module)} must be an ` +
        'absolute path — the worker resolves it, and its idea of "here" is not ' +
        "the caller's. Use require.resolve()."
    );
  }

  return new Promise((resolve, reject) => {
    const worker = new Worker(path.join(__dirname, 'provider-host.cjs'), {
      workerData: {
        module: spec.module,
        export: spec.export ?? null,
        options: spec.options ?? {},
        providerOptions: spec.provider ?? {},
      },
    });
    let settled = false;
    const fail = (e) => {
      if (settled) return;
      settled = true;
      worker.terminate().then(
        () => reject(e),
        () => reject(e)
      );
    };
    worker.once('error', fail);
    worker.once('exit', (code) => {
      if (!settled) {
        fail(new Error(`aethervfs: provider worker exited (code ${code}) before registering`));
      }
    });
    worker.once('message', (msg) => {
      if (settled) return;
      if (!msg || msg.ok !== true) {
        const detail = msg && msg.stack ? msg.stack : msg && msg.message ? msg.message : String(msg);
        fail(
          new Error(
            `aethervfs: provider module ${spec.module} did not register:\n${detail}`
          )
        );
        return;
      }
      settled = true;
      resolve(new ProviderWorker(worker, msg.handle));
    });
  });
}

/** The handle to a provider running on its own worker loop. */
class ProviderWorker {
  constructor(worker, handle) {
    this.worker = worker;
    this.handle = handle;
    this.provider = native.Provider.fromHandle(handle);
    this._closed = false;
  }

  /** Counters for this provider — see `Provider.stats()`. */
  stats() {
    return this.provider.stats();
  }

  /**
   * Release the provider's loop and stop the worker.
   *
   * The registry entry stays, because a handle is process-global and outlives
   * the object that made it by design; calls after this fail with a status
   * naming the released loop rather than hanging. Without it the worker never
   * exits — a live threadsafe function keeps its loop alive, which is the whole
   * reason the worker stays up while the session runs.
   */
  async close() {
    if (this._closed) return;
    this._closed = true;
    const exited = new Promise((r) => this.worker.once('exit', r));
    this.worker.postMessage({ type: 'release' });
    const timer = setTimeout(() => this.worker.terminate(), 2000);
    // Node keeps the process alive for a pending timer; this one is only a
    // backstop for a worker that ignored the message.
    timer.unref?.();
    await exited;
    clearTimeout(timer);
  }
}

module.exports = {
  ...native,
  VfsError,
  registerProvider,
  providerWorker,
  ProviderWorker,
  statusName,
  // Spec §6's catalog. These deliberately shadow the native exports of the same
  // name, which take integer handles; a host passes providers.
  memory,
  readonly,
  seekable,
  cached,
  layered,
  overlay,
  router,
  assertConformance,
  /** `{ ST_OK: 0, ST_NOT_FOUND: -2, ... }`, read once from Rust. */
  STATUS,
  /** `{ getattr: 1, readdir: 2, ... }` — the op integers, for diagnostics. */
  OP,
  /** `{ KIND_FILE: 1, KIND_DIR: 2, KIND_TOMBSTONE: 3 }`. */
  KIND,
  /** `{ OPEN_READ, OPEN_WRITE, OPEN_CREATE, ... }` — the bits `open`'s `flags` carries. */
  OPEN,
};

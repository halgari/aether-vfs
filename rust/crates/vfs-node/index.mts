// The package entry point. It does two things the addon cannot do for itself.
//
// **One: tell Rust where the addon was loaded from.**
// `vfs_embed::LaunchOpts::shim_dll` falls back to searching next to
// `std::env::current_exe()`, which inside an addon is `node.exe` — wherever the
// user happens to have installed Node, and nowhere near the DLLs this package
// ships. `import.meta.dirname` is the answer and only JS has it, so it is handed
// over at load time. Requiring `aethervfs.node` directly skips this and produces a
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
//
// ---------------------------------------------------------------------------
// **This file is the source; `index.mjs` and `index.d.mts` are its outputs.**
//
// `index.d.ts` used to be hand-written beside a hand-written `index.cjs`, and
// the two had to agree by discipline. They did not: the declaration claimed the
// launch `env` variables were set on the child only when the seam sets them
// process-wide, and its `memory()` example read back an unfolded name from the
// wrong root — teaching the exact silent corruption the same file warned about
// 280 lines further down. Emitting the declaration removes the class of defect
// where the two disagree about a *signature*. It does not remove the class where
// they disagree about *prose*, and both of those defects were prose — which is
// why `scripts/check-types.cts` still exists and why the doc comments below are
// written where the code is, not in a second file.
//
// `native.mts` holds the addon's own surface. That one is still asserted rather
// than derived, because `@napi-rs/cli` is the thing that could derive it and
// this package does not take a network dependency to build.
// ---------------------------------------------------------------------------

import * as path from 'node:path';
import { Worker as WorkerCtor } from 'node:worker_threads';

import * as native from './native.mjs';
import type {
  CacheOptions,
  CallRequest,
  CallResult,
  ConformanceReport,
  JsDirEntry,
  JsOpen,
  JsStat,
  Provider,
  ProviderOptions,
  ProviderStats,
} from './native.mjs';

// Everything the addon exports, forwarded. Ten of these names are shadowed by
// the wrappers below — the primitives, `registerProvider` and `releaseProvider`
// — and a local export wins over `export *`, which is what makes a host's
// `readonly(base)` reach the wrapper and not Rust's `readonly(handle)`.
//
// The forward is deliberately a `export *` and not a list: a `#[napi]` export
// added on the Rust side reaches a host without an edit here. What it does *not*
// do is declare itself — `native.mts` has to name it before TypeScript can see
// it, and `scripts/check-types.cts` is what notices when it has not.
export * from './native.mjs';

native.setPackageDir(import.meta.dirname);

// Read once, from Rust, so there is exactly one definition of each number in the
// process. A dispatcher and a Rust caller that disagreed about which integer
// means `readAt` would give a provider that answers the wrong question.
/** `{ ST_OK: 0, ST_NOT_FOUND: -2, ... }`, read once from Rust at load. */
export const STATUS: Record<string, number> = native.statusCodes();
/** `{ getattr: 1, readdir: 2, ... }` — the op integers a `CallRequest` carries. */
export const OP: Record<string, number> = native.providerOps();
/** `{ KIND_FILE: 1, KIND_DIR: 2, KIND_TOMBSTONE: 3 }`. */
export const KIND: Record<string, number> = native.kinds();
/** `{ OPEN_READ, OPEN_WRITE, OPEN_CREATE, OPEN_TRUNC, OPEN_APPEND, OPEN_EXCL }`. */
export const OPEN: Record<string, number> = native.openFlags();

/**
 * The subset of one of those tables that this file's own code indexes by name,
 * checked at load.
 *
 * Reading `OP.readAt` out of a `Record<string, number>` is `number | undefined`,
 * and the honest way to spend that `undefined` is to prove it cannot happen
 * rather than to assert it away: a Rust-side rename would otherwise make
 * `case undefined:` a branch that never matches, and the op would fall through
 * to `default` as "no dispatch for provider op 5". This says so at load instead.
 */
function codes<K extends string>(
  table: Record<string, number>,
  names: readonly K[],
  what: string
): Record<K, number> {
  const out = {} as Record<K, number>;
  const missing: string[] = [];
  for (const name of names) {
    const v = table[name];
    if (typeof v !== 'number') missing.push(name);
    else out[name] = v;
  }
  if (missing.length > 0) {
    throw new TypeError(
      `aethervfs: the addon's ${what} does not define ${missing.join(', ')}. ` +
        'The addon and this JavaScript are built together and have diverged; ' +
        'run `pnpm build`.'
    );
  }
  return out;
}

const ST = codes(STATUS, ['ST_IO_ERROR'] as const, 'statusCodes()');

const OPS = codes(
  OP,
  [
    'getattr',
    'readdir',
    'open',
    'close',
    'readAt',
    'readNext',
    'writeAt',
    'setLen',
    'flush',
    'mkdir',
    'remove',
    'rename',
    'setAttr',
  ] as const,
  'providerOps()'
);

// ---------------------------------------------------------------------------
// Errors the host raises on purpose.
// ---------------------------------------------------------------------------

/**
 * Fail a provider call with a specific `ST_*` status.
 *
 * `code` may be a number from `statusCodes()` or its name (`'ST_NOT_FOUND'`).
 * Anything else thrown or rejected from a provider method becomes `ST_IO_ERROR`,
 * with the stack logged and counted — spec §8b rule 3. A `VfsError` whose code is
 * not a status this workspace defines (or is `ST_OK`) is clamped to `ST_IO_ERROR`
 * on the Rust side, so a host cannot invent a code.
 */
export class VfsError extends Error {
  /** The `ST_*` number. */
  readonly code: number;
  /** The same number, duck-typed so it survives crossing a realm. */
  readonly vfsStatus: number;

  constructor(code: number | string, message?: string) {
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
    super(message ?? `VfsError(${typeof code === 'string' ? code : statusName(num)})`);
    this.name = 'VfsError';
    this.code = num;
    // Duck-typed as well as classed. `instanceof` is per-realm, and a provider
    // may well be loaded in a worker or a second copy of this module; a plain
    // property is what actually survives that.
    this.vfsStatus = num;
  }
}

/** The `ST_*` name for a status, or `"status <n>"` if it is not one. */
export function statusName(code: number): string {
  for (const [name, value] of Object.entries(STATUS)) {
    if (value === code) return name;
  }
  return `status ${code}`;
}

/** The `ST_*` code an exception asks for, or `null` if it is not asking. */
function vfsStatusOf(e: unknown): number | null {
  if (e === null || typeof e !== 'object') return null;
  const status = (e as { vfsStatus?: unknown }).vfsStatus;
  if (typeof status === 'number') return status;
  if (e instanceof VfsError && typeof e.code === 'number') return e.code;
  return null;
}

// ---------------------------------------------------------------------------
// Result coercion. Each function names the method it is checking, because the
// message is the whole value of refusing a bad shape here.
// ---------------------------------------------------------------------------

function describe(v: unknown): string {
  if (v === null) return 'null';
  if (v === undefined) return 'undefined';
  if (typeof v === 'object') {
    return (v as { constructor?: { name?: string } }).constructor?.name ?? 'object';
  }
  return typeof v;
}

function toBuffer(v: unknown, what: string): Buffer {
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

function toKind(k: unknown, what: string): number {
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
function optionalNumber(v: unknown): number | undefined {
  return v === undefined || v === null ? undefined : Number(v);
}

function toStat(v: unknown, what: string): JsStat {
  if (v === null || typeof v !== 'object') {
    throw new TypeError(
      `aethervfs: ${what} must return { kind, size, mtime? } or null; got ${describe(v)}`
    );
  }
  const outer = v as ProviderStat & { stat?: ProviderStat };
  const src: ProviderStat = outer.stat ?? outer;
  const kind = src.kind ?? (src.isDir ? 'dir' : 'file');
  return {
    kind: toKind(kind, what),
    size: Number(src.size ?? 0),
    mtime: optionalNumber(src.mtime),
  };
}

function toEntries(v: unknown, what: string): JsDirEntry[] {
  if (v === undefined || v === null) return [];
  if (!Array.isArray(v)) {
    throw new TypeError(
      `aethervfs: ${what} must return an array of { name, kind, size } (or []); got ${describe(v)}`
    );
  }
  return (v as unknown[]).map((e) => {
    if (typeof e === 'string') {
      throw new TypeError(
        `aethervfs: ${what} returned the bare name ${JSON.stringify(e)}. Each ` +
          'entry needs its stat too — the director merges directories by kind ' +
          'and size and cannot invent them.'
      );
    }
    const stat = toStat(e, what);
    return {
      name: String((e as { name?: unknown }).name),
      kind: stat.kind,
      size: stat.size,
      mtime: stat.mtime,
    };
  });
}

function toOpen(v: unknown, what: string): JsOpen {
  if (v === null || typeof v !== 'object') {
    throw new TypeError(
      `aethervfs: ${what} must return { handle, size, isDir? }; got ${describe(v)}. ` +
        'Throw `new VfsError("ST_NOT_FOUND")` for a path this provider does not have.'
    );
  }
  const o = v as { handle?: unknown; size?: unknown; isDir?: unknown };
  if (typeof o.handle !== 'number') {
    throw new TypeError(
      `aethervfs: ${what} must return a numeric \`handle\`; got ${describe(o.handle)}. ` +
        'The handle is opaque to the director and is only ever handed back to ' +
        'this provider.'
    );
  }
  return { handle: o.handle, size: Number(o.size ?? 0), isDir: Boolean(o.isDir) };
}

function toCount(v: unknown, what: string, max: number): number {
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

/**
 * Call the host's method for one request.
 *
 * The optional methods are called through `!`. Rust reads the object's methods
 * once at registration and never dispatches an op the object does not have — a
 * missing optional method answers `ST_NOT_SUPPORTED` without a round trip — so
 * the property is present whenever this line runs. If that ever stopped being
 * true the resulting `TypeError` is caught by `makeDispatch` and reported as
 * `ST_IO_ERROR` with a stack, which is the same outcome as any other host throw.
 */
function invoke(obj: ProviderObject, req: CallRequest): unknown {
  switch (req.op) {
    case OPS.getattr:
      return obj.getattr(req.root, req.path);
    case OPS.readdir:
      return obj.readdir(req.root, req.path);
    case OPS.open:
      return obj.open(req.root, req.path, req.flags);
    case OPS.close:
      return obj.close(req.handle);
    case OPS.readAt:
      return obj.readAt!(req.handle, req.offset, req.len);
    case OPS.readNext:
      return obj.readNext!(req.handle, req.len);
    case OPS.writeAt:
      return obj.writeAt!(req.handle, req.offset, req.data!);
    case OPS.setLen:
      return obj.setLen!(req.handle, req.size!);
    case OPS.flush:
      return obj.flush!(req.handle);
    case OPS.mkdir:
      return obj.mkdir!(req.root, req.path);
    case OPS.remove:
      return obj.remove!(req.root, req.path);
    case OPS.rename:
      return obj.rename!(req.root, req.path, req.root2!, req.path2!);
    case OPS.setAttr:
      return obj.setAttr!(req.root, req.path, {
        mtime: req.mtime ?? null,
        size: req.size ?? null,
      });
    default:
      throw new Error(`aethervfs: no dispatch for provider op ${req.op}`);
  }
}

function encode(req: CallRequest, value: unknown): CallResult {
  switch (req.op) {
    case OPS.getattr:
      // `null`/`undefined` is "this provider does not have that path", which is
      // `Ok(None)` and not an error.
      return value === undefined || value === null
        ? { status: 0 }
        : { status: 0, stat: toStat(value, 'getattr') };
    case OPS.readdir:
      return { status: 0, entries: toEntries(value, 'readdir') };
    case OPS.open:
      return { status: 0, open: toOpen(value, 'open') };
    case OPS.readAt:
      return { status: 0, bytes: toBuffer(value, 'readAt') };
    case OPS.readNext:
      return { status: 0, bytes: toBuffer(value, 'readNext') };
    case OPS.writeAt:
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
function errorResult(e: unknown): CallResult {
  const code = vfsStatusOf(e);
  if (code !== null) return { status: code, threw: true };
  const stack = e instanceof Error && e.stack ? e.stack : String(e);
  return { status: ST.ST_IO_ERROR, threw: true, hostError: stack };
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
function makeDispatch(obj: ProviderObject): (req: CallRequest) => void {
  return function dispatch(req: CallRequest): void {
    const settle = (result: CallResult): void => {
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
          `aethervfs: completeCall(${req.callId}) failed: ${
            e instanceof Error && e.stack ? e.stack : String(e)
          }`
        );
        try {
          native.completeCall(req.callId, {
            status: ST.ST_IO_ERROR,
            threw: true,
            hostError:
              `aethervfs: the provider's result for \`${req.op}\` could not cross the ` +
              `boundary: ${e instanceof Error && e.message ? e.message : String(e)}`,
          });
        } catch {
          /* nothing left to try; the call is counted as a stall */
        }
      }
    };
    try {
      let value: unknown;
      try {
        value = invoke(obj, req);
      } catch (e) {
        settle(errorResult(e));
        return;
      }
      if (
        value !== null &&
        typeof value === 'object' &&
        typeof (value as { then?: unknown }).then === 'function'
      ) {
        // Rule 2: the director thread is parked and will wait as long as this
        // takes. A promise that never settles is the case spec §8b names, and
        // it is counted on the Rust side rather than defended against here.
        (value as PromiseLike<unknown>).then(
          (v) => {
            try {
              settle(encode(req, v));
            } catch (e) {
              settle(errorResult(e));
            }
          },
          (e: unknown) => settle(errorResult(e))
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

// ---------------------------------------------------------------------------
// Release accounting.
//
// `releaseProvider` is **mandatory, not hygiene**: a live threadsafe function
// holds a ref on the loop that services it, which is exactly what keeps the
// provider callable — and therefore what stops the loop from ever draining. A
// leaked handle is a process that never exits.
//
// The failure has no natural diagnostic, because the symptom *is* the absence of
// one: the loop never drains, so nothing that runs "on the way out" ever runs.
// The two things that can be done are done here — make release structural
// (`Symbol.dispose` / `Symbol.asyncDispose`, so `using` cannot forget), and name
// the leak on any exit that does happen, which covers every host that calls
// `process.exit()` or lets a worker finish.
// ---------------------------------------------------------------------------

/** Handles from `registerProvider` on this thread that have not been released. */
const unreleased = new Set<number>();
let exitHookInstalled = false;

function noteRegistered(handle: number): void {
  unreleased.add(handle);
  if (exitHookInstalled) return;
  exitHookInstalled = true;
  process.on('exit', () => {
    if (unreleased.size === 0) return;
    // **Written straight to stderr, not through `process.emitWarning`.** That
    // function defers to `process.nextTick`, and no tick ever runs after `exit`
    // begins — so the warning would be composed and then dropped, which is
    // exactly the kind of silent nothing this whole message exists to break.
    // The `(node:pid) Name: ` shape is copied from node's own warning format so
    // it reads like one.
    process.stderr.write(
      `(node:${process.pid}) AetherVfsProviderLeak: ` +
        `aethervfs: ${unreleased.size} JS provider(s) were never released ` +
        `(handle${unreleased.size === 1 ? '' : 's'} ${[...unreleased].join(', ')}). ` +
        'A live provider holds a ref on the loop that services it, so this thread ' +
        'would not have exited on its own — if this process needed a process.exit() ' +
        'or a terminate() to finish, that is why. Call releaseProvider(handle), or ' +
        'use `using p = registerProvider(...)` / `await using w = await ' +
        'providerWorker(...)`, which release on scope exit. For a composed graph the ' +
        'handles to release are `provider.jsLeaves()`, not the composed handle.\n'
    );
  });
}

/** What a provider's `getattr` may return. `null` means "not here". */
export interface ProviderStat {
  /** `'file' | 'dir' | 'tombstone'`, or a number from `KIND`. */
  kind?: 'file' | 'dir' | 'tombstone' | number;
  /** Shorthand for `kind: 'dir'`. */
  isDir?: boolean;
  size?: number;
  mtime?: number | null;
}

export interface ProviderDirEntry extends ProviderStat {
  name: string;
}

export interface ProviderOpenResult {
  /** Opaque to the director; only ever handed back to this provider. */
  handle: number;
  size?: number;
  isDir?: boolean;
}

/**
 * What a JS provider looks like.
 *
 * Every method may be `async` — the calling director thread parks until the
 * promise settles, for as long as that takes. `capabilities` is read **once, at
 * registration**, and the methods present at that moment are the ones that will
 * ever be called: a missing optional method answers `ST_NOT_SUPPORTED` without a
 * round trip, exactly as a Rust provider's trait defaults do.
 *
 * The five methods a read-only provider must have are `getattr`, `readdir`,
 * `open`, `close` and `readAt` (or `readNext` for `seqread`). Declaring
 * `readwrite` additionally requires `writeAt`, and `registerProvider` refuses the
 * object at construction if it is missing.
 */
export interface ProviderObject {
  capabilities?: {
    /** Default `'read'`. `'seqread'` must be wrapped in `seekable`. */
    access?: 'read' | 'readwrite' | 'seqread';
    /** Content never changes. Illegal together with `'readwrite'`. */
    immutable?: boolean;
    /** Reads are expensive; this provider wants a cache in front. */
    slow?: boolean;
    /** Block-size hint for `cached`. §8c measured 64 KiB as the best tested. */
    preferredBlock?: number;
  };

  getattr(root: number, path: string): ProviderStat | null | Promise<ProviderStat | null>;
  readdir(root: number, path: string): ProviderDirEntry[] | Promise<ProviderDirEntry[]>;
  /** `flags` carries the `OPEN_*` bits. Throw `VfsError('ST_NOT_FOUND')` for an absent path. */
  open(
    root: number,
    path: string,
    flags: number
  ): ProviderOpenResult | Promise<ProviderOpenResult>;
  close(handle: number): void | Promise<void>;

  /** Positional read. A short read is legal anywhere, not only at EOF. */
  readAt?(
    handle: number,
    offset: number,
    length: number
  ): Uint8Array | string | Promise<Uint8Array | string>;
  /** Forward-only read, for `access: 'seqread'`. */
  readNext?(
    handle: number,
    length: number
  ): Uint8Array | string | Promise<Uint8Array | string>;
  /** Must return the number of bytes written; returning nothing is refused rather than guessed. */
  writeAt?(handle: number, offset: number, data: Buffer): number | Promise<number>;
  setLen?(handle: number, length: number): void | Promise<void>;
  flush?(handle: number): void | Promise<void>;
  mkdir?(root: number, path: string): void | Promise<void>;
  remove?(root: number, path: string): void | Promise<void>;
  rename?(
    fromRoot: number,
    fromPath: string,
    toRoot: number,
    toPath: string
  ): void | Promise<void>;
  setAttr?(
    root: number,
    path: string,
    attr: { mtime: number | null; size: number | null }
  ): void | Promise<void>;
}

/**
 * Mount a JS object as a provider, serviced by the **calling** thread's event
 * loop. Returns a `Provider` whose `handle` is a process-global integer.
 *
 * That loop is the one thread that may not drive a session mounting this
 * provider: a blocking provider call issued on the loop that services it can
 * never settle, because the loop cannot run the callback while parked. The guard
 * refuses that with an explanation rather than hanging, but the way to not need
 * it is `providerWorker()`. This is the primitive underneath it, for a host
 * already running on the loop it wants (and for the deadlock guard's own tests).
 *
 * ## `releaseProvider` is mandatory, not hygiene
 *
 * A live threadsafe function holds a ref on the loop that services it — which is
 * precisely what keeps the provider callable, and therefore what stops that loop
 * from ever draining. **A handle that is never released is a thread that never
 * exits**: a worker stays up forever, and on the main thread the process hangs.
 *
 * There is no diagnostic for it, because the symptom *is* the absence of one —
 * nothing that runs "on the way out" ever runs. So:
 *
 * ```ts
 * using p = registerProvider(obj);        // released when the block ends
 * ```
 *
 * is the shape to write (`Provider[Symbol.dispose]`, Node 22.6+), and any exit
 * that *does* happen — a `process.exit()`, a worker finishing — emits an
 * `AetherVfsProviderLeak` warning naming the handles. For a composed graph the
 * handles to release are `provider.jsLeaves()`, not the composed handle;
 * `releaseProvider(composed.handle)` correctly refuses.
 */
export function registerProvider(obj: ProviderObject, options?: ProviderOptions): Provider {
  const o: unknown = obj;
  if (o === null || typeof o !== 'object') {
    throw new TypeError(
      `aethervfs: registerProvider(obj) needs an object with provider methods; got ${describe(o)}`
    );
  }
  const p = native.registerProvider(obj, makeDispatch(obj), options ?? {});
  noteRegistered(p.handle);
  return p;
}

/**
 * Release a JS provider's event loop, so the thread holding it can exit — see
 * {@link registerProvider} for why this is mandatory.
 *
 * Calls afterwards fail with a status naming the released loop; a director thread
 * already parked on it is woken. The registry entry stays, because a handle is
 * process-global by design. Idempotent from here on: a second call for a handle
 * already released is not an error a host should have to guard against.
 *
 * **A non-integer handle is refused, not rounded.** This wrapper used to pass a
 * number straight through while every other one took it through `handleOf`, and
 * that asymmetry was destructive here rather than merely inconsistent: handles are
 * process-global integers, Rust coerces the argument, so `releaseProvider(1.7)`
 * released handle `1` and `releaseProvider(NaN)` released handle `0` — someone
 * else's live provider, silently. Nothing reported it, because releasing a live
 * handle is a legitimate operation; the victim only found out when a later call
 * failed with a released-loop status.
 */
export function releaseProvider(handle: number | Provider): void {
  const n = handleOf(handle, 'releaseProvider(handle)');
  unreleased.delete(n);
  return native.releaseProvider(n);
}

// `using p = registerProvider(obj)` — release becomes structural rather than
// remembered. Defined on `Provider` rather than only on the bare registration
// result so that it works on a *composed* handle too: `jsLeaves()` is exactly
// "the JS providers reachable through this composition", which is the list
// `releaseProvider` has to be called on and the trap a host otherwise walks into
// (`releaseProvider(cached(seekable(p)).handle)` correctly refuses).
//
// A graph of Rust primitives has no leaves and disposing it does nothing, which
// is why this can be unconditional.
native.Provider.prototype[Symbol.dispose] = function dispose(this: Provider): void {
  for (const leaf of this.jsLeaves()) {
    try {
      releaseProvider(leaf);
    } catch {
      /* already released, or never JS-backed; disposal is not where that is reported */
    }
  }
};

// `using s = new Session('x')` — `close()` is what removes the staged launch
// directory and stops the ring, so it is the deterministic teardown and worth
// making structural. Idempotent on the Rust side.
native.Session.prototype[Symbol.dispose] = function dispose(this: native.Session): void {
  this.close();
};

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

/** Anything a primitive accepts where a provider is wanted. */
export type ProviderLike = Provider | ProviderWorker | number;

/** A `Provider`, a `ProviderWorker`, or a raw handle → the handle. */
function handleOf(p: unknown, what: string): number {
  if (typeof p === 'number' && Number.isInteger(p) && p >= 0) return p;
  if (p !== null && typeof p === 'object') {
    const h = (p as { handle?: unknown }).handle;
    if (typeof h === 'number') return h;
  }
  // A number that failed the test above gets its own message. The generic one
  // below ends in "got number", which reads as a type complaint to a caller who
  // did pass a number and leaves the actual problem — the *value* — unnamed. This
  // is the only class of argument here that Rust would otherwise coerce into a
  // different valid handle rather than reject.
  if (typeof p === 'number') {
    throw new TypeError(
      `aethervfs: ${what} needs a provider handle: a non-negative integer. Got ${p}. ` +
        'A handle is an index into a process-global registry, so a fractional, NaN, ' +
        'infinite or negative value is not an approximation of the handle you meant — ' +
        'coerced, it names a different live provider.'
    );
  }
  throw new TypeError(
    `aethervfs: ${what} needs a Provider (or its numeric handle); got ${describe(p)}. ` +
      'Every primitive returns one, and `providerWorker()` exposes `.provider`.'
  );
}

// Not exported: the hand-written declaration spelled this union out inline at
// every use, and naming it here without exporting it keeps the package's public
// type surface the same while writing it once. `tsc` emits the alias into
// `index.d.mts` as a local declaration.
type MemoryBytes = Buffer | Uint8Array | string;

/** `{ 'a.ini': bytes }` or `[{ path, bytes }]` → what Rust's `memory()` takes. */
function toMemoryFiles(
  files?: Record<string, MemoryBytes> | Array<{ path: string; bytes: MemoryBytes }> | null
): Array<{ path: string; bytes: Buffer }> {
  if (files === undefined || files === null) return [];
  const list: Array<[unknown, unknown]> = Array.isArray(files)
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
 * ```ts
 * // Seed it folded. The shim folds every vpath component before it crosses the
 * // ring, so the game's write to `Skyrim.ini` reaches this provider as
 * // `skyrim.ini` — and `memory()` is case-sensitive by design.
 * const inis = memory({ 'skyrim.ini': iniBytes });
 * session.mount(1, router({ '*.ini': inis }, base));
 * // ... the game writes Skyrim.ini ...
 * session.readFile('skyrim.ini', 1);   // folded name, and the root it is mounted on
 * ```
 *
 * **Both arguments of that last line are load-bearing**, and getting either
 * wrong returns plausible bytes rather than an error:
 *
 *  * the **name must be folded**, because a host-side read does not fold and
 *    this provider is case-sensitive — see `Session.readFile` and spec §6b.
 *    `session.readFile('Skyrim.ini', 1)` reads the *host's* seed, or nothing,
 *    while the game's write sits beside it under the folded name;
 *  * the **root must be the one it was mounted on**. `root` defaults to `0`, so
 *    omitting it on a provider mounted on root 1 reads a different graph
 *    entirely.
 *
 * Nothing touches disk, and it declares `readwrite`, so it also works as an
 * `overlay` upper or as the target of a `router` route for exactly the paths a
 * host wants writable.
 */
export function memory(
  files?: Record<string, MemoryBytes> | Array<{ path: string; bytes: MemoryBytes }>
): Provider {
  return native.memory(toMemoryFiles(files));
}

/**
 * Demote a provider to read-only (spec §6's `readonly`).
 *
 * The declaration becomes `read` — which is what the director consults, and
 * therefore what makes a refused write land in `session.rejectedWrites()` for
 * spec §7's discovery workflow — and every mutating call is refused with
 * `ST_READ_ONLY`.
 *
 * **This is the only way to get a non-empty `rejectedWrites()`.** `disk()` is
 * `readwrite`, so a graph built from `disk` alone can never refuse a write.
 */
export function readonly(provider: ProviderLike): Provider {
  return native.readonly(handleOf(provider, 'readonly(provider)'));
}

/**
 * Give a forward-only provider positional reads (spec §6's `seekable`):
 * `seqread` becomes `read`.
 *
 * A `seqread` provider that is not wrapped in this **cannot be mounted** —
 * `session.mount` refuses it, because the director reads with
 * `read_at(handle, offset, buf)` and a forward-only source has no answer for
 * one. Wrapping an already-positional provider is a no-op passthrough.
 */
export function seekable(provider: ProviderLike): Provider {
  return native.seekable(handleOf(provider, 'seekable(provider)'));
}

/**
 * Re-root a provider at one of its own subdirectories (spec §6's `subdir`):
 *
 * ```js
 * // A zipped game directory whose contents sit inside one top-level folder.
 * s.mount(0, subdir(zip(archive), 'Skyrim Special Edition'));
 * ```
 *
 * The opposite direction from `mount`'s `prefix`, which moves a provider *down*
 * so its content appears beneath that name. This moves the view *up*, discarding
 * a level the source has — the only one of the two that can flatten an archive
 * so the image at its root is reachable as `SkyrimSE.exe`.
 *
 * A prefix that names nothing is not an error: a provider may serve paths that
 * did not exist when the graph was built. Confirm with `session.getattr(...)`.
 */
export function subdir(provider: ProviderLike, prefix: string): Provider {
  return native.subdir(handleOf(provider, 'subdir(provider)'), prefix);
}

/**
 * A block cache in front of a provider (spec §6's `cached`). Access passes
 * through and `slow` is cleared. `provider.cacheStats()` reports its hits.
 */
export function cached(provider: ProviderLike, options?: CacheOptions): Provider {
  return native.cached(handleOf(provider, 'cached(provider)'), options ?? {});
}

/**
 * Stack providers so a **later** argument wins on a shared path (spec §6's
 * `layered`); `readdir` unions with the same rule per name.
 *
 * ```ts
 * layered(readonly(base), disk(modsDir))   // the mod wins over the vanilla file
 * ```
 *
 * Access is the *strongest* child's, not the weakest: every write routes to
 * whichever child declares `readwrite`. Accepts a spread or a single array,
 * because a host building the list programmatically has an array.
 */
export function layered(...providers: ProviderLike[]): Provider;
export function layered(providers: ProviderLike[]): Provider;
export function layered(...providers: Array<ProviderLike | ProviderLike[]>): Provider {
  const first = providers[0];
  const list: ProviderLike[] =
    providers.length === 1 && Array.isArray(first) ? first : (providers as ProviderLike[]);
  return native.layered(list.map((p, i) => handleOf(p, `layered() argument ${i}`)));
}

/**
 * Copy-up writes and whiteouts over a base (spec §6's `overlay`).
 *
 * Reports `readwrite` whatever `base` declares: a write to a path only `base`
 * holds copies the whole file into `upper` first, so an in-place edit of
 * read-only content succeeds instead of being refused. `upper` must declare
 * `readwrite` — checked here, not at the first write.
 */
export function overlay(base: ProviderLike, upper: ProviderLike): Provider {
  return native.overlay(
    handleOf(base, 'overlay(base, …)'),
    handleOf(upper, 'overlay(…, upper)')
  );
}

/**
 * Dispatch by glob, falling back to `defaultProvider` (spec §6's `router`).
 *
 * ```ts
 * router({ '*.ini': inis }, overlay(disk(docs), disk(scratch)))
 * ```
 *
 * First matching route wins; an object's insertion order is the match order,
 * which JS guarantees for string keys. Routes may also be an array of
 * `[pattern, provider]` pairs — or of `{ pattern, provider }` objects — for when
 * a pattern would be a duplicate key.
 *
 * `*` does not cross a `/`, so `'*.ini'` matches `Skyrim.ini` and not
 * `sub/Skyrim.ini` — use `'**\/*.ini'` for a subtree.
 *
 * **`readdir` is single-dispatch, not the union spec §6 specifies.** A file
 * served by a route is readable by name and invisible to a directory listing;
 * put it in the default provider if anything enumerates it. See the Rust doc
 * comment on `router` for what to do about it.
 */
export function router(
  routes:
    | Record<string, ProviderLike>
    | Array<[string, ProviderLike]>
    | Array<{ pattern: string; provider: ProviderLike }>,
  defaultProvider: ProviderLike
): Provider {
  const entries: Array<[unknown, unknown]> = Array.isArray(routes)
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
 * Run the workspace's conformance suite against a provider — **stage 4's gate.**
 *
 * Not a TypeScript reimplementation: this calls
 * `vfs_provider::assert_conformance`, the same function `DiskProvider`,
 * `MemoryProvider`, `LayeredProvider` and every other provider in the workspace
 * is held to in its own Rust tests. Cases are selected by the provider's
 * *declared* capabilities, so a `seqread` provider faces the sequential cases
 * (including "a positional read must be refused") and a `readwrite` one faces the
 * write cases as well.
 *
 * The provider must serve the reference tree — `conformanceFixture()` hands it
 * over, so a host holds no second copy of the contract.
 *
 * Composed providers work, which is how spec §10's own example reads from a host:
 * `await assertConformance(seekable(myStreamingProvider))`.
 *
 * **Rejects** with the failing case's message; the panic's file and line go to
 * stderr.
 *
 * The suite runs on a libuv pool thread, never on a JS loop, so this works for a
 * provider registered on the calling loop as well as one in a worker — `await` is
 * what leaves the servicing loop free to run the callbacks. Do not block the loop
 * while awaiting it.
 */
export function assertConformance(provider: ProviderLike): Promise<ConformanceReport> {
  return native.assertConformance(handleOf(provider, 'assertConformance(provider)'));
}

// ---------------------------------------------------------------------------
// The recommended shape: a provider on its own worker loop.
// ---------------------------------------------------------------------------

export interface ProviderWorkerSpec {
  /**
   * **Absolute path** to a CommonJS module — not an object. Isolates share no JS
   * objects, so a provider instance cannot be handed across one; what crosses is
   * the integer handle, which `mount()` accepts from any thread. Use
   * `require.resolve()`.
   *
   * The module may export the provider directly, or a factory called with
   * `options` — a factory is preferable, because it constructs the provider on
   * the loop its methods will run on.
   */
  module: string;
  /** Named export to use instead of `provider` / `default` / the module itself. */
  export?: string;
  /** Passed to the factory, structured-cloned into the worker. */
  options?: unknown;
  provider?: ProviderOptions;
}

/**
 * Load a provider module in a dedicated worker and register it there — the
 * recommended shape, and the only configuration §8c measured as immune to a busy
 * main loop (1449 MiB/s against 3.8 for a main-loop provider under ~1 ms of work
 * per turn). Concurrency scales with worker count and only with worker count.
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
export function providerWorker(spec: ProviderWorkerSpec): Promise<ProviderWorker> {
  const s: unknown = spec;
  if (s === null || typeof s !== 'object' || typeof (s as ProviderWorkerSpec).module !== 'string') {
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

  return new Promise<ProviderWorker>((resolve, reject) => {
    const worker = new WorkerCtor(path.join(import.meta.dirname, 'provider-host.mjs'), {
      workerData: {
        module: spec.module,
        export: spec.export ?? null,
        options: spec.options ?? {},
        providerOptions: spec.provider ?? {},
      },
    });
    let settled = false;
    let live: ProviderWorker | null = null;
    const fail = (e: Error): void => {
      if (settled) return;
      settled = true;
      worker.terminate().then(
        () => reject(e),
        () => reject(e)
      );
    };
    // `on`, not `once`, and it does something in both states.
    //
    // A worker that dies *after* this promise settles has nowhere to report: the
    // promise is spent, so there is nothing to reject, and this listener is the
    // only `'error'` handler on the worker — so node's "unhandled 'error' event"
    // path, which would at least be loud, never runs either. The early return that
    // used to be the whole post-settle branch therefore swallowed the death
    // completely. The provider is gone and every later call on its handle fails
    // with a released-loop status, so the host needs to hear about the cause and
    // this warning is the only place it can.
    worker.on('error', (e: Error) => {
      if (!settled) {
        fail(e);
        return;
      }
      const detail = e instanceof Error && e.stack ? e.stack : String(e);
      process.emitWarning(
        live === null
          ? // The registration already failed and `providerWorker()` rejected with
            // the first error, so that death is reported. This is a *second* one
            // arriving while the worker was being torn down.
            `aethervfs: the provider worker for ${spec.module} raised again while it was ` +
              `being torn down after a failed registration: ${detail}\n` +
              'The rejection from providerWorker() carries the original failure; this is ' +
              'the follow-on, reported rather than dropped.'
          : `aethervfs: the provider worker for ${spec.module} (handle ${live.handle}) raised ` +
              `after registering, and its loop is gone: ${detail}\n` +
              'Calls on that handle now fail with a status naming the released loop. This ' +
              'warning is the only report of it — the promise that would have rejected had ' +
              'already resolved by the time the worker died.',
        'AetherVfsProviderWorkerError'
      );
    });
    worker.once('exit', (code: number) => {
      if (!settled) {
        fail(new Error(`aethervfs: provider worker exited (code ${code}) before registering`));
      }
    });
    worker.once('message', (msg: unknown) => {
      if (settled) return;
      const m = msg as { ok?: unknown; handle?: number; stack?: string; message?: string } | null;
      if (!m || m.ok !== true) {
        const detail = m && m.stack ? m.stack : m && m.message ? m.message : String(msg);
        fail(
          new Error(`aethervfs: provider module ${spec.module} did not register:\n${detail}`)
        );
        return;
      }
      settled = true;
      live = new ProviderWorker(worker, m.handle as number);
      resolve(live);
    });
  });
}

/** The handle to a provider running on its own worker loop. */
export class ProviderWorker {
  readonly worker: WorkerCtor;
  readonly handle: number;
  readonly provider: Provider;

  /** Resolved once the worker's thread is gone, however it went. */
  private readonly _exit: Promise<void>;
  private _exited = false;
  /** The in-flight `close()`, so a second call awaits it rather than racing it. */
  private _closing: Promise<void> | null = null;

  constructor(worker: WorkerCtor, handle: number) {
    this.worker = worker;
    this.handle = handle;
    this.provider = native.Provider.fromHandle(handle);

    // **Subscribed here, not in `close()`.** `'exit'` is emitted exactly once, so a
    // listener registered after the fact waits for an event that can never come
    // again — which is what made `close()` hang forever on an already-exited
    // worker. Registering in the constructor is safe because node dispatches
    // worker events on the parent loop: the worker cannot have exited between
    // `new Worker(...)` and this line, which run in the same synchronous turn.
    this._exit = new Promise<void>((resolve) => {
      worker.once('exit', () => {
        this._exited = true;
        resolve();
      });
    });
  }

  /**
   * Is the worker's thread gone?
   *
   * `_exited` is the `'exit'` event *this object* saw. `threadId === -1` is node's
   * state for a thread that has ended, and it covers the case the listener cannot:
   * a worker wrapped after it had already exited, whose one `'exit'` event was
   * spent before this object existed.
   */
  private get hasExited(): boolean {
    return this._exited || this.worker.threadId === -1;
  }

  /** Counters for this provider — see `Provider.stats()`. */
  stats(): ProviderStats | null {
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
   *
   * **This always settles**, whatever state the worker is in — already exited,
   * terminated, or dead from an uncaught throw. It is the teardown `await using`
   * invokes, so a `close()` that hangs is indistinguishable from the leak it
   * exists to prevent and takes the host's whole teardown with it. Idempotent, and
   * safe to call concurrently: every caller awaits the same shutdown.
   */
  async close(): Promise<void> {
    // Memoized rather than flag-guarded. A bare `if (this._closed) return` handed
    // the second caller an already-resolved promise, so it was told teardown was
    // done while the worker was still coming down.
    this._closing ??= this.shutdown();
    await this._closing;
  }

  private async shutdown(): Promise<void> {
    // Nothing to release and nothing to wait for. Without this, the wait below
    // would depend on an `'exit'` that has already been emitted.
    if (this.hasExited) return;

    // Guarded because the result is memoized: a throw here would reject the
    // promise every present *and future* `close()` awaits, turning one bad send
    // into permanently unusable teardown. Node makes `postMessage` to a worker
    // that has gone a no-op rather than an error, so this is a belt for a race —
    // the thread ending between the check above and this line — and the backstop
    // below is what settles the wait if the release never lands.
    try {
      this.worker.postMessage({ type: 'release' });
    } catch {
      /* the worker is gone or its port is closed; the backstop covers it */
    }

    let timer: NodeJS.Timeout | undefined;
    // The backstop terminates a worker that ignored the message — and, unlike
    // before, *also settles this promise when it does*. `terminate()` resolving is
    // itself proof the thread is gone, so this holds even if `_exit` were somehow
    // never to fire, which is the last way `close()` could hang.
    const backstop = new Promise<void>((resolve) => {
      const t = setTimeout(() => {
        this.worker.terminate().then(
          () => resolve(),
          () => resolve()
        );
      }, 2000);
      // Node keeps the process alive for a pending timer; this one is only a
      // backstop for a worker that ignored the message.
      t.unref?.();
      timer = t;
    });

    try {
      await Promise.race([this._exit, backstop]);
    } finally {
      clearTimeout(timer);
    }
  }

  /**
   * `await using w = await providerWorker({...})` — the worker is released and
   * stopped when the block ends, however it ends.
   *
   * This is the one place in the API where forgetting the teardown does not
   * throw, log, or fail a test: it hangs the process with no diagnostic, because
   * the thing that would report it is the loop draining. Node 22.6+ can make it
   * structural, so it is made structural.
   */
  async [Symbol.asyncDispose](): Promise<void> {
    await this.close();
  }
}

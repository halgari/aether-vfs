// Hand-written, deliberately.
//
// `@napi-rs/cli` generates this file from the `#[napi]` attributes, and using it
// would mean an npm dependency — a network install — in the build path of a
// package whose whole build is otherwise four `cargo` calls and four file
// copies. The surface is small enough that the trade is not worth it yet. If it
// grows, generate it; a drifting hand-written .d.ts is worse than a dependency.

// ---------------------------------------------------------------------------
// **How an absent optional reads in JavaScript**, because the two directions
// differ and getting it wrong produces a check that silently never matches.
//
//  * A **return value** of Rust `Option<T>` arrives as `null`
//    (`provider.stats()`, `provider.cacheStats()`, `provider.kind`).
//  * An **object field** of `Option<T>` has its key **omitted**, so it reads as
//    `undefined` (`ProviderStats.callTimeoutMs`, `ConformanceReport.providerCalls`,
//    `ProviderCapabilities.preferredBlock`).
//
// Optional *fields* below are therefore written `name?: T` and optional
// *returns* `T | null`. `x === null` on a field will not match; task 7 lost time
// to exactly that with `callTimeoutMs`.
// ---------------------------------------------------------------------------

/** An opaque handle to a provider living in Rust. */
export class Provider {
  /**
   * Rebuild a wrapper from a handle, validating that the handle exists.
   *
   * Handles are **process-global integers**, not per-isolate references: Rust
   * statics are shared by every isolate that loads the addon, while no JS
   * object crosses an isolate boundary. So a worker that creates a provider
   * passes `provider.handle` to the main thread, and the main thread calls this.
   */
  static fromHandle(handle: number): Provider;
  /** The process-global integer this wrapper stands for. */
  get handle(): number;
  /**
   * Counters and configuration for a JS-authored provider; `null` for a Rust
   * one, which has no bridge and nothing to report.
   */
  stats(): ProviderStats | null;
  /**
   * What this provider **declares** it can do — the same `Capabilities` the
   * director reads before it issues a call. This is how spec §6's capability
   * recomputation rules become checkable from a host.
   */
  capabilities(): ProviderCapabilities;
  /**
   * Which primitive made this handle: `'disk'`, `'memory'`, `'js'`,
   * `'readonly'`, `'seekable'`, `'cached'`, `'layered'`, `'overlay'`,
   * `'router'`.
   */
  get kind(): string | null;
  /** The handles this provider was composed from, in argument order. */
  get children(): number[];
  /**
   * Every JS-authored provider reachable through this composition — **the list
   * `releaseProvider` has to be called on.**
   *
   * A live threadsafe function keeps its loop alive, so a worker never exits
   * until its provider is released; wrapping that provider in
   * `cached(seekable(...))` produces new handles, none of which is the one to
   * release. `releaseProvider(composed.handle)` correctly refuses, and this is
   * how to find what to call it with instead. `[]` for a graph of Rust
   * primitives.
   */
  jsLeaves(): number[];
  /**
   * Block-cache counters for a handle `cached()` produced; `null` for anything
   * else. Without it, "I put a cache in the graph" is not verifiable.
   */
  cacheStats(): ProviderCacheStats | null;
}

/** A provider's declared capabilities. */
export interface ProviderCapabilities {
  access: 'seqread' | 'read' | 'readwrite';
  /** Content never changes for the provider's lifetime. */
  immutable: boolean;
  /**
   * Reads are expensive and this provider should sit behind `cached`. `cached()`
   * clears it, which is what makes `mount`'s warning exact rather than a guess.
   */
  slow: boolean;
  preferredBlock?: number;
}

/** Block-cache counters, as `provider.cacheStats()` reports them. */
export interface ProviderCacheStats {
  hits: number;
  misses: number;
  ramEvicts: number;
  diskHits: number;
  diskWrites: number;
  bytesFromCache: number;
  bytesFromSource: number;
  ramBytes: number;
  ramBlocks: number;
  /**
   * The block size actually in use, after the wrapped provider's
   * `preferredBlock` and the [4 KiB, 4 MiB] clamp.
   */
  blockSize: number;
}

/**
 * A read-write provider over a real directory (spec §6's `disk` primitive).
 *
 * Throws if `path` is not an existing directory: a provider over a missing path
 * answers `ST_NOT_FOUND` for everything without reporting an error, so a typo
 * would produce a session that silently serves nothing.
 */
export function disk(path: string): Provider;

// ---------------------------------------------------------------------------
// Spec §6's primitive catalog. Everything here is a Rust type; a host writes
// none of them and composes all of them. `Provider | number` everywhere, because
// what actually crosses an isolate boundary is the integer.
// ---------------------------------------------------------------------------

/** Anything a primitive accepts where a provider is wanted. */
export type ProviderLike = Provider | ProviderWorker | number;

/**
 * A read-write in-memory file tree (spec §6's `memory`).
 *
 * ```ts
 * const inis = memory({ 'Skyrim.ini': iniBytes });
 * session.mount(1, router({ '*.ini': inis }, base));
 * // ... the game writes Skyrim.ini ...
 * session.readFile('Skyrim.ini');   // what the game wrote, never on disk
 * ```
 *
 * Declares `readwrite`, so it also works as an `overlay` upper.
 */
export function memory(
  files?: Record<string, Buffer | Uint8Array | string> | Array<{ path: string; bytes: Buffer | Uint8Array | string }>
): Provider;

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
export function readonly(provider: ProviderLike): Provider;

/**
 * Give a forward-only provider positional reads (spec §6's `seekable`):
 * `seqread` becomes `read`.
 *
 * A `seqread` provider that is not wrapped in this **cannot be mounted** —
 * `session.mount` refuses it, because the director reads with
 * `read_at(handle, offset, buf)` and a forward-only source has no answer for
 * one. Wrapping an already-positional provider is a no-op passthrough.
 */
export function seekable(provider: ProviderLike): Provider;

/** Options for {@link cached}. */
export interface CacheOptions {
  /** RAM budget for block payloads, in bytes. Default 64 MiB. */
  ramBytes?: number;
  /**
   * Block size in bytes. Default 1 MiB, and **overridden** by the wrapped
   * provider's own `preferredBlock` when it declares one (clamped to
   * [4 KiB, 4 MiB]) — a source that states its natural unit knows better than
   * its caller.
   */
  blockSize?: number;
  /**
   * Directory for the on-disk block tier. Unset means RAM only. Only worth
   * setting for a provider declaring **both** `immutable` and `slow`.
   */
  diskDir?: string;
}

/**
 * A block cache in front of a provider (spec §6's `cached`). Access passes
 * through and `slow` is cleared. `provider.cacheStats()` reports its hits.
 */
export function cached(provider: ProviderLike, options?: CacheOptions): Provider;

/**
 * Stack providers so a **later** argument wins on a shared path (spec §6's
 * `layered`); `readdir` unions with the same rule per name.
 *
 * ```ts
 * layered(readonly(base), disk(modsDir))   // the mod wins over the vanilla file
 * ```
 *
 * Access is the *strongest* child's, not the weakest: every write routes to
 * whichever child declares `readwrite`. Accepts a spread or one array.
 */
export function layered(...providers: ProviderLike[]): Provider;
export function layered(providers: ProviderLike[]): Provider;

/**
 * Copy-up writes and whiteouts over a base (spec §6's `overlay`).
 *
 * Reports `readwrite` whatever `base` declares: a write to a path only `base`
 * holds copies the whole file into `upper` first, so an in-place edit of
 * read-only content succeeds instead of being refused. `upper` must declare
 * `readwrite` — checked here, not at the first write.
 */
export function overlay(base: ProviderLike, upper: ProviderLike): Provider;

/**
 * Dispatch by glob, falling back to `defaultProvider` (spec §6's `router`).
 *
 * ```ts
 * router({ '*.ini': inis }, overlay(disk(docs), disk(scratch)))
 * ```
 *
 * First matching route wins; an object's insertion order is the match order.
 * `*` does not cross a `/`, so `'*.ini'` matches `Skyrim.ini` and not
 * `sub/Skyrim.ini` — use `'**\/*.ini'` for a subtree.
 *
 * **`readdir` is single-dispatch, not the union spec §6 specifies.** A file
 * served by a route is readable by name and invisible to a directory listing;
 * put it in the default provider if anything enumerates it.
 */
export function router(
  routes: Record<string, ProviderLike> | Array<[string, ProviderLike]> | Array<{ pattern: string; provider: ProviderLike }>,
  defaultProvider: ProviderLike
): Provider;

// ---------------------------------------------------------------------------
// The conformance gate.
// ---------------------------------------------------------------------------

/** What a passing conformance run reports. */
export interface ConformanceReport {
  handle: number;
  /** The provider's `kind`, as `provider.kind` reports it. */
  kind?: string;
  /** What the provider declared, and therefore which cases it was held to. */
  access: 'seqread' | 'read' | 'readwrite';
  immutable: boolean;
  slow: boolean;
  preferredBlock?: number;
  /**
   * The case groups that ran: `'common'`, then `'sequential'` or `'positional'`,
   * plus `'writable'` for a `readwrite` provider.
   */
  cases: string[];
  /**
   * Provider calls that crossed the bridge during the run; `null` for a Rust
   * provider. **This is the number that says the suite did work** — a JS
   * provider that passed with `0` was skipped, not tested.
   */
  providerCalls?: number;
  durationMs: number;
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
 * The provider must serve the reference tree — {@link conformanceFixture} hands
 * it over, so a host holds no second copy of the contract.
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
export function assertConformance(provider: ProviderLike): Promise<ConformanceReport>;

/** One file of the reference tree a conformance-tested provider must serve. */
export interface FixtureFile {
  path: string;
  bytes: Buffer;
}

/**
 * The reference tree, from the same constant Rust reads. Hard-coding it in
 * JavaScript would put a second copy of the contract somewhere nothing keeps in
 * step with the first.
 */
export function conformanceFixture(): FixtureFile[];

/**
 * Write the reference tree into a real directory, for
 * `assertConformance(disk(dir))`. **Clears `dir` first** — a leftover file would
 * show up in `readdir` of the root and fail the suite for a reason unrelated to
 * the provider.
 */
export function writeConformanceFixture(dir: string): void;

/** One declared root, as `session.roots()` reports it. */
export interface RootInfo {
  id: number;
  /** The host's label. Diagnostics only — the director addresses roots by id. */
  name: string;
  path: string;
}

/** One refused write, as `session.rejectedWrites()` reports it. */
export interface RejectedWrite {
  path: string;
  count: number;
}

/**
 * Which DLLs a launch would use, with size and mtime.
 *
 * Size and mtime, *not* an identity check: spec §8's packaging section asks a
 * binding to verify a build hash embedded in each DLL, and no such hash exists
 * in the workspace yet. This is enough to catch a stale DLL that survived a
 * rebuild and not enough to catch two different DLLs of the same size.
 */
export interface ShimInfo {
  shimDll: string;
  payloadDll: string;
  shimSize: number;
  /** Unix epoch milliseconds, or 0 if unavailable. */
  shimModifiedMs: number;
  payloadSize: number;
  payloadModifiedMs: number;
}

export interface LaunchOptions {
  args?: string[];
  /**
   * Wait for the child to exit (default `true`). With `false` the session must
   * outlive the child — it owns the ring and the staged image.
   */
  wait?: boolean;
  /** Override the shim DLL for this launch. */
  shimDll?: string;
  /** Override the payload DLL. Unset with `shimDll` given, it is looked for beside that shim. */
  payloadDll?: string;
  /** Extra images to stage beside a graph-resolved image, by vpath. */
  stageAlso?: string[];
  /** Real-disk directories searched for imports the provider graph does not carry. */
  stageFallbackDirs?: string[];
  /** Extra environment variables for the child only. */
  env?: Record<string, string>;
}

/** One VFS session: roots, the graph each root serves, the ring, and the launch. */
export class Session {
  /**
   * `name` is a label. It appears in `session.name` and in the session's
   * directory names under `%TEMP%`, so a developer can tell which session left
   * what behind.
   */
  constructor(name: string);

  get name(): string;
  /** The tree holding this session's root, overlay and state directories. */
  get baseDir(): string;
  /** Root 0's managed directory — what the injected child recognises as the virtual root. */
  get virtualRoot(): string;
  get stateDir(): string;

  /**
   * Where root `root`'s shim-local overlay writes land on disk. Root-scoped, so
   * this is *not* `baseDir/overlay`.
   */
  overlayLayerDir(root: number): string;

  /**
   * Declare that root `id` virtualizes the host directory `path`. Id 0 repoints
   * `virtualRoot`.
   *
   * Declaring is not mounting. Declare without mounting and the root serves
   * nothing; mount without declaring and the child never classifies any path
   * into that root, so every path under it falls through to real disk —
   * silently.
   */
  addRoot(id: number, name: string, path: string): void;
  roots(): RootInfo[];

  /**
   * Mount `provider` on root `root`, optionally under `prefix` within it.
   * Accumulates; later mounts win on a path both serve.
   */
  mount(root: number, provider: Provider, prefix?: string): void;

  /** Start the ring so an injected child can remap I/O. Idempotent; `launch` calls it. */
  serve(): void;
  isServing(): boolean;

  /**
   * Read a whole file out of a root's graph, host-side; `root` defaults to 0.
   *
   * This is how a host reads back what a launched child wrote into a
   * `memory()` provider — spec §8's last line, which mounts the INIs on root 1.
   *
   * **A path given here is not case-folded.** The shim folds every vpath
   * component before it crosses the ring, so a child's write to `Skyrim.ini`
   * reaches the provider as `skyrim.ini`; a host-side read does not fold, and
   * `memory()` is case-sensitive. Until spec §6's `casefold` primitive exists, a
   * host that wants to read a child's writes back out of `memory()` must fold
   * its own keys — see `examples/spec-8-example.cts`, which demonstrates both
   * the working round trip and the silent wrong answer.
   */
  readFile(vpath: string, root?: number): Buffer;

  /**
   * Point this session at a specific shim (and optionally payload) DLL, for a
   * host running against a dev build rather than the packaged DLLs.
   */
  setShimDlls(shimDll: string, payloadDll?: string): void;

  /** Which DLLs a launch would use right now. Throws, naming every candidate, if not found. */
  shimInfo(): ShimInfo;

  /**
   * Launch `exe` under the virtual root with the shim injected; returns the
   * child's exit code. Serves first if not already serving.
   *
   * An absolute `exe` is launched as given. A relative one is looked for as a
   * real file under the managed root, then as a vpath in root 0's graph — in
   * which case it is staged out with its PE import closure and that copy is
   * launched, with the staging directory mounted back into the graph below
   * everything the host mounted.
   *
   * **Blocking** with `wait: true`: it occupies the calling JS thread for the
   * child's lifetime. Call it from a worker in an Electron main process.
   */
  launch(exe: string, options?: LaunchOptions): number;

  /**
   * Every write refused because no read-write provider served that path — spec
   * §7's discovery workflow. **Process-wide**, not per session: the director
   * keeps one global table with no session dimension.
   */
  rejectedWrites(): RejectedWrite[];
  /** Clear rejected-write tracking. Process-wide, same caveat. */
  resetRejectedWrites(): void;
  /** Opens that reached the director, as `[succeeded, failed]`. Process-wide. */
  openTotals(): number[];

  /**
   * Stop serving and drop the session. Dropping is what removes the staged
   * launch directory, so this is the deterministic teardown. Idempotent;
   * further calls on a closed session throw. The session's directories are
   * left in place for inspection.
   */
  close(): void;
}

/** Record the directory the addon was loaded from. `index.cjs` calls this with `__dirname`. */
export function setPackageDir(dir: string): void;
export function packageDir(): string | null;
/** The `ST_*` status codes, so a caller can compare against a name. */
export function statusCodes(): Record<string, number>;
/** The addon's version. A loaded-and-answering check for `require('aethervfs')`. */
export function version(): string;

// ===========================================================================
// JS-authored providers — spec §8b, as corrected by measurement in §8c.
// ===========================================================================

/** `{ ST_OK: 0, ST_NOT_FOUND: -2, ... }`, read once from Rust at load. */
export const STATUS: Record<string, number>;
/** `{ getattr: 1, readdir: 2, ... }` — the op integers a `CallRequest` carries. */
export const OP: Record<string, number>;
/** `{ KIND_FILE: 1, KIND_DIR: 2, KIND_TOMBSTONE: 3 }`. */
export const KIND: Record<string, number>;
/** `{ OPEN_READ, OPEN_WRITE, OPEN_CREATE, OPEN_TRUNC, OPEN_APPEND, OPEN_EXCL }`. */
export const OPEN: Record<string, number>;

/** The `ST_*` name for a status, or `"status <n>"` if it is not one. */
export function statusName(code: number): string;

/**
 * Fail a provider call with a specific `ST_*` status.
 *
 * Anything else thrown or rejected from a provider method becomes
 * `ST_IO_ERROR`, with the stack logged and counted — spec §8b rule 3. A
 * `VfsError` whose code is not a status this workspace defines (or is `ST_OK`)
 * is clamped to `ST_IO_ERROR` on the Rust side, so a host cannot invent a code.
 */
export class VfsError extends Error {
  constructor(code: number | string, message?: string);
  /** The `ST_*` number. */
  readonly code: number;
  /** The same number, duck-typed so it survives crossing a realm. */
  readonly vfsStatus: number;
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
  open(root: number, path: string, flags: number): ProviderOpenResult | Promise<ProviderOpenResult>;
  close(handle: number): void | Promise<void>;

  /** Positional read. A short read is legal anywhere, not only at EOF. */
  readAt?(handle: number, offset: number, length: number): Uint8Array | string | Promise<Uint8Array | string>;
  /** Forward-only read, for `access: 'seqread'`. */
  readNext?(handle: number, length: number): Uint8Array | string | Promise<Uint8Array | string>;
  /** Must return the number of bytes written; returning nothing is refused rather than guessed. */
  writeAt?(handle: number, offset: number, data: Buffer): number | Promise<number>;
  setLen?(handle: number, length: number): void | Promise<void>;
  flush?(handle: number): void | Promise<void>;
  mkdir?(root: number, path: string): void | Promise<void>;
  remove?(root: number, path: string): void | Promise<void>;
  rename?(fromRoot: number, fromPath: string, toRoot: number, toPath: string): void | Promise<void>;
  setAttr?(
    root: number,
    path: string,
    attr: { mtime: number | null; size: number | null }
  ): void | Promise<void>;
}

export interface ProviderOptions {
  /**
   * Abandon a call that has not settled after this long: the director thread is
   * released with `ST_IO_ERROR` and `abandonedCalls` counts it.
   *
   * Unset is the default and is the contract: a provider that never settles
   * hangs *one director thread*, not the session. The hang is still diagnosable
   * without this — `stallWarnMs` counts and logs it.
   */
  callTimeoutMs?: number;
  /** Count and log a call still outstanding after this long. Default 5000. */
  stallWarnMs?: number;
}

export interface ProviderStats {
  handle: number;
  access: 'read' | 'readwrite' | 'seqread';
  immutable: boolean;
  slow: boolean;
  preferredBlock?: number;
  /** The methods the object was found to have at registration. */
  methods: string[];
  /** The event loop that services this provider, as the deadlock guard names it. */
  ownerThread: string;
  released: boolean;
  callTimeoutMs?: number;
  stallWarnMs: number;
  calls: number;
  settledCalls: number;
  /** Calls that came back as a deliberate status. */
  vfsErrors: number;
  /** Calls where the host threw something that was not a `VfsError`. */
  hostErrors: number;
  /** Calls still outstanding when `stallWarnMs` passed — where a hang is counted. */
  stalledCalls: number;
  /** Calls given up on because `callTimeoutMs` expired. */
  abandonedCalls: number;
  /** Calls the deadlock guard refused. */
  selfCallRefusals: number;
  /** Calls that could not be queued — a released or dead loop. */
  dispatchFailures: number;
  lastHostError?: string;
  lastDiagnostic?: string;
}

/**
 * Mount a JS object as a provider, serviced by the **calling** thread's event
 * loop.
 *
 * That loop is the one thread that may not drive a session mounting this
 * provider: a blocking provider call issued on the loop that services it can
 * never settle, because the loop cannot run the callback while parked. The guard
 * refuses that with an explanation rather than hanging, but the way to not need
 * it is `providerWorker()`.
 */
export function registerProvider(obj: ProviderObject, options?: ProviderOptions): Provider;

/**
 * Release a JS provider's event loop, so the worker holding it can exit. Calls
 * afterwards fail with a status naming the released loop; a director thread
 * already parked on it is woken. The registry entry stays, because a handle is
 * process-global by design.
 */
export function releaseProvider(handle: number): void;

/** Provider calls outstanding across the whole process. */
export function outstandingProviderCalls(): number;

/** The op integers, keyed by JS method name. */
export function providerOps(): Record<string, number>;
export function kinds(): Record<string, number>;
export function openFlags(): Record<string, number>;

export interface ProviderWorkerSpec {
  /**
   * **Absolute path** to a CommonJS module — not an object. Isolates share no JS
   * objects, so a provider instance cannot be handed across one; what crosses is
   * the integer handle. Use `require.resolve()`.
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

/** A provider running on its own worker loop. */
export class ProviderWorker {
  readonly worker: import('worker_threads').Worker;
  readonly handle: number;
  readonly provider: Provider;
  stats(): ProviderStats | null;
  /** Release the loop and stop the worker. Without it the worker never exits. */
  close(): Promise<void>;
}

/**
 * Load a provider module in a dedicated worker and register it there — the
 * recommended shape, and the only configuration §8c measured as immune to a busy
 * main loop (1449 MiB/s against 3.8 for a main-loop provider under ~1 ms of work
 * per turn). Concurrency scales with worker count and only with worker count.
 */
export function providerWorker(spec: ProviderWorkerSpec): Promise<ProviderWorker>;

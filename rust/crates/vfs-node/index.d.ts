// Hand-written, deliberately.
//
// `@napi-rs/cli` generates this file from the `#[napi]` attributes, and using it
// would mean an npm dependency — a network install — in the build path of a
// package whose whole build is otherwise four `cargo` calls and four file
// copies. The surface is small enough that the trade is not worth it yet. If it
// grows, generate it; a drifting hand-written .d.ts is worse than a dependency.

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
}

/**
 * A read-write provider over a real directory (spec §6's `disk` primitive).
 *
 * Throws if `path` is not an existing directory: a provider over a missing path
 * answers `ST_NOT_FOUND` for everything without reporting an error, so a typo
 * would produce a session that silently serves nothing.
 */
export function disk(path: string): Provider;

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

  /** Read a whole file out of root 0's graph, host-side. */
  readFile(vpath: string): Buffer;

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
  preferredBlock: number | null;
  /** The methods the object was found to have at registration. */
  methods: string[];
  /** The event loop that services this provider, as the deadlock guard names it. */
  ownerThread: string;
  released: boolean;
  callTimeoutMs: number | null;
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
  lastHostError: string | null;
  lastDiagnostic: string | null;
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

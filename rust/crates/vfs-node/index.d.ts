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

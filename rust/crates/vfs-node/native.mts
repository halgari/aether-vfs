// The addon's `#[napi]` surface, and the one place that loads it.
//
// **This file is the irreducible hand-written part of the declaration.** Every
// other `.d.mts` in this package is a `tsc` output; this one describes a Rust
// binary, and nothing in the JavaScript toolchain can derive it. `@napi-rs/cli`
// could — from the `#[napi]` attributes themselves — and using it would put a
// network npm install into the build path of a package whose build is four
// `cargo` calls and four file copies. That trade is still refused, so the
// declaration below is maintained by hand and checked by
// `scripts/check-types.mts` against the addon's real exports.
//
// The difference from before the TypeScript migration is *how much* is
// hand-written. `index.d.ts` used to declare the whole package — the addon and
// the JavaScript layer wrapped around it — while `index.cjs` implemented the
// second half in untyped JavaScript. The two had to agree by discipline and did
// not. Now the JavaScript layer's declaration is emitted from the code that
// implements it, and only this file is still asserted rather than derived.
//
// ## Why `fn()` instead of one cast of the whole module
//
// The addon is a `.node` binary: TypeScript can say nothing about it, so some
// assertion has to happen. `fn<T>(name)` makes each one *narrow and checked* —
// it reads exactly one property, verifies at load that it is a function, and
// names the addon path in the error if it is not. A single
// `require(addonPath) as Addon` would type-check identically and tell you
// nothing when a `#[napi]` export is renamed; this fails on the first load with
// the missing name in the message.
//
// ## Absent optionals: `undefined` for a field, `null` for a return
//
// The two directions differ, and getting it wrong produces a check that
// silently never matches:
//
//  * a **return value** of Rust `Option<T>` arrives as `null` — `provider.stats()`,
//    `provider.cacheStats()`, `provider.kind`;
//  * an **object field** of `Option<T>` has its key **omitted**, so it reads as
//    `undefined` — `ProviderStats.callTimeoutMs`, `ConformanceReport.providerCalls`,
//    `ProviderCapabilities.preferredBlock`.
//
// Optional *fields* below are therefore written `name?: T` and optional
// *returns* `T | null`. `x === null` on a field will not match; task 7 lost time
// to exactly that with `callTimeoutMs`. Verified rather than assumed — see the
// task 2 report, which prints `'providerCalls' in report === false`.
//
// ## Node version
//
// `package.json` says `>=24`, and that number is about the *package*, not the
// addon. Three floors, lowest first:
//
//   * **18** — load and use the addon. `require('aethervfs')` is N-API 8.
//   * **22.6** — run the `.cts` sources and test files directly under node's
//     type stripping.
//   * **24** — the `using` declarations in `test/dispose.test.mts` (Explicit
//     Resource Management syntax). The disposables themselves work anywhere
//     `Symbol.dispose` exists; only writing `using` needs the syntax.
//
// `engines` is the highest, because it describes this package as it stands, with
// its own scripts and tests. A consumer that only loads the addon can relax it.

import { createRequire } from 'node:module';
import * as fs from 'node:fs';
import * as path from 'node:path';

// The addon is a `.node` binary and can only be loaded by `require`; ESM has
// no global `require`, so this file makes its own.
const require = createRequire(import.meta.url);
const addonDir = import.meta.dirname;
const addonPath = path.join(addonDir, 'aethervfs.node');

if (!fs.existsSync(addonPath)) {
  throw new Error(
    `aethervfs: native addon not found at ${addonPath}. ` +
      'Build it with `pnpm build` (or `pnpm build:release`) in ' +
      `${addonDir}. That builds the addon, the shim DLL and the ` +
      'separate-workspace payload DLL, and places all three here.'
  );
}

const addon = require(addonPath) as Record<string, unknown>;

/**
 * One export off the addon, asserted to be there.
 *
 * The assertion is the point: a renamed or dropped `#[napi]` export becomes a
 * `TypeError` naming the addon and the missing name, at load, instead of an
 * `undefined is not a function` at the first call.
 */
function fn<T>(name: string): T {
  const v = addon[name];
  if (typeof v !== 'function') {
    throw new TypeError(
      `aethervfs: the addon at ${addonPath} has no \`${name}\`; it is ` +
        `${v === undefined ? 'absent' : typeof v}. The addon and this package's ` +
        'JavaScript are built together — a stale `aethervfs.node` is the usual ' +
        'cause. Run `pnpm build`.'
    );
  }
  return v as T;
}

// ---------------------------------------------------------------------------
// Provider.
// ---------------------------------------------------------------------------

/** An opaque handle to a provider living in Rust. */
export interface Provider {
  /** The process-global integer this wrapper stands for. */
  readonly handle: number;
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
  readonly kind: string | null;
  /** The handles this provider was composed from, in argument order. */
  readonly children: number[];
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
  /**
   * `using p = registerProvider(obj)` — releases every JS provider reachable
   * through this handle when the block ends.
   *
   * Deliberately defined on every `Provider` and not only on a bare
   * registration: it disposes {@link Provider.jsLeaves}, which is exactly the
   * list `releaseProvider` has to be called on, so it also does the right thing
   * for `cached(seekable(myProvider))` — where `releaseProvider(composed.handle)`
   * correctly refuses and a host has to know to look for the leaves. A graph of
   * Rust primitives has no leaves and disposing it does nothing.
   *
   * Installed by `index.mts`, not by Rust.
   */
  [Symbol.dispose](): void;
}

/**
 * The `Provider` constructor object.
 *
 * **It has no `new`**, and that is not an omission. `Provider::from_handle` is
 * `#[napi(factory)]`, which napi-derive exposes as a static and which leaves the
 * class with no JS constructor at all: `new Provider()` throws
 * *"Class contains no `constructor`, can not new it!"*. The declaration this
 * replaced wrote `export class Provider` with no constructor listed, which in
 * TypeScript means an implicit zero-argument one — so `new Provider()`
 * type-checked and threw at runtime.
 */
interface ProviderConstructor {
  /**
   * Rebuild a wrapper from a handle, validating that the handle exists.
   *
   * Handles are **process-global integers**, not per-isolate references: Rust
   * statics are shared by every isolate that loads the addon, while no JS
   * object crosses an isolate boundary. So a worker that creates a provider
   * passes `provider.handle` to the main thread, and the main thread calls this.
   */
  fromHandle(handle: number): Provider;
  readonly prototype: Provider;
}

export const Provider: ProviderConstructor = fn('Provider');

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
  /**
   * **Absent, not `null`,** when the provider declares no hint. An `Option<T>`
   * *field* of an `#[napi(object)]` has its key omitted, so it reads as
   * `undefined`; only an `Option<T>` *return* arrives as `null`.
   * `preferredBlock === null` is a check that never matches.
   */
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
export const disk: (path: string) => Provider = fn('disk');

/**
 * A read-only provider over a **Stored** zip archive (spec §6's `zip`).
 *
 * Serves entries at their stored offsets — no extraction and no second copy of
 * the archive's contents on disk. Throws if `path` is not a file, or if the
 * central directory cannot be read.
 *
 * Opening parses the entire central directory eagerly, which on a multi-gigabyte
 * archive takes long enough to look like a hang. Open once and keep the
 * provider.
 *
 * Not shadowed in `index.mts`: it takes a path, so there is no handle to widen.
 */
export const zip: (path: string) => Provider = fn('zip');

// ---------------------------------------------------------------------------
// Spec §6's primitive catalog, as Rust exports it: **integer handles**, because
// a handle is the only thing that means the same in two isolates. `index.mts`
// shadows every name below with a wrapper that also accepts a `Provider`; these
// are the primitives underneath.
// ---------------------------------------------------------------------------

/** One seed file for {@link memory}. */
interface MemoryFile {
  /** A vpath. Case-sensitive — see `index.mts`'s `memory()`. */
  path: string;
  bytes: Buffer;
}

/** The handle-taking `memory`. `index.mts` shadows it with the object form. */
export const memory: (files?: MemoryFile[]) => Provider = fn('memory');

/** The handle-taking `readonly`. `index.mts` shadows it. */
export const readonly: (provider: number) => Provider = fn('readonly');

/** The handle-taking `seekable`. `index.mts` shadows it. */
export const seekable: (provider: number) => Provider = fn('seekable');

/** The handle-taking `subdir`. `index.mts` shadows it. */
export const subdir: (provider: number, prefix: string) => Provider = fn('subdir');

/** Options for `cached`. */
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

/** The handle-taking `cached`. `index.mts` shadows it. */
export const cached: (provider: number, options?: CacheOptions) => Provider = fn('cached');

/** The handle-taking `layered`. `index.mts` shadows it with the spread form. */
export const layered: (providers: number[]) => Provider = fn('layered');

/** The handle-taking `overlay`. `index.mts` shadows it. */
export const overlay: (base: number, upper: number) => Provider = fn('overlay');

/** One route for {@link router}. */
interface RouteSpec {
  pattern: string;
  provider: number;
}

/** The handle-taking `router`. `index.mts` shadows it with the object form. */
export const router: (routes: RouteSpec[], defaultProvider: number) => Provider = fn('router');

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
   * Provider calls that crossed the bridge during the run. **This is the number
   * that says the suite did work** — a JS provider that passed with `0` was
   * skipped, not tested.
   *
   * **Absent** for a Rust provider, which has no bridge — so it reads as
   * `undefined`, not `null`. An `Option<T>` *field* of an `#[napi(object)]` has
   * its key omitted; only an `Option<T>` *return* — `Provider.stats()`,
   * `Provider.cacheStats()`, `Session.getattr()` — arrives as `null`. The two
   * directions differ, and `providerCalls === null` is a check that never
   * matches. Measured, not assumed: `'providerCalls' in report` is `false`.
   */
  providerCalls?: number;
  durationMs: number;
}

/** The handle-taking `assertConformance`. `index.mts` shadows it. */
export const assertConformance: (provider: number) => Promise<ConformanceReport> =
  fn('assertConformance');

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
export const conformanceFixture: () => FixtureFile[] = fn('conformanceFixture');

/**
 * Write the reference tree into a real directory, for
 * `assertConformance(disk(dir))`. **Clears `dir` first** — a leftover file would
 * show up in `readdir` of the root and fail the suite for a reason unrelated to
 * the provider.
 */
export const writeConformanceFixture: (dir: string) => void = fn('writeConformanceFixture');

// ---------------------------------------------------------------------------
// Session.
// ---------------------------------------------------------------------------

/** One declared root, as `session.roots()` reports it. */
export interface RootInfo {
  id: number;
  /** The host's label. Diagnostics only — the director addresses roots by id. */
  name: string;
  path: string;
}

/**
 * One entry from `session.readdir()`.
 *
 * `kind` is a number from `KIND` (`KIND_FILE`, `KIND_DIR`, `KIND_TOMBSTONE`) —
 * not the string a *provider* may return from its own `getattr`. The director
 * resolves that before it gets here.
 */
export interface DirEntryInfo {
  name: string;
  kind: number;
  size: number;
  /** Unix epoch seconds, or 0 if the provider does not track it. */
  mtime: number;
}

/** What `session.getattr()` reports for a path the graph serves. */
export interface StatInfo {
  /** A number from `KIND`. */
  kind: number;
  size: number;
  mtime: number;
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
  /**
   * Extra environment variables for the child.
   *
   * **They are set on *this* process, not only on the child.** `CreateProcessW`
   * is called with a null environment block, which is what makes the child
   * inherit them — so `vfs_embed::Session::launch` writes each one with
   * `std::env::set_var`, launches, and then restores the previous value. A
   * process-wide lock serializes that against every other env write this
   * library performs, so two sessions cannot interleave.
   *
   * **The lock cannot protect a host's own threads.** `std::env::set_var` in a
   * multi-threaded process races anything else reading the environment (it is
   * `unsafe` in Rust 2024 for exactly this reason), and a Node process is
   * multi-threaded by construction. So: while a `launch` with `env` is in
   * flight, no other thread in this process may read or write the environment —
   * including any library that does so for you. Leave `env` unset and the
   * hazard is narrower but not gone: the ring's own `VFS_*` variables are
   * published the same way.
   *
   * The fix is to build the child's environment block explicitly and hand it to
   * `CreateProcessW`; it is costed and not built. Until then this is a
   * documented hazard rather than a safe API.
   */
  env?: Record<string, string>;
}

/** One VFS session: roots, the graph each root serves, the ring, and the launch. */
export interface Session {
  readonly name: string;
  /** The tree holding this session's root, overlay and state directories. */
  readonly baseDir: string;
  /** Root 0's managed directory — what the injected child recognises as the virtual root. */
  readonly virtualRoot: string;
  readonly stateDir: string;

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
   *
   * **It can throw**, so it is not pure bookkeeping. Spec §6's mount-time flag
   * table is enforced here:
   *
   *  * a **`seqread`** provider is a hard error, naming `seekable()` as the fix.
   *    The director reads with `read_at(handle, offset, buf)`, which a
   *    forward-only provider answers `ST_NOT_SUPPORTED` to — so accepting the
   *    mount would mean every read failing later, inside an injected process;
   *  * a **`slow`** provider with no cache above it gets a warning on stderr
   *    naming the handle. Advisory, exactly as §6 specifies, and exact rather
   *    than heuristic because `cached()` clears the flag — the flag surviving to
   *    here *means* nothing is caching it.
   *
   * Takes a `Provider`, not a handle: the Rust signature is
   * `mount(&self, root: u32, provider: &Provider, ...)`. A `ProviderWorker`
   * exposes the object as `.provider`.
   */
  mount(root: number, provider: Provider, prefix?: string): void;

  /**
   * Declare `provider` the writable **upper** for `root` (default 0): reads fall
   * through to the mount graph, and a write copies the file up into `provider`
   * first.
   *
   * Not interchangeable with a `mount`. A sibling mount can only receive writes
   * the graph already routed to it, so an in-place edit of read-only content
   * reaches the read-only source and is refused; an upper is copied up into
   * instead.
   *
   * Point it at `overlayLayerDir(root)`. The shim's local overlay is
   * root-scoped, so a layer declared at that directory's parent shows the
   * director an empty layer while the shim writes one level deeper — a write
   * then reads back as missing.
   */
  setWriteLayer(provider: Provider, root?: number): void;

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
   * its own keys — see `examples/spec-8-example.mts`, which demonstrates both
   * the working round trip and the silent wrong answer.
   */
  readFile(vpath: string, root?: number): Buffer;

  /**
   * List a directory in a root's graph, host-side; `root` defaults to 0.
   *
   * Not a convenience — it is the only way to check two of spec §6's rules from
   * a host. `layered` **unions** its children's listings with top-wins per name,
   * while `router`'s listing is **single-dispatch** rather than the union §6
   * specifies: a file served by a route is readable by name and invisible to a
   * listing of its own directory. Those are indistinguishable without this, and
   * the second is a silent wrong answer.
   *
   * Drives the graph on the calling thread, so the deadlock guard applies here
   * exactly as it does to `readFile`.
   */
  readdir(vpath: string, root?: number): DirEntryInfo[];

  /**
   * Stat one path in a root's graph, host-side; `root` defaults to 0. `null` —
   * not a throw — means the graph does not serve it.
   *
   * The cheapest answer to *does my graph serve the path I think it does?* A
   * mistyped mount prefix or an undeclared root gives a session that serves
   * nothing and reports nothing; this is one call instead of a `readFile` in a
   * `try`.
   *
   * Same case-folding caveat as {@link Session.readFile}: nothing folds a
   * host-supplied path.
   */
  getattr(vpath: string, root?: number): StatInfo | null;

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

  /**
   * `using s = new Session('x')` — `close()` on scope exit. Idempotent.
   *
   * Installed by `index.mts`, not by Rust.
   */
  [Symbol.dispose](): void;
}

interface SessionConstructor {
  /**
   * `name` is a label. It appears in `session.name` and in the session's
   * directory names under `%TEMP%`, so a developer can tell which session left
   * what behind.
   */
  new (name: string): Session;
  readonly prototype: Session;
}

export const Session: SessionConstructor = fn('Session');

// ---------------------------------------------------------------------------
// Loose functions.
// ---------------------------------------------------------------------------

/** Record the directory the addon was loaded from. `index.mts` calls this with `import.meta.dirname`. */
export const setPackageDir: (dir: string) => void = fn('setPackageDir');

export const packageDir: () => string | null = fn('packageDir');

/** The `ST_*` status codes, so a caller can compare against a name. */
export const statusCodes: () => Record<string, number> = fn('statusCodes');

/** The addon's version. A loaded-and-answering check for `require('aethervfs')`. */
export const version: () => string = fn('version');

/**
 * **Panic on purpose.** Exists so a host can confirm in its own process that a
 * Rust panic arrives here as a catchable exception rather than killing Node.
 *
 * Every `#[napi]` function in the addon carries `catch_unwind`, because
 * napi-derive emits the containment only when asked; that is enforced
 * structurally by `tests/napi_entry_points_contain_panics.rs`, and demonstrated
 * by `test/panic.test.mts`, whose only tool is this function. A structural check
 * cannot show the generated containment *works*.
 *
 * `kind` selects the panic payload shape — `'string'` (default), `'str'`, or
 * `'other'` — because `catch_unwind`'s downcast has an arm for each. Anything
 * else is an ordinary rejected argument, not a panic.
 *
 * @throws the panic message, as an `Error`.
 */
export const panicForTest: (kind?: 'string' | 'str' | 'other') => never = fn('panicForTest');

// ---------------------------------------------------------------------------
// The JS-provider bridge — spec §8b, as corrected by measurement in §8c.
//
// These four shapes are the bridge's wire format. They were undeclared before
// the TypeScript migration, which is why the dispatcher that builds them was
// unchecked; `index.mts`'s `encode()` is now typed against `CallResult`.
// ---------------------------------------------------------------------------

/** What a provider's `getattr` returns, once coerced for the bridge. */
export interface JsStat {
  /** One of `kinds()`. */
  kind: number;
  size: number;
  /** **`undefined`, never `null`** — see `index.mts`'s `optionalNumber`. */
  mtime?: number;
}

/** One `readdir` entry, once coerced for the bridge. */
export interface JsDirEntry {
  name: string;
  kind: number;
  size: number;
  mtime?: number;
}

/** What a provider's `open` returns, once coerced for the bridge. */
export interface JsOpen {
  handle: number;
  size: number;
  isDir?: boolean;
}

/**
 * One provider call, as the threadsafe function delivers it.
 *
 * Every field is present on every op — Rust builds the whole struct — so which
 * ones carry meaning is decided by `op`. `root2`/`path2` are `rename` only,
 * `data` is `writeAt` only, `mtime`/`size` are `setAttr` only.
 */
export interface CallRequest {
  callId: number;
  /** One of `providerOps()`. */
  op: number;
  root: number;
  path: string;
  root2?: number;
  path2?: string;
  handle: number;
  offset: number;
  len: number;
  flags: number;
  data?: Buffer;
  mtime?: number;
  size?: number;
}

/**
 * What `completeCall` carries back. Exactly one payload field is populated on a
 * successful call, chosen by the op; the shapes do not overlap, so Rust's
 * `decode` does not need to know which op it was.
 */
export interface CallResult {
  /** An `ST_*` status. 0 is success. */
  status: number;
  /**
   * The host method threw or its promise rejected. What it buys is that a
   * `VfsError(ST_OK)` cannot be mistaken for a success.
   */
  threw?: boolean;
  /**
   * Present when the throw was *not* a `VfsError`: message and stack. Its
   * presence forces `ST_IO_ERROR` on the Rust side regardless of `status`,
   * which is spec §8b rule 3 enforced at the boundary.
   */
  hostError?: string;
  bytes?: Buffer;
  number?: number;
  stat?: JsStat;
  entries?: JsDirEntry[];
  open?: JsOpen;
}

/**
 * **Internal.** How a provider call is settled: `index.mts`'s dispatcher calls
 * this with the call id and the result, and it wakes the parked director thread.
 * Declared because it is exported, not because a host should call it — a host
 * writes a `ProviderObject` and lets the dispatcher do this.
 */
export const completeCall: (callId: number, result: CallResult) => void = fn('completeCall');

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
  /**
   * **Absent, not `null`,** when no timeout is set — an `Option<T>` *field* has
   * its key omitted and reads as `undefined`, while an `Option<T>` *return*
   * arrives as `null`. Task 7 lost time to `callTimeoutMs === null`, which is a
   * check that never matches.
   */
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
 * The three-argument registration Rust exports: the provider object, the
 * dispatcher built from it, and the options.
 *
 * A host calls `index.mts`'s two-argument `registerProvider(obj, options?)`,
 * which builds the dispatcher. An N-API threadsafe function is created from a
 * *function*, not an object, which is why the dispatcher has to be a separate
 * argument.
 */
export const registerProvider: (
  obj: object,
  dispatch: (req: CallRequest) => void,
  options?: ProviderOptions
) => Provider = fn('registerProvider');

/** The handle-taking `releaseProvider`. `index.mts` shadows it. */
export const releaseProvider: (handle: number) => void = fn('releaseProvider');

/** Provider calls outstanding across the whole process. */
export const outstandingProviderCalls: () => number = fn('outstandingProviderCalls');

/** The op integers, keyed by JS method name. */
export const providerOps: () => Record<string, number> = fn('providerOps');

export const kinds: () => Record<string, number> = fn('kinds');

export const openFlags: () => Record<string, number> = fn('openFlags');

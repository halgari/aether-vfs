# aether-vfs — Architectural Overview

**Audience:** engineers joining or reviewing the system.
**Scope:** how the whole thing fits together, and how the genuinely hard parts
are solved. Implementation detail lives in module docs; this is the map.

Companion documents: [`../../docs/product-overview.md`](../../docs/product-overview.md)
(non-technical), [`durability.md`](./durability.md) (what is durable when, for
the storage layers and the registry overlay),
[`../../docs/shim-invariants.md`](../../docs/shim-invariants.md) (the shim's
invariants and the incidents behind them), [`benchmarks/`](./benchmarks/)
(measurements), [`vfs-summary.md`](./vfs-summary.md) (earlier long-form
narrative, historical). The design docs are indexed in
[`../../docs/superpowers/README.md`](../../docs/superpowers/README.md).

---

## 1. What the system does

aether-vfs makes a Windows game see a filesystem that does not exist on disk.

A game is installed as a 15 GB zip archive plus a set of mod folders. Instead of
extracting and merging those onto disk, aether-vfs composes them into a single
virtual tree and serves that tree to one process, live, by intercepting the
Windows NT file API inside it. Every other process on the machine sees the
original, untouched directory.

The proof point is Skyrim Special Edition: it boots, loads its world, and plays
from a Stored zip with **no durable extract** of game content. That runs on
Windows natively, and on Linux under GE-Proton's Wine (§3.10): the game and the
shim are Windows code either way, and only the host and the director differ.

### Why this is worth doing

The established approach to game modding is to copy mod files over the game
install, or to use a kernel filter driver. Copying is destructive, slow, and
makes "what am I actually running?" unanswerable. A driver is invasive, needs
signing, and a bug takes the machine down rather than the game.

A userspace VFS keeps the install pristine, makes a mod list a piece of data
rather than a mutation, and confines failure to one process.

---

## 2. Topology

```text
┌─ Host process (CLI, daemon, or embedding app) ────────────────────────────┐
│                                                                           │
│  Session          (vfs-embed) roots, mounts, serve, launch                │
│  Director         userspace FUSE kernel: resolve, overlay, handle table   │
│  Providers        zip · disk · cache · compose · gRPC plugin              │
│                                                                           │
└───────────────────────────────┬───────────────────────────────────────────┘
                                │  shared memory:
                                │  control ring (slots) + bulk arena (banks)
┌───────────────────────────────▼───────────────────────────────────────────┐
│ Game process                                                              │
│                                                                           │
│  ntdll detours   NtCreateFile / NtOpenFile / NtRead / stat×3 / enum×2 / …  │
│         │                                                                 │
│         ├─ path under the managed root ──► director client ──► ring ──► director│
│         └─ anything else ────────────────► real ntdll, untouched          │
│                                                                           │
│  synthetic handles · demand-paged sections · staged PE closure            │
└───────────────────────────────────────────────────────────────────────────┘
```

**Direction of authority.** The director owns content; the shim owns
interception and owns nothing else. The shim never opens a layer archive. That
single rule is what keeps the trust boundary describable: archive bytes enter
the game only through the ring.

---

## 3. The layers

### 3.1 Content model — `vfs-core`

Pure, OS-free, no I/O. Given enumerated layers, it produces a merged tree and
answers `resolve(vpath)`. It knows about:

- **Layer precedence** — later layers win.
- **Tombstones** — a first-class entry kind meaning "hide what is beneath".
  Deleting in an overlay must not reveal the file it was covering.
- **Case folding** — Windows is case-insensitive; the virtual tree must be too,
  without being case-*destructive* (the original spelling is preserved for
  enumeration).
- **Wildcards** — enumeration filters (`*.esm`) are matched here, not in the hook.

Keeping this layer pure is what makes the merge semantics testable without a
game, a driver, or even a filesystem.

### 3.2 Providers and composition — `vfs-provider`, `vfs-source`, `vfs-zip`, `vfs-compose`, `vfs-storage`

Everything that can supply bytes implements the `Provider` trait
(`vfs-provider`), addressed by `(RootId, relative path)` via `VPath` rather
than a bare string — the root id is what lets one provider instance serve
several virtualized locations at once and still tell `[1, "a"]` from
`[0, "a"]` apart. A provider declares its `Capabilities` once, at
construction; a read-only provider implements a five-method floor
(`capabilities`, `getattr`, `readdir`, `open`, `close`, plus `read_at` or
`read_next`), and everything past that defaults to `ST_NOT_SUPPORTED`. One
conformance suite, `vfs_provider::assert_conformance`, runs the case subset
implied by a provider's declared capabilities against every implementation —
Rust today, host-language bindings later — so none can drift from the
others. Full detail: [`vfs-provider/README.md`](../crates/vfs-provider/README.md)
and the design spec's [§5](../../docs/superpowers/specs/2026-08-13-pluggable-providers-design.md#5-the-provider-contract).

#### Why `immutable` and `slow` are separate flags

`immutable` and `slow` look like they should be one flag; they are not, and
conflating them is the easiest way to get this model wrong. `immutable` says
caching a block is **safe** — the content never changes, so a cached copy is
good forever, even across process restarts. `slow` says caching is
**warranted** — reads are expensive enough that paying for a cache is worth
it. A local disk file is fast and mutable: no cache needed. A remote provider
serving content that can change underneath it might be slow but not
immutable: worth a RAM cache with invalidation, never a disk cache, because a
persisted block could go stale. A Stored zip entry is immutable but fast to
seek into: safe to cache, not obviously worth it on its own. Only a provider
that is both — a large, static, slow-to-fetch download — earns a cache that
survives across sessions. Getting the two backwards produces either a
correctness bug (stale bytes served from a disk cache for content that turns
out to be mutable) or a silent performance bug (content that never changes,
never cached).

- **`vfs-zip`**'s `ZipProvider` parses the ZIP64 central directory and serves
  **Stored** (uncompressed) entries as byte windows into the container.
  Stored-only is a deliberate constraint: a Stored entry is a contiguous
  range, so a read at an arbitrary offset is a seek, not a
  decompress-from-the-start. That is what makes random access into a 15 GB
  archive viable.
- **`vfs-compose`** provides combinators over other providers: `layered`
  (top-wins; `readdir` unions), `router` (glob-pattern dispatch —
  `getattr`/`open` take the first matching route; `readdir` is currently
  single-dispatch rather than the cross-route union the design calls for),
  `overlay` (a writable upper over a base provider: reads fall through to the
  base, the first write to a base-only file **copies it up** into the upper
  (staged as `.cu.<n>.<name>`, then renamed into place), and removing a
  base-visible path writes a `.wh.<name>` whiteout; the base is never
  mutated), `subdir` (rewrite addressing to expose a subtree as a root),
  `seekable` and `readonly` (wrappers that add positional reads or demote
  write access), `disk` (a directory, served read-write), `memory` and
  `inline` (in-memory trees, the second read-only), and `MountGraph` (mounts
  providers at paths and resolves a path to the mount that serves it).
- **`vfs-storage`** owns one `vfs-block-store` block store (deduplicated,
  compressed packs), a redb catalog beside it and a RAM tier of decompressed
  blocks, and serves two things from it: `Storage::cached` wraps an
  immutable, slow source (a remote one) as a pull-through cache keyed by a
  stable `SourceKey`, and `Storage::layer` hands out a named, persistent
  read-write layer — a session's write layer that survives the session. The
  daemon opens one `Storage` per process (`vfs daemon --storage-dir`). What is
  durable when is in [`durability.md`](./durability.md); the design is
  [the vfs-storage design](../../docs/superpowers/specs/2026-09-29-vfs-storage-design.md).
- **`vfs-source`** turns a declarative spec into a live provider, including
  `RemoteProvider`, which forwards every op to an out-of-process gRPC plugin
  — so a provider can be written in any language.

### 3.3 The director — `vfs-director`

The userspace FUSE kernel. Holds the mount table, resolves a virtual path
through it, owns the global file-handle table, and serves ring requests. It is
the only component that touches archive containers.

It is a kernel, not the API a host embeds. `Session` — configure roots and
mounts, `serve()` to stand up the ring, `launch()` to start the target — lives in
`vfs-embed` (§3.11), which sits above the director.

### 3.4 IPC — `vfs-ipc`, `vfs-win`, `vfs-unix`

A shared memory segment holding a **control ring** of fixed slots plus a **bulk
arena** of per-slot banks. Small requests and replies travel inline in the slot;
large reads land in the arena so the ring never has to carry megabytes.

Slot ownership moves by `compare_exchange`, which makes it multi-producer safe
without a lock — necessary because a game issues file I/O from many threads at
once. `vfs-ipc` imports no OS API at all; the mapping and the event objects live
in `vfs-win` (a named section on Windows) and `vfs-unix` (an `mmap` of a real
file, which a Windows shim inside Wine maps too). All `unsafe` is confined to
the segment accessor.

**Concurrency.** Each game thread claims its own slot and waits on that slot
alone, so file operations on different threads do not wait for each other; the
shim holds no lock across a round trip. A slot's life is

```
FREE → CLAIMED → SUBMITTED → PROCESSING → COMPLETED → FREE
                                  └────→ ABANDONED → FREE
```

- A director worker runs one request start to finish, so a read blocked in its
  provider (a block still coming from the network) holds its worker. The shim
  therefore counts reads, writes, truncates and write-opens against a gate
  (`vfs_ipc::DataGate`) sized at three quarters of the worker count the
  director publishes in the ring header. The remaining workers are always free
  for read-opens, stats, listings and closes, which are not counted. The gate
  is process-local `std` synchronisation; nothing about it crosses the ring.
  A permit stands for a worker a data request may be holding, which gives the
  gate three rules:
  - **A permit outlives a timeout.** A read the client gave up on is still
    inside its worker, so its permit stays out until the director has finished
    with the slot. A provider that stops answering can therefore hold at most
    the gate's share of the workers, however many reads time out.
  - **One call cannot take the gate.** A pipelined read holds at most half the
    permits, and takes permits beyond its first only while more than a quarter
    stay free. With 16 workers (12 permits) two deep reads of slow content
    hold 6 and 3, and three permits remain for other threads' reads.
  - **First come, first served.** A permit returned while callers are waiting
    is handed to the one at the front of the line, so a pipeline asking again
    for its next batch goes behind a caller already waiting.

  A wait at the gate is bounded by the same deadline as a wait for a response,
  and fails the read when it runs out.
- A client waits for its response by spinning, with no system call, for the
  first millisecond — every request the ring alone can answer is back long
  before that. After it, the shim yields, and after 20 ms sleeps a millisecond
  at a time (`Notifier::idle_client`), so a thread waiting on a fetch does not
  hold a core.
- A full ring is waited for the same way, and for no longer. A pipelined batch
  has one deadline for all of its requests.
- A client that gives up after `RESPONSE_DEADLINE` (60 s) does not free a slot
  the director is still processing: it marks it `ABANDONED`, and the worker
  frees it when it finishes. Until then nobody can claim the slot or its arena
  bank, so a late reply can never be read by a later request. The worker also
  echoes the request id it answered into the slot header, and the client checks
  it.

The ring's wire version (`vfs_ipc::layout::VERSION`, now 4) covers this state
machine as well as the payload layouts: a shim and a director built from
different versions refuse each other at attach. (The shim's bootstrap config
carries its own version first: §3.5.)

On Linux the ring file is named `state_dir/ring.bin`, but when
`$XDG_RUNTIME_DIR` is a tmpfs owned by the user and closed to everyone else,
that name is a symlink to a file there (mode 0600, in a 0700 directory), so the
ring's pages are never written to disk.

### 3.5 The shim — `vfs-shim`, `vfs-redirect`, `vfs-ntlayout`

Detours on ntdll, installed inside the game. For each intercepted call it
decides: is this path ours? If yes, serve it (from the director, or from a
synthetic handle); if no, call the original function so the rest of the system
is untouched.

**The shim always runs with a director.** Its client (`FuseClient`, in
`vfs-shim/src/director.rs`) owns the ring. There is no standalone engine and no
shim-local answer: the shim holds no tree and no write overlay of its own, and
without the director's client attached nothing is under a managed root. Writes,
copy-up and whiteouts are the director's overlay provider's (§3.2); the shim
forwards them. A call it cannot forward fails; it does not fall back to the real
disk under a root.

`vfs-redirect` holds the pure path logic — which declared root, if any, a path
falls under, and its canonical spelling — so the policy is unit-testable away
from the hooks. `vfs-ntlayout` holds the pure NT byte layouts the hooks fill in
(directory records, dispositions, object names), so they are tested on any host.
The hooks themselves live in `vfs-shim/src/hook/`, one module per concern
(open, attributes, I/O, mutation, directory queries, sections, registry, close);
the invariants each one keeps are in
[`../../docs/shim-invariants.md`](../../docs/shim-invariants.md).

**Bootstrap config.** The host hands the shim a small byte buffer: the managed
root and the static-import table. It is versioned (`VFSC` magic, layout version
2, in `vfs-protocol::shimcfg`), and a host and shim from different builds fail
bootstrap with a named `BootstrapError::Config` rather than reading each other's
bytes. The ring `VERSION` check then rejects a stale shim at attach.

#### The read cache

A game makes huge numbers of tiny reads (one launch of a 3,472-mod list read
`Skyrim.esm` 838,643 times in 4 KiB pieces, and `plugins.txt` a byte per
call), and over the ring each is a round trip. So the shim keeps a block
cache in front of the director for **small reads of immutable files**
(`vfs_ipc::readcache`, OS-free so native tests and `ring-bench` run the same
code; wired in by `vfs-shim`'s `read_cache.rs`):

- A synchronous `NtReadFile` shorter than 64 KiB is served from aligned
  64 KiB units; a miss fetches one unit with one bulk read through the same
  `read_fragmented`, gate and deadline as any other read, and while a file's
  misses keep landing where its last fetch ended (a sequential reader) the
  run doubles, up to 1 MiB in one request. A unit never extends past the
  end of its file. Bytes are what a miss costs — a provider reading a
  compressed store spends about a millisecond a MiB, a hundred small round
  trips' worth — and the first design's fixed 1 MiB blocks fetched 3.3 GiB
  for a launch whose small reads were 2 GiB. Larger reads,
  and reads asking for completion by APC or event, take the uncached path
  unchanged. Anything the cache does not serve falls back to that path, so
  the bytes, the `IO_STATUS_BLOCK` and the file position are always what it
  would give.
- Blocks are per **file** (root and folded path), shared by every handle on
  it, versioned by its size and the director's **mount generation**: 16 MiB
  per file with LRU, 256 MiB process-wide (`VFS_SHIM_READ_CACHE_MIB`) with
  global LRU. Per-file locks, none
  held across a fetch, one fetch per block however many threads miss it,
  and bytes reserved before a fetch starts, so memory stays bounded.
- **Coherence rule: only what cannot change is cached.** The director's
  open reply says whether the handle is immutable (`Provider::is_immutable`,
  which an overlay answers for the child holding the handle: a base file of
  an immutable base is, anything in the write layer is not). Only such
  handles are served. A write open, an open reported mutable, a write, a
  truncate, or a delete or rename (source and target, everything under them)
  through this process drops the file for the rest of the process. Because
  writable files are excluded rather than invalidated, another process
  writing the same write layer cannot make the cache stale: what it writes
  is never cached, and a file it copies up is reported mutable at its next
  open here.
- A file read at random across more units than it may hold goes *cold*:
  after 16 re-fetches of units its own LRU dropped, averaging under 8 hits
  each, it is read uncached for a while instead of turning every small read
  into a fetch. A file's first fetch of a unit, and a miss on a unit the
  process-wide cap evicted, are not held against it.

The policy was tuned by replaying the provider-side traces of real launches
(every handle's reads, in order) through the cache: against no cache and
the first design, the launch above needs 52,811 small round trips (vs
475,274 and 52,886) and fetches 2.13 GiB (vs 2.03 GiB read and 3.55 GiB).

A provider that traces its reads sees the cache's unit fetches, not
the program's own read pattern: capture an access trace (Haskill's replay
numbers, for one) with the cache off.

`VFS_SHIM_READ_CACHE=0` turns it off, and the `VFS_SHIM_STATS_LOG` report has
a section for it (hits, misses, declined, fetches and bytes, evictions,
invalidations, misses the cap caused) with the twenty busiest files: their
small reads, hits, misses, bytes fetched, and whether they went cold and
why. The open reply's `immutable` flag and generation sit in what
was padding, so the wire version did not change: an older director's reply
reads as mutable and is never cached.

### 3.6 Process creation — `vfs-pe`, `vfs-inject`, `vfs-director::stage`

Getting the shim into the process before the process needs the VFS. This is the
subtlest part of the system and gets its own section below.

**Activation fails closed.** An import-activated exe cannot start without its
shim: the loader refuses a missing one (`0xC0000135`) or one whose `DllMain`
fails (`0xC0000142`). An injected target is held suspended until the shim has
bootstrapped inside its `LoadLibrary` and said so in the ready file, and a target
whose shim failed is terminated: if injection fails, the shim's bootstrap
reports a failure (config mismatch, director unreachable), the target dies
early, or the ready timeout passes, the launch returns an error and the process
is killed. The
same rule holds for every child the game creates (§4.3). The failure mode is a
launch that errors, never an unvirtualised game that writes to the real disk.

### 3.7 Configuration — `vfs-env`

Almost all configuration crosses a process boundary: the host sets environment
variables, `CreateProcessW` inherits them, and the shim reads them inside the
game. That is the right mechanism for a boundary we do not otherwise control,
but spelling the names as literals at both ends made drift free and silent — a
rename at the writer that misses the reader produces no error, just a feature
that quietly stops working.

`vfs-env` holds one constant per name, a table describing all of them, and a
test that fails if any crate reads a `VFS_*` name absent from that table. It has
no dependencies, so even the shim can use it.

Two boolean forms, because the tree already had both spelled inconsistently:
`opt_in` is off unless explicitly enabled (used for anything that relaxes a
guarantee — `ALLOW_DISK_FALLTHROUGH` un-seals the managed root, and must not be
enableable by accident), and `opt_out` is on unless explicitly disabled.

`vfs_env::describe()` prints the whole surface; the rustdoc on each constant is
the reference.

### 3.8 Control plane — `vfs-control`, `vfs-directord`

A gRPC contract plus a declarative config schema (`vfs-control`), and a daemon
(`vfs-directord`) that can hold many sessions, with the `vfs` CLI. The daemon
builds each session through `vfs-embed`, as any other host would. The control
plane is language-agnostic; the data plane is the ring.

### 3.9 Registry overlay — `vfs-registry`, `vfs-director`'s `registry` module, the shim's `reg*` modules

Injected processes see the real registry, but none of their registry writes
reach it: they go to a per-profile **registry layer** that persists between
runs (spec: `docs/superpowers/specs/2026-10-05-registry-overlay-design.md`).

- **Switch.** A host attaches a layer with `Session::set_registry_layer`;
  while one is attached, launches set `VFS_REGISTRY=1`. Without it the shim
  installs **no** registry detours at all, so a process with the overlay off
  carries none of them.
- **Director.** `RegistryHost` holds the session's overlay tree
  (`vfs-registry::Overlay`: values, value tombstones, present or tombstoned
  child entries, created-here or overlaying-real origin) and saves it whole as
  `overlay.reg` in the layer (write `overlay.reg.tmp`, rename), at most once a
  second while dirty and at every flush.
- **Durable point.** A save is an ordinary layer write, so under a deferred
  store it is not durable by itself. The host passes its store's sync as the
  layer's `RegistrySync` hook (`registry_sync_for(&storage)` for a `Storage`);
  the session calls it after the final save at stop and at detach, and the
  saver calls it at most every five minutes while a save is not yet durable.
- **Ring.** Opcodes 15–22: `REG_LOOKUP`, `REG_KEY`, `REG_SET_VALUE`,
  `REG_DELETE_VALUE`, `REG_CREATE_KEY`, `REG_DELETE_KEY`, `REG_RENAME_KEY`,
  `REG_CHANGED`. The director publishes a registry generation in the ring
  header (`reg_gen`, ring `VERSION` 4), bumped by every registry write and
  every attach or detach.
- **Shim.** Hooks the NT registry calls (open/create, query, write, notify,
  security, handle flags, and the out-of-scope hive and transaction calls). A
  key the overlay does not touch is served by the real handle unchanged; one
  it touches gets a synthetic handle whose queries merge the real key with the
  overlay node in the exact NT layouts. `REG_LOOKUP`/`REG_KEY` answers are
  cached per path, tagged with the generation read before asking, and used
  only while the published generation still equals it — a cache hit is one
  atomic load.
- **All or nothing per process.** If any registry detour cannot be installed,
  the process does not virtualise its registry at all (every hook passes
  through), because a missing write hook would let writes through a virtual
  view reach the real registry.
- **Fail closed.** Writes never fall back to the real registry: a director
  failure, a key handle whose name cannot be read, or a real-modifying call
  made while the hook is bypassed returns `STATUS_UNSUCCESSFUL`. Reads fall
  back to the real registry, counted in the shim stats.

### 3.10 The Proton host path — `vfs-proton`, `vfs-unix`, `vfs-embed`

Linux is a supported host. The game is still a Windows program and so is the
shim: both run inside GE-Proton's Wine, while the director, the providers and
the host are native Linux. Only the transport and the launch differ from the
Windows path.

- **Ring.** `Session::serve` creates a file-backed ring (`state_dir/ring.bin`,
  mapped by `vfs-unix` natively and by the shim through Wine), tmpfs-backed when
  `$XDG_RUNTIME_DIR` is private (§3.4). The wire protocol and the version are
  the same as on Windows.
- **Runtime.** `vfs-proton` finds, downloads, verifies and extracts GE-Proton
  (`vfs-proton install`); it refuses anything that is not GE, because an
  unset or wrong `PROTONPATH` silently falls back to stock Proton. It is portable
  on purpose, so its logic is tested on both CI hosts.
- **Prefix.** A launch runs in a Wine prefix: an anonymous one deleted with the
  session, or a named one that persists (`$VFS_HOME/sessions/<name>/prefix`).
  `Session::prepare_prefix` brings one up without serving anything. A root's
  location (`C:\...`) is a symlink into the prefix's `drive_c`, created at the
  first launch and removed with the session. The managed root is always empty on
  disk; content only ever arrives through the ring.
- **Windows half.** The injector (`vfs-injector.exe`), the shim and payload
  DLLs, and the test probes are Windows binaries cross-built by
  `bin/build-windows`; `vfs_proton::artifacts::WINDOWS_ARTIFACTS` is the one list
  of them.
- **Launch.** The injector argv and the environment the shim reads are pure
  functions (`vfs_proton::launch`), so they are unit-tested without Wine. A
  launch lasts as long as anything runs in the prefix, not only the program it
  started (a mod loader that starts the game and exits keeps it alive), and
  `LaunchHandle` lets the host poll or stop it.

### 3.11 Embedding — `vfs-embed`

`vfs-embed` is the one crate a host names. It owns a `Session`: the roots, the
provider graph each root serves, the ring the injected shim talks over, and the
launch (Windows or Proton). `vfs.exe` and the daemon are hosts like any other;
if a host has to reach past `vfs-embed`, the fix belongs in `vfs-embed`. It
re-exports what a host needs (the provider and composition types, the storage
types, and a `proton` module for runtime, prefix and GPU probes). Its Proton
code is `session/proton/`; Windows launch and staging are `session/windows.rs`
and `session/stage.rs`.

---

## 4. The hard parts, and how they are solved

This is the section worth reading. Each of these took real effort to get right,
and several were only understood after a failure that produced **no error at
all**.

### 4.1 Bootstrapping: the process must be hooked before it can run

You cannot hook a process that does not exist, and by the time it exists the
Windows loader has already resolved the executable's static imports. A game EXE
sitting alone in an otherwise-virtual directory dies at `STATUS_DLL_NOT_FOUND`
before a single line of our code runs.

Two mechanisms:

1. **Staging the PE closure.** Before launch, the director writes the target EXE
   and its non-system static imports — transitively, and nothing else — into a
   scratch directory. For Skyrim that is a 37 MB EXE and three DLLs against
   ~15 GB that stays virtual. The directory is deleted when the process exits.
   The loader resolves every static import from real files, so nothing needs to
   be hooked before it runs.
2. **The shim bootstraps in its own `DllMain`,** synchronously, however it was
   loaded, and fails the load if it cannot. It arrives one of two ways:
   - **Import activation** (`LaunchOpts::activation`, the default). Staging
     rewrites each EXE it stages (`vfs_pe::add_first_import`, the Detours
     `setdll` technique) to import the shim before anything else, raises its
     header's stack reserve to 16 MiB, and stages the shim beside it. The
     launcher starts it normally. The shim's `DllMain` runs after the imports
     are mapped and before any other import's `DllMain`, its TLS callbacks or
     its entry point, whoever started the process; the loader refuses to start
     it if the shim is missing or fails.
   - **Injection**, for an EXE that is not staged (a real file, or outside every
     root), that the patch refused (`StagedDir::unpatched`, reported in the
     launch notes), or under `Activation::Inject`. The patch refuses an EXE whose
     Steam DRM wrapper (SteamStub) verifies its own file: Journals of
     Jyggalag's downgraded 1.6.1170 `SkyrimSE.exe` does, and patched it put up a
     "Steam Error" dialog instead of starting; Steam's own 1.7.104 does not. It works the way SKSE injects
     its DLL: create the target suspended,
     grow its primary stack to 16 MiB, `LoadLibrary` the shim on a remote
     thread (which first runs process initialisation, then the shim's
     bootstrap), wait for that thread, and resume once the ready file says
     "ready". The primary thread never runs before the hooks are live.

   The launcher and the process hook choose per EXE, by reading its import
   table (`vfs_inject::exe_imports_shim`; the hook reads the real file, since
   the VFS answers the path with the unpatched original). Injecting a patched
   EXE would load a second shim.

Until 2026-10 a third mechanism ran first: a `no_std` early payload, reflectively
mapped and entered by redirecting the primary thread's start address, hooked the
path stubs before the loader ran and held the thread at a spin gate while the
full shim loaded. It existed to serve static imports from the virtual root;
staging made it redundant, and it was removed.

### 4.2 `CreateProcess` needs a real file on disk

Windows will not create a process from a buffer, and for a long time the answer
here was **process hollowing**: create the process from some unrelated on-disk
host image, then overwrite that image in memory with the PE we actually wanted.
It preserved a strict "no game PE ever touches disk" invariant.

Making a hollowed MSVC CRT executable actually *run* cost a great deal —
security cookie initialisation, remote TLS plus the TEB slot,
`RtlAddFunctionTable` for x64 unwind data, LDR `SizeOfImage`/`EntryPoint`
fixups, an entry trampoline so exception registration ran on the primary
thread — and every one of those was a hand-written re-implementation of
something the Windows loader already does correctly.

**That invariant no longer holds, so the mechanism is gone.** Staging writes the
real EXE and its import closure to a scratch directory, which means there *is* a
real file to `CreateProcess`, and the loader maps, relocates and binds it
properly. The launch path is now simply: stage → `CreateProcess` suspended →
inject → resume.

The measurements that settled it (three runs each, 2026-08-13) are worth keeping,
because the redundancy was not free:

| | time to window | VFS bytes read |
|---|---:|---:|
| with hollow | 5.07 / 5.13 / 5.12 s | 532 MiB |
| without | **2.82 / 2.81 / 2.81 s** | **69.5 MiB** |

The hollow had become a no-op that still did all the work: `VFS_HOLLOW_HOST`
pointed at the staged EXE, so the code re-read that PE from the VFS, re-applied
the same relocations, and wrote it back over the loader's own correct mapping at
the same base. `host_is_target` was true, which already caused the TLS setup to
be skipped — an explicit admission that the loader had done it right.

Removing it deleted ~3,800 lines (`ghostly.rs`), the purpose-built neutral host
crate, three diagnostic binaries, a `hollow_pe` flag threaded through the gRPC
contract and every launch API, and a hardcoded `contains("skyrimse")` special
case. One consequence had to be carried across deliberately rather than deleted:
the hollow path also grew the primary thread's stack to 16 MiB, because the
shim's extra frames overflow the stock 1 MiB stack (`0xC00000FD`). That is not
hollow-specific and now happens on the single launch path.

**The one capability genuinely lost** is launching a child EXE that exists *only*
inside an archive, with no path to `CreateProcess` from. Staging the whole launch
closure — including a child that a loader will spawn — covers the real cases (see
§4.3), and keeping a second launch mechanism alive for a hypothetical one was
judged not worth its weight.

### 4.3 Following the game across process creation

A mod loader does not *become* the game; it launches it. `skse64_loader.exe`
starts, does its work, spawns `SkyrimSE.exe`, and exits. The virtualised view has
to survive that handover, or the process that actually plays the game sees the
real, nearly-empty directory. This is a known hard part of the problem for any
VFS in this space.

Two halves solve it:

**Stage the whole launch closure, not just the entry point.** When the launch
executable is a loader, staging also places the game EXE beside it, because the
loader will `CreateProcess` it and that needs a real image for exactly the same
reason the top-level launch did. It also stages `skse64_<runtime>.dll`
explicitly: SKSE injects that at runtime rather than importing it, so a PE import
walk cannot discover it. For an SKSE launch the staged set is six files.

**Follow the shim across `CreateProcess`.** The shim detours
`kernelbase!CreateProcessInternalW` — the single funnel beneath every
`CreateProcess*` variant — forces the child to start suspended, injects it as
above (the child's primary thread is never resumed before the shim is up, so a
caller that asked for a suspended child, like `skse64_loader`, gets it still
suspended), then resumes it unless the caller asked otherwise. A child whose
EXE imports the shim is left to activate itself through that import. **It fails
closed**: a child whose injection fails, whose shim reports a bootstrap failure,
that dies early, or that is not ready within the launch's ready timeout is
terminated and its `CreateProcess` call returns `FALSE` (`ERROR_PROCESS_ABORTED`).
A child is never released without the shim, so the failure mode is a launch that
errors, not an unvirtualised game that writes to the real disk. The refusal
reaches the launcher: the shim lists the children it killed beside the ready
file, and the host reports them. The wait is the
launch's own (`LaunchOpts::ready_timeout`, else 180 s), which the injector passes
down in `VFS_READY_TIMEOUT_SECS`; every child is injected, none is skipped. The
child's image identity is scoped so the parent's does not leak into it.

Verified 2026-08-13: launched via SKSE, the hook-stats file is written by the
*child* pid, `getskseversion` reports `2.2.6` in-game, and `coc riverwood` loads
the world — with no hollow anywhere in the path.

### 4.4 Multi-gigabyte memory-mapped archives

Bethesda archives are opened with `CreateFileMapping` and read through slid
views. A naive implementation would have to materialise a whole BSA to back the
section — several GB, per archive.

Instead, `NtCreateSection` **reserves** address space without committing it, and
a vectored exception handler commits and streams 256 KiB chunks from the
director on first touch. Large archives are demand-paged; small ones take an
eager path.

The lifetime rule matters as much as the paging. The reservation belongs to the
*section*, never to a view, because that is NT's model and the game depends on
it: a BSA reader slides views across one archive, so unmapping one window must
leave every other window — and any later remap of the still-open section —
valid. The VA is released only once the section handle is closed **and** the last
view is gone. Getting this wrong produced crashes far away from the cause.

### 4.5 A file has more than one name

This is the defect class that cost the most, and it is worth stating plainly:
**NT lets a caller name a file as (directory handle + relative name)**, not only
as an absolute path. `CreateFileW("Data\\X")` reaches ntdll as the process's
current-directory *handle* plus the relative string.

A hook that only understands absolute names does not fail on these. It decodes
nothing, declines to act, and the call proceeds to whatever is really on disk
behind the mount. No error, no log line, no counter — the file simply appears
not to exist. Skyrim reached its main menu with an empty load order for exactly
this reason: every plugin lookup took the relative form.

The resolution is one shared `parent_dir_of_handle` consulted by every hook that
decodes a name, covering three kinds of parent:

1. our own synthetic directory handles,
2. real directory handles the process opened (we record a path for every
   successful open),
3. **the current-directory handle**, which the OS creates and publishes only in
   `RTL_USER_PROCESS_PARAMETERS.CurrentDirectory` — there is no API that returns
   it, so it is read from the PEB.

The same defect appeared independently in three hooks. It is now structurally
impossible for one to know about a parent the others cannot.

### 4.6 The same question has several APIs

Windows offers multiple ways to ask "does this file exist, and how big is it",
and callers choose between them for reasons of their own — the same program uses
different ones in different code paths.

- **Enumeration**: `NtQueryDirectoryFile` *and* `NtQueryDirectoryFileEx`.
- **Stat**: `NtQueryAttributesFile`, `NtQueryFullAttributesFile`, and
  `NtQueryInformationByName` — which Windows 11 prefers for existence checks.

Any hook that answers differently from its siblings produces a program that
believes a file both exists and does not. Both failure directions are real: a
false negative makes content silently invisible (this is what suppressed
Skyrim's intro video), and a false positive leaks a file the sealed
view deliberately hides.

Every one of these entry points is hooked, they share one implementation body
where the shapes allow, and cross-API agreement is a test rather than a
convention (§6).

### 4.7 Performance was never where it looked

Early measurement showed the VFS adding ~9.3 s to a game launch, roughly a 10×
slowdown. The natural assumption — too many reads, or reads that are too small —
was wrong. Swapping the content source (zip vs plain disk) changed nothing, and
only ~800 director operations were involved.

The cost was **wake latency**, not work. Per-hook instrumentation with max and
`>1 ms` stall counts made this visible: `NtQueryFullAttributesFile` averaged
819 µs/call, but 215 of 231 calls were fast and sixteen took up to 15.2 ms —
the Windows timer quantum. The average described no call that ever happened.

Two fixes, both about who is awake:

- **Server-side spin-then-wait**: the director spins while the ring is hot
  instead of sleeping between bursts. 10.34 s → 2.74 s.
- **The client wakes the server**: the shim signals an event on submit and then
  spins for the reply, which arrives in 20–209 µs — far less than the cost of
  sleeping for it. Time inside hooks fell 0.536 s → 0.180 s, and every
  quantum-scale stall outside `NtReadFile` disappeared.

The lesson encoded in the tooling: report max and stall counts, never a bare
mean, because the two shapes of "slow" want opposite fixes.

### 4.8 Directory listings

An enumeration must apply the caller's wildcard, honour case-insensitivity,
and remain stable across the restart-scan and single-entry-at-a-time protocols
NT allows. It is also stateful: a directory handle carries a cursor, so the
listing is built once per scan and served in slices.

It must **not** show the union of real and virtual entries. An earlier version
of the shim merged them, and was wrong for the same reason §4.9 gives. A
directory listing is a spelling of the paths it names, so the sealed root applies
to it in full: under a managed root the director's `readdir` is the whole
answer, never merged with the real directory behind the mount. If the director
cannot be asked, the listing is empty rather than drained from disk. See
`serve_dir_query` in `vfs-shim/src/hook/dirquery.rs`, and
[`escape-matrix.md`](./escape-matrix.md) for the fall-through this replaced and
the test that holds it closed.

### 4.9 Isolation

The managed root is **sealed**: under-root paths resolve through the director
only, never falling through to whatever happens to be on disk there. This is
what makes "the game cannot read anything we did not give it" a property rather
than a hope, and it is why the runtime directory is nearly empty at rest.

### 4.10 Handle identity

Once a file is served from a synthetic handle, everything asked *about* that
handle must answer consistently — name, size, position, volume information —
or a caller that stats its own open file gets a contradiction. Handles carry
their virtual identity so queries return the virtual answer, not the backing
file's.

---

## 5. Performance

Time-to-window for Skyrim SE, three clean runs each
([`benchmarks/load-debug-vs-release.md`](./benchmarks/load-debug-vs-release.md),
[`benchmarks/hollow-removal.md`](./benchmarks/hollow-removal.md)):

| configuration | mean | measured |
|---|---:|---|
| native, no VFS | 1.0 s | 2026-08-12 |
| VFS, before wake fixes | 10.34 s | 2026-08-12 |
| VFS, after wake fixes (hollow still in) | 2.74 s | 2026-08-12 |
| **VFS, staged launch (current)** | **2.81 s** | 2026-08-13 |

The current figure is roughly 1.8 s over native. About 0.72 s of that is work
native never does at all — staging, injection — and the rest is hook time.

Two cautions about reading this table. The rows are not a clean progression:
each was measured on the build of its day, and the 2026-08-12 rows predate the
relative-name fixes (§4.5), which broadened what resolves through the VFS. On
2026-08-13 the *same* benchmark put the hollow path at 5.1 s, so that path had
regressed since its 2.74 s was recorded; removing it restored the profile
(69.5 MiB read at the window, 164 KiB/read — an exact match for the historical
table) rather than beating it.

Content source is not a factor: zip and disk providers measure the same
(10.34 vs 10.38 s pre-fix), which is what identified wake latency as the cost.

---

## 6. Testing strategy

The system's characteristic failure is **silence** — work that is skipped rather
than work that errors. Tests are shaped around that.

- **Pure layers are unit-tested** (`vfs-core`, `vfs-redirect`, `vfs-ntlayout`,
  `vfs-registry`, `vfs-ipc`, `vfs-pe`): merge order, tombstones, case folding,
  wildcards, NT byte layouts, ring state transitions. They build and test on
  Linux.
- **Hook behaviour is tested in-process.** Integration binaries install the real
  detours into the test process and then use ordinary `std::fs` and raw NT calls
  against a live director (a real one, or the fake in `tests/fakedirector`).
  One install per process, so every `#[test]` in `vfs-shim/tests/`
  re-executes itself in a fresh process (`tests/common`), and the scenarios are
  grouped into a few binaries by concern. These are Windows executables: on
  Linux they run under Wine with `bin/wine-shim-tests`.
- **The Proton path is tested end to end** by `#[ignore]`d `vfs-embed` tests
  that launch Windows fixtures under GE-Proton against a native director. A test
  whose prerequisite is missing prints `SKIP` and passes; `VFS_TEST_REQUIRE_ALL=1`
  makes a skip a failure (see the root README).
- **Every naming form is covered, not just the convenient one.** The
  relative-name battery exercises each decoding hook through a real directory
  handle, because Win32 decides on its own whether a relative path becomes an
  absolute name or a handle pair — going through `std::fs` alone cannot
  guarantee the second form was hit.
- **Cross-API agreement is asserted directly**: both enumeration entry points
  must return one view; every stat API must agree about existence and size, in
  both directions.
- **Byte-exactness** is checked against ground truth, including against a native
  extract of the same archive when the corpus is present.

Two habits worth keeping:

*Test the wiring, not only the unit.* The staging-alias predicate was unit-tested
and correct; what drifted was which callers consulted it. A test of a predicate
cannot catch a caller that does not call it — only a single shared implementation
can.

*Verify a regression test fails.* The enumeration-parity test was confirmed to
fail with the classic detour disabled, which is the exact defect that once
shipped.

Beyond the suite, behavioural changes are verified against the running game. The
window is driven headlessly with [`tools/gamectl.ps1`](../../tools/gamectl.ps1),
which screenshots it and injects scancode-level input via
`SendInput`/`KEYEVENTF_SCANCODE` (the game ignores `SendKeys` and posted
messages): `key GRAVE` opens the console, `type "coc riverwood"` spells a command
out key by key, `shot out.png` captures a frame, and runs end with `qqq` so the
game exits on its own path. See
[`benchmarks/hollow-removal.md`](benchmarks/hollow-removal.md) for the procedure.

---

## 7. Diagnostics

Because failures are silent, the shim carries instrumentation that can be turned
on with `VFS_SHIM_STATS_LOG` and answers questions counters normally cannot:

- per-hook calls, total and **max** time, and a `>1 ms` stall count;
- what the **read cache** did (§3.5): hits, misses, block fetches, evictions
  and invalidations;
- **open paths by frequency** — a retry loop reopens one path thousands of
  times, and a deduplicated list hides exactly the path that matters;
- **every directory enumeration** with its filter, entry count, and which
  mechanism answered it — `director`, `contained` (under a root, but the
  director could not be asked: the listing is empty), or `OS` (outside every
  root). "Listed `Data`, got nothing" and "never listed `Data`" are different
  bugs with identical symptoms, and so are "the director listed it" and
  "something else did";
- **every attribute query with its outcome**, since a stat that wrongly says no
  never becomes an open;
- an **ordered trace** of under-root operations, because counts cannot show
  where a sequence stopped;
- **undecodable opens**, which is the only way an unnameable path becomes
  visible at all.

That last one is the general lesson: when a counter shows *nothing*, suspect the
observer before concluding the process is idle.

---

## 8. Crate map

| crate | role |
|---|---|
| `vfs-core` | pure merged-tree resolver: layers, tombstones, case folding, wildcards |
| `vfs-provider` | provider contract: `Capabilities`, `VPath`, `Provider`, conformance suite |
| `vfs-protocol` | ring wire codecs and opcodes, the shim bootstrap config (`VFSC` v2); re-exports the provider contract |
| `vfs-ipc` | control ring + bulk arena + read cache, OS-free |
| `vfs-win` | Windows shared memory, events and volume lookups |
| `vfs-unix` | Unix shared memory: the file-backed ring (the mirror of `vfs-win`) |
| `vfs-zip` | ZIP64 central directory, Stored windows, `ZipProvider` |
| `vfs-compose` | provider combinators: layered, overlay (copy-up), router, subdir, seekable, readonly, disk, memory, inline, `MountGraph` |
| `vfs-storage` | pull-through cache for slow sources + named persistent layers, on `vfs-block-store` |
| `vfs-block-store` | deduplicating, compressing block store (redb index + zstd packs) |
| `vfs-source` | declarative spec to provider, incl. `RemoteProvider` gRPC plugins |
| `vfs-director` | the kernel: root to provider table, handle namespace, ring server, staging, registry host |
| `vfs-embed` | **the embeddable API**: `Session`, roots, composition, serve, launch (Windows and Proton) |
| `vfs-proton` | GE-Proton install, prefix, Wine launch, Steam and NVAPI probes; the Windows artefact list |
| `vfs-registry` | registry overlay tree, its file format, the merge with a real key, NT query layouts |
| `vfs-directord` | daemon + `vfs` CLI (a host of `vfs-embed`) |
| `vfs-control` | gRPC contract + config schema |
| `vfs-env` | every `VFS_*` switch, defined once, with a drift test |
| `vfs-redirect` | pure path core: root map and canonicalisation |
| `vfs-ntlayout` | pure NT byte layouts and decisions for the shim's hooks |
| `vfs-shim` / `vfs-shim-dll` | NT detours, the director client, synthetic handles, sections, registry hooks |
| `vfs-inject` | injection and process creation (`vfs-injector`) |
| `vfs-pe` | pure PE byte parsing, so any host can stage Windows executables |
| `vfs-fixture-*` | Windows probe programs the tests run under the shim |
| `vfs-testkit` | dev-only helpers shared by integration tests |
| `vfs-bench` | `ring-bench` (ring round trips) and `skyrim-live` (live Skyrim launch harness, Windows) |

Dependency direction is enforced by the split: pure crates never learn about the
OS, and the zip provider never learns about the host.

---

## 9. Known limitations

- **Windows x64 guests only.** The game and the shim are Windows x64 code. A
  Linux host runs them under GE-Proton's Wine (§3.10); there is no native Linux
  game support, and no macOS host.
- **Stored zip entries only.** Deflate would defeat random access. Archives are
  expected to be repacked Stored.
- **Router `readdir` is single-dispatch**, not the cross-route union the
  provider design calls for.
- **Anti-cheat.** The techniques here are indistinguishable from those an
  anti-cheat system exists to detect. This is a single-player modding tool.
- **Per-child staging recursion** for sub-processes is designed but not
  implemented.

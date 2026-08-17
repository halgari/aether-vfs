# Pluggable Providers and the Embeddable Library — Design Spec

**Date:** 2026-08-13
**Status:** Approved, not implemented.
**Scope:** Windows-first. The provider contract itself is OS-independent; the
shim, launch, and root-resolution work is Windows-only as before.

## 1. Goal

Turn aether-vfs from a CLI application into an **embeddable library** with a
capability-typed, pluggable provider model.

Two things change together, because they are the same interface seen from two
sides:

1. **Providers become genuinely pluggable.** A provider declares what it can do
   — sequential read, seekable read, read-write, immutable, slow — and the
   composition layer respects that declaration. Providers can be written in
   Rust or in a host language.
2. **The engine becomes embeddable.** The director runs inside the host
   language's process. `vfs.exe` and the Python `aethervfs` package are two
   hosts of equal standing over one library. Python is the first non-Rust host;
   TypeScript and C# are expected to follow the same pattern.

The end state: a Python script composes a filesystem from a Steam CDN provider,
a zip archive, some mod directories, and an in-memory INI provider, launches
Skyrim against it, and reads back what the game wrote.

## 2. Reversal of a prior decision

The 2026-08-11 director daemon spec locked:

> **gRPC is the only entry point.** The in-process C ABI is removed [...] No
> in-process embedding mode going forward.

**This spec reverses that.** In-process embedding is now the primary mode. The
reasoning has changed: at the time, the goal was a single daemon with one
control plane, and a C ABI was redundant with gRPC. The goal is now a library
that other languages host, and an out-of-process control plane cannot host
anything — it can only talk to something already running.

The reversal is *not* a return to the deleted C ABI. gRPC remains, as the
control plane for the daemon host and as one provider implementation
(`remote`). What returns is in-process embedding, by way of **per-language
native bindings** rather than a C header.

## 3. Locked decisions

| Fork | Decision |
|------|----------|
| Cross-language boundary | **Per-language native bindings.** PyO3 for Python, napi-rs or similar later. No shared C ABI. |
| Capability expression | **Declared flags plus one wide interface.** A provider returns a `Capabilities` value; unimplemented methods return `ST_NOT_SUPPORTED`. |
| Write routing | **All writes flow through the provider stack.** The shim's redirect-to-real-disk becomes one provider implementation, not a parallel mechanism. |
| Read-only fallover | **Explicit only.** No implicit scratch overlay. A write with no `ReadWrite` provider at that path fails, and is recorded so the set can be discovered. |
| Composition | **One provider per root.** Combining is done by providers that take providers. No layer-ordered mount merging. |
| Addressing | **`(RootId, root-relative path)`.** Combinators pass both through unchanged. |
| Primitive ownership | **Rust owns the primitive library.** Host languages write novel data sources, never plumbing. |
| V1 scope | **Full vertical slice**, including the Python end-to-end Skyrim test. |

### Also decided

- **`Backend` is renamed to `Provider`** throughout, and `SourceSpec` to
  `ProviderSpec`. The codebase currently uses `Source`, `Backend`, and
  "provider" for one concept; a library gets one word.
- **The workspace splits** so the main tree can build with `panic = "unwind"`.
  See §9.
- **`tools/gamectl.ps1` is kept**, not rewritten in Python.

## 4. Architecture

```text
   ┌──────────────────────┐        ┌───────────────────────────┐
   │  vfs.exe (Rust host) │        │  aethervfs (Python host)  │
   │  CLI + gRPC daemon   │        │  PyO3 extension module    │
   └──────────┬───────────┘        └─────────────┬─────────────┘
              │                                  │
              └───────────────┬──────────────────┘
                              │
                    ┌─────────▼──────────┐
                    │     vfs-embed      │  session lifecycle, roots,
                    │  (public API)      │  composition, launch
                    └─────────┬──────────┘
                              │
        ┌─────────────────────┼─────────────────────┐
        │                     │                     │
  ┌─────▼──────┐      ┌───────▼────────┐    ┌───────▼────────┐
  │  Director  │      │  vfs-provider  │    │  primitives    │
  │  (kernel)  │◄─────┤   (contract)   ├───►│ vfs-compose,   │
  └─────┬──────┘      └────────────────┘    │ vfs-cache, ... │
        │                                    └────────────────┘
        │ shared-memory ring
  ┌─────▼──────────────────────────────┐
  │  game process (shim injected)      │
  └────────────────────────────────────┘
```

The game is always a separate process. The ring server and director dispatch
run on native Rust threads that the library owns. A Python host touches them at
session setup and teardown only. Python pays a GIL acquisition **only** when a
Python-authored provider is in the stack — which is exactly the case that gets
a cache in front of it.

### Crate shape

| Crate | Role | Status |
|---|---|---|
| `vfs-provider` | Contract: `Capabilities`, `VPath`, `Provider`, status codes, conformance suite | **new** |
| `vfs-embed` | Public embeddable API: session lifecycle, roots, composition, launch | **new** |
| `vfs-python` | PyO3 cdylib → `aethervfs` wheel | **new** |
| `vfs-protocol` | Ring wire codecs and opcodes only; the trait moves out | trimmed |
| `vfs-compose` | Combinator primitives, capability-aware | extended |
| `vfs-cache` | `cached` primitive; keys gain root scoping | extended |
| `vfs-source` | gRPC `remote` provider; proto extended with caps and writes | extended |
| `vfs-directord` | Thin Rust host over `vfs-embed` | slimmed |
| `vfs-director` | Director kernel; mount merge deleted, write dispatch added | changed |
| `vfs-redirect` | Multi-root path resolution | changed |

## 5. The provider contract

The ring protocol already reserves `OP_WRITE`, `OP_SETATTR`, `OP_RENAME`,
`OP_DELETE`, `OP_MKDIR`, and `OP_MATERIALIZE` (`vfs-protocol/src/lib.rs:20-25`).
Provider methods mirror those opcodes rather than inventing a second vocabulary.

### Capabilities

```rust
pub struct Capabilities {
    pub access: Access,               // SeqRead | Read | ReadWrite
    pub immutable: bool,              // content never changes
    pub slow: bool,                   // expensive to read
    pub preferred_block: Option<u32>, // block-size hint for `cached`
}

pub enum Access { SeqRead, Read, ReadWrite }
```

`ReadWrite` implies positional read; there is no write-without-seek tier.

`immutable` and `slow` are orthogonal, and the combination is what carries
information. `immutable` says caching is **safe**; `slow` says caching is
**warranted**. Only both together justify persisting blocks to a disk cache
across sessions. A mutable-but-slow provider gets RAM caching with
invalidation and never touches the disk cache.

This also makes an existing silent assumption explicit:
`CachingBackend::file_id_for` (`vfs-cache/src/backend.rs:40`) mixes path, size,
and mtime into a cache key, which is only sound for immutable content.

`preferred_block` is the one field beyond the four capabilities originally
identified. It earns its place because `cached` otherwise has to guess, and a
CDN provider that wants 1 MiB fetches has no way to say so.

`ReadWrite` combined with `immutable` is contradictory and is rejected at
construction.

### Addressing

```rust
#[derive(Copy, Clone)]
pub struct VPath<'a> {
    pub root: RootId,   // u32
    pub rel: &'a str,   // normalized, '/'-separated, no leading slash, root is ""
}
```

A **root** is a real filesystem location the session virtualizes. Skyrim needs
at least two: the game directory and `Documents\My Games\Skyrim`. Roots are
declared at session setup with an integer id and a human name. The integer
travels on the hot path and the wire; the name exists for config, logs, and
error messages.

The root id is what lets one provider instance serve several roots and still
distinguish `[1, "foo/bar"]` from `[0, "foo/bar"]`. A provider mounted at a
single root ignores it.

Paths reaching a provider are always root-relative. Translation from NT reality
(`\??\C:\...`, CWD-relative opens, 8.3 short names) happens once, at the
shim/director boundary. Keeping that class of input confined to one layer is an
invariant, not a habit — a CWD-relative resolution bug previously produced an
empty load order.

### The interface

Everything past the read core defaults to `ST_NOT_SUPPORTED`.

`Handle` is an opaque `u64` minted by the provider that issued it. `SetAttr`
carries optional mtime and optional size, both `None` meaning "leave alone";
size is included because NT sets end-of-file by path as well as by handle.

```rust
pub type Handle = u64;

pub struct SetAttr {
    pub mtime: Option<i64>,
    pub size: Option<u64>,
}

pub trait Provider: Send + Sync {
    fn capabilities(&self) -> Capabilities;
    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32>;
    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32>;
    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32>;
    fn close(&self, h: Handle) -> Result<(), i32>;

    // Read | ReadWrite
    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32>;
    // SeqRead
    fn read_next(&self, h: Handle, buf: &mut [u8]) -> Result<usize, i32>;

    // ReadWrite
    fn write_at(&self, h: Handle, offset: u64, buf: &[u8]) -> Result<usize, i32>;
    fn set_len(&self, h: Handle, len: u64) -> Result<(), i32>;
    fn flush(&self, h: Handle) -> Result<(), i32>;
    fn mkdir(&self, p: VPath) -> Result<(), i32>;
    fn remove(&self, p: VPath) -> Result<(), i32>;
    fn rename(&self, from: VPath, to: VPath) -> Result<(), i32>;
    fn set_attr(&self, p: VPath, attr: SetAttr) -> Result<(), i32>;
}
```

A read-only zip provider implements five methods and declares `Access::Read`.

### New status codes

| Code | Name | Meaning |
|---|---|---|
| `-8` | `ST_NOT_SUPPORTED` | Method not implemented by this provider |
| `-9` | `ST_READ_ONLY` | No `ReadWrite` provider serves this path |

### Contract invariants

Enforced by the conformance suite (§8):

- Capabilities are constant for a provider's lifetime. Read once at mount,
  cached thereafter.
- Paths arrive normalized, forward-slash, no leading slash, root is `""`,
  original case preserved.
- Short reads are legal anywhere, not only at EOF. The director and combinators
  loop.
- `SeqRead` handles carry a single cursor, are not shareable, and reset only by
  reopening.
- Handles are scoped to the provider that issued them. Every combinator keeps
  its own handle namespace and never leaks a child handle upward.
- Declared capabilities must match implemented methods. Mismatch is an error at
  **registration**, not at first traffic.

### Case sensitivity

Providers are expected to match case-insensitively on Windows. Rather than make
that a capability, a `casefold` combinator wraps case-sensitive providers using
a `readdir`-built index. A Python dict-backed provider gets correctness by
adding one wrapper instead of reimplementing `vfs_core::fold`.

## 6. Primitives and composition

### One provider per root

`Director` drops its layer-ordered mount list and the reverse-iteration merge in
`getattr` / `readdir` / `open` (`vfs-director/src/director.rs:69-155`). Roots
become a flat non-overlapping map and resolution is one lookup. Everything that
was "merge across mounts" becomes an explicit `layered(...)` in the graph, where
it is visible. The `layer` field disappears from config.

### The object graph is the model

Config is a serialization of the graph, not the other way round.

```python
base    = vfs.zip("C:/GameLayers/base.zip")
scratch = vfs.disk("C:/scratch")
session.mount(0, vfs.layered(vfs.readonly(base), vfs.disk("C:/mods/SkyUI")))
```

```toml
[provider.base]
type = "zip"
path = "C:/GameLayers/base.zip"

[provider.scratch]
type = "disk"
path = "C:/scratch"

[[mount]]
root     = 1
provider = { type = "overlay", base = "base", upper = "scratch" }
```

The existing flat `[[source]]` list survives as documented sugar for "`layered`
of these, mounted at root 0", so current configs and tests keep working. Marked
deprecated.

### Primitive catalog

All Rust. All implement `Provider`. All constructible from any host language.

**Leaves (data sources):** `disk`, `zip`, `memory`, `remote` (gRPC), plus
host-authored providers.

**Combinators (take providers):**

| Primitive | Purpose |
|---|---|
| `layered(a, b, c)` | Topmost wins; `readdir` unions |
| `overlay(base, upper)` | Copy-up writes and whiteouts; reports `ReadWrite` |
| `router({pattern: p}, default)` | Pattern dispatch |
| `subdir(p, "Data")` | Expose a subtree as root — the one deliberate rewriter |
| `cached(p, opts)` | Block cache, RAM plus optional disk |
| `seekable(p)` | `SeqRead` → `Read` |
| `casefold(p)` | Case-insensitive index over a case-sensitive provider |
| `readonly(p)` | Demote `ReadWrite` → `Read`; protects a vanilla install |

Python and TypeScript write none of these. They write novel data sources — a
Steam CDN client, a mod-manager database — and compose the rest.

### Capability recomputation

Combinators derive their own capabilities from their children:

- `seekable` over `SeqRead` reports `Read`.
- `cached` passes access through and clears `slow`.
- `overlay` reports `ReadWrite` regardless of base.
- `layered` reports the weakest access across children, and `immutable` only if
  every child is immutable.
- `readonly` clamps access to `Read`.

### Flags advise; they do not mutate the graph

Rust provides primitives and the caller combines them, so silently inserting a
cache would contradict the model.

| Situation | Behavior |
|---|---|
| `slow` provider with no `cached` above it | **Warning** at mount time, naming the provider |
| `SeqRead` provider not wrapped in `seekable` | **Hard error** — the director cannot issue positional reads |
| Nested `cached` inside `cached` | Collapsed, not doubled |

`vfs.auto(p)` is an opt-in helper that applies the recommended wrapping and
reports what it did.

### Router asymmetry

A pattern route claims only some entries in a directory, so `router` cannot
dispatch `readdir` to a single child:

- `getattr` / `open`: first matching route wins, single dispatch.
- `readdir`: union across the default plus every route that could contribute to
  that directory.

This asymmetry is specified in the contract and has dedicated conformance cases
rather than being left to each implementation's judgment.

### Registry

`type` maps to a factory function. Built-ins (`disk`, `zip`, `memory`,
`remote`) register at startup. Hosts register their own via
`register_provider(name, factory)`. Registering a name already taken is a hard
error — silent shadowing of a built-in produces bug reports nobody can
reproduce.

## 7. The write path

The shim's *client* half already speaks writes: `open_write` and chunked
`write` over the ring exist (`vfs-shim/src/fuse_client.rs:243,258`), written for
the removed JVM overlay. Missing are the server half (`ring_dispatch` handles
only GETATTR/READDIR/OPEN/READ/CLOSE/HEARTBEAT) and the provider
implementations.

### Op routing

`Director::open` stops rejecting `OPEN_WRITE`
(`vfs-director/src/director.rs:128`) and gains dispatch for `OP_WRITE`,
`OP_SETATTR`, `OP_RENAME`, `OP_DELETE`, and `OP_MKDIR`. Each maps to the
provider method of the same name. No new wire opcodes are required.

### Open flags

| Flag | NT origin |
|---|---|
| `OPEN_CREATE` | `OPEN_ALWAYS` / `CREATE_ALWAYS` |
| `OPEN_EXCL` | `CREATE_NEW` |
| `OPEN_TRUNC` | `TRUNCATE_EXISTING` |
| `OPEN_APPEND` | `FILE_APPEND_DATA` |

**The director owns append**, not providers. It keeps a per-handle cursor
initialized to size at open and resolves append writes to a concrete offset
before calling `write_at`. Providers stay purely positional.

*Named limitation:* two handles appending to the same file concurrently can
interleave incorrectly. Games write logs from a single handle. If that stops
being true, per-path cursors are the fix.

### `overlay(base, upper)` semantics

| Operation | Behavior |
|---|---|
| `open` for write, in upper | Open in upper |
| `open` for write, whiteout present | Not found, unless `OPEN_CREATE` |
| `open` for write, in base | Copy whole file up, then open in upper |
| `open` for write, in neither | Create in upper if `OPEN_CREATE`, else not found |
| `remove`, in upper | Delete in upper |
| `remove`, visible in base | Write whiteout in upper |
| `remove` on a base directory | Whiteout hides the whole subtree (opaque semantics) |
| `rename` | Copy up source, rename within upper, whiteout the original |

**Copy-up is whole-file, not lazy per block.** Justified by the domain: the
files games write are INIs, saves, and logs; the multi-gigabyte files are
read-only. Block-tracked lazy copy-up is a real optimization and the wrong one
to build first.

**Copy-up needs a per-path lock.** Two concurrent opens-for-write must not both
copy up. This is specified because it is the kind of race that appears once in
a hundred launches and costs a day to find.

**Whiteouts keep the `.wh.<name>` convention** (`vfs-compose/src/overlay.rs:59`),
which now works over any `ReadWrite` provider rather than only a real
directory, so an in-memory upper gets deletes for free. Standard tradeoff: a
genuine file named `.wh.foo` is shadowed.

**`upper` must declare `ReadWrite`.** `overlay` validates this at construction
and fails there, not at first write.

### Read-only rejection, made discoverable

Strictness is only workable if discovery is a feature:

- Writes with no `ReadWrite` provider at that path return `ST_READ_ONLY`, which
  the shim maps to `STATUS_ACCESS_DENIED`. `STATUS_MEDIA_WRITE_PROTECTED` is
  more truthful, but applications handle access-denied gracefully and often
  mishandle write-protected media.
- Every rejection is recorded in `io_stats` by `(root, path)` with first-seen
  time and count, surfaced as `session.rejected_writes()` in Python and
  `vfs stats` in the CLI.
- The workflow: launch, ask what was rejected, add `overlay(...)` for those
  subtrees.
- **`dry_run_writes=True`** accepts and discards writes while recording them.
  Without it, discovery is one rejection per launch, and a game that dies on
  its first INI write never reaches the code that writes saves.

### Bulk writes

Reads have an arena fast path (`FLAG_READ_BULK`); writes currently chunk through
the ring payload. `FLAG_WRITE_BULK` mirrors the read path so a save file is not
written 60 KB at a time.

### Durability

`close` implies `flush` unless the provider overrides it. Provider `flush`
backs `NtFlushBuffersFile`. For the `memory` provider, contents remain readable
from the host after the session ends — that is the point of the read-write INI
case, and it is what the Python end-to-end test asserts.

## 8. The Python binding

Package `aethervfs`, built with maturin and PyO3 as an abi3 wheel so one
Windows wheel covers Python 3.8+. It is a host over `vfs-embed`, exactly like
`vfs.exe`. One session-lifecycle implementation, two callers.

```python
import aethervfs as vfs

class SteamCdn(vfs.Provider):
    caps = vfs.Capabilities(access=vfs.Access.SEQ_READ, immutable=True,
                            slow=True, preferred_block=1 << 20)
    def getattr(self, root, path): ...
    def readdir(self, root, path): ...
    def open(self, root, path, flags): ...
    def read_next(self, handle, n) -> bytes: ...
    def close(self, handle): ...

session = vfs.Session("skyrim")
session.add_root(0, "game", r"C:\Games\Skyrim")
session.add_root(1, "docs", r"C:\Users\me\Documents\My Games\Skyrim")

base = vfs.cached(vfs.seekable(SteamCdn(depot="489830")),
                  ram="512MiB", disk="C:/cache")
inis = vfs.memory({"Skyrim.ini": ini_bytes})

session.mount(0, vfs.layered(vfs.readonly(base), vfs.disk(r"C:\mods\SkyUI")))
session.mount(1, vfs.router({"*.ini": inis},
                            default=vfs.overlay(vfs.disk(docs),
                                                upper=vfs.disk(scratch))))

with session.launch("SkyrimSE.exe") as proc:
    proc.wait()

print(session.rejected_writes())
print(inis.read("Skyrim.ini"))     # what the game actually wrote
```

Everything except `SteamCdn` is a Rust primitive. That is the test of whether
§6 succeeded.

### GIL discipline

Director threads are native Rust and never touch Python unless a Python
provider is in the stack. When one is, `PyProvider` acquires the GIL per call,
so a Python provider serializes every director thread that reaches it. This is
why `slow` exists and why the mount-time warning matters: `cached` over a 1 MiB
block turns per-read GIL acquisitions into per-block-miss acquisitions.

Rust-side blocking (ring waits, disk I/O) runs under `allow_threads` so the GIL
is never held across a wait.

### Data transfer

`read_at` and `read_next` return `bytes`; Rust copies into the caller's buffer.
A writable `memoryview` would avoid the copy but makes every provider harder to
write, and with caching in front the copy is not the cost that matters.
Recorded as a later option if measurement disagrees.

### Errors

`vfs.VfsError(code)` maps to `ST_*`. Any other exception becomes `ST_IO_ERROR`
with its traceback logged. No exception crosses the FFI boundary uncaught.

### Registration-time validation

The binding inspects the class at `register_provider` / construction. Declaring
`ReadWrite` without defining `write_at` is an error there, with the session
never starting.

### Packaging

The wheel contains `vfs_shim_dll.dll` and `vfs_payload.dll`, because
`Session::launch` locates them beside the host binary. The wheel build
therefore builds the second workspace (§9) too.

Stale shim DLLs have silently produced wrong results before, so the binding
**verifies DLL identity at session start** — a build hash embedded in each DLL
and checked — rather than trusting whatever sits next to the module. Editable
installs point at the dev build output.

## 6b. Corrections to §6's catalog (2026-08-17, from the Node binding)

Exposing §6's primitives from a host language tested §6's central claim — that a
host composes Rust primitives and writes only the leaf. Three corrections.

**`readonly` and `seekable` were not primitives.** Both existed only as
`Capabilities` helpers (`read_only_clamp()`, `seekable()`) with unit tests and no
provider behind either. There was nothing to expose. Implementing them inside a
binding would have produced primitives the *next* binding must write again and
`vfs.exe` cannot reach — the opposite of what §6 claims. They are now
`vfs_compose::ReadOnlyProvider` and `vfs_compose::SeekableProvider`, both
conformance-clean. The `readonly` case is worth noting: its conformance run is
over a **writable** fixture, so the clamp is what makes the suite exercise the
read cases instead of the write ones.

**`casefold` is missing, and its absence silently corrupts §8's own example.**

The shim folds every vpath component before it crosses the ring.
`MemoryProvider` is case-sensitive by design (§10 says so). With no `casefold`
primitive between them, an injected child's write to `Skyrim.ini` lands **beside**
a host's seeded `Skyrim.ini`, as `skyrim.ini` — and nothing reports it. Not the
child, not `rejected_writes()`, not the filesystem. So:

```
inis = memory({"Skyrim.ini": ini_bytes})
...
inis.read("Skyrim.ini")   // returns what the HOST seeded, not what the game wrote
```

**§8's flagship example fails on its own filename**, and returns plausible data
while doing it. Isolated across five experiments (new versus existing paths,
`disk` versus `memory`, long lower-case names to rule out 8.3 shortening) and
recorded as a `todo` test asserting the behaviour a host is entitled to.

**Amended 2026-08-17 after the end-to-end example: the cost is wider than
`memory()`.** A host-authored provider must fold **its own lookups** too —
mutation-verified, and without it an injected child reads zero bytes and still
exits 0. So until `casefold` exists, *every host provider in every binding*
reimplements folding, and `memory()` is the one case a host cannot fix that way.
**This is the highest-value gap left in the catalog** — not a missing
convenience, but a correctness hole with no diagnostic.

**One further gap the example found: `read_file` had no root parameter.**
`Session::read_file` hardcoded `RootId::DEFAULT` while `readdir` already took a
root, so §8's own last line — reading back from root 1 — was unreachable. The
seam now has `read_file_at(root, vpath)`. Evidence it was a real gap rather than
a nicety: the `vfs-embed` seam test carried a hand-rolled open/read/close loop
against `session.kernel()` for exactly this reason. Note also that §8's
`inis.read(...)` is the wrong *shape*, not merely missing — a host reads through
the session, not by holding a provider object and querying it.

**§6's mount-time flag table was unimplemented workspace-wide.** The table
describes hard errors and warnings at mount time; none existed. The `SeqRead`
hard error now lives in `vfs_embed::Session::mount_at`. The rest of the table is
still unimplemented and should be treated as design intent rather than behaviour.

**§8's read-back is a session call, not a provider call** (2026-08-17, from
running §8's example end to end). The example's last line is
`inis.read("Skyrim.ini")` — a read issued against a provider object. Nothing in
the workspace offers that, and the shape is worse than it looks: `inis` is a
handle to an `Arc<dyn Provider>` with no root, no mount prefix and no graph
around it, so reading through it would answer a *different* question from the one
the game's writes went through. The reachable form is
`session.read_file_at(root, vpath)`, which reads the same composed graph the
child did — added for this, because `read_file` hardcoded root 0 while `readdir`
already took a root, and §8 mounts the INIs on **root 1**. So a host could list a
second root's graph and never read a byte out of it. Read §8's last line as
`session.read_file_at(1, "skyrim.ini")`, folded key included (§6b above).

## 8b. The Node binding — and why it comes first (decided 2026-08-16)

**Reordering.** §8's Python binding is still wanted and its content mostly
carries over, but **Node/TypeScript is now the first binding built.** The long-term
host is an Electron application's Node backend; plain Node is the near-term target
and Electron is treated as a later packaging concern.

**The reason for the order is threading, not preference.** PyO3 and N-API are not
the same shape of problem, and the stricter one should define the contract:

- **Python.** A director worker thread acquires the GIL, calls the provider,
  releases it. Blocking is fine, reentrancy is survivable, and `PyProvider` can
  implement the synchronous `Provider` trait almost directly.
- **Node.** JavaScript runs on a single event-loop thread. A director worker
  thread cannot call it directly; it schedules through an N-API threadsafe
  function and blocks until the loop resolves the call. So a synchronous
  `Provider::read_at` becomes **a block on a foreign scheduler**, and if a call
  ever originates *from* the JS thread into Rust and back into JS, it
  **deadlocks**.

Building Python first would let the boundary harden around assumptions the GIL
forgives and Node does not. Building Node first forces the contract to be stated
explicitly, after which Python is a straightforward second implementation.

### The threading contract

This is binding on every host, not only Node:

1. **A provider call must not be serviced by the loop that is blocked waiting
   for it.** *(Corrected 2026-08-17 by measurement — see §8c. An earlier version
   of this rule said "provider calls originate only on director worker threads,
   never the host's main thread", which names the wrong invariant: it
   over-restricts, because main-thread-to-worker is safe, and under-restricts,
   because a provider living in a worker deadlocks on a synchronous call from
   that same worker.)* The invariant is **loop identity**, not thread role.
2. **The host's provider call may block the calling director thread** for as long
   as the host scheduler takes. That is what `slow` is for, and why `cached` in
   front of a host provider is the expected deployment rather than an
   optimisation.
3. **No host exception or rejected promise crosses the FFI boundary uncaught.**

### Package and ABI

`napi-rs`, targeting **N-API** rather than raw V8 — ABI-stable across Node
versions and across Electron's bundled runtime, the direct analogue of choosing
an abi3 wheel for Python. Prebuilt binaries per platform; Electron compatibility
is verified when there is a real Electron host to test against, not designed for
speculatively.

### Data transfer and errors

`readAt`/`readNext` return a `Buffer`/`Uint8Array`; Rust copies out of it, the
same trade §8 makes for `bytes`. A `VfsError(code)` maps to `ST_*`; any other
throw or rejection becomes `ST_IO_ERROR` with the stack logged.

An `async` provider method returning a `Promise` is expected and supported — the
director thread parks until it settles. A provider that never settles hangs that
one director thread, not the session; that is a diagnosable failure and should be
counted, not merely survived.

### Registration-time validation

Identical in intent to §8: the binding inspects the object when it is
constructed, and declaring `ReadWrite` without a `writeAt` is an error there,
with the session never starting.

### What is unresolved and must be measured, not assumed

Whether an event-loop round trip per read is viable at real read volumes, or
whether a JS provider is only usable behind `cached`. This is measurable on a
spike and should be measured before the binding's shape is fixed — the answer
changes whether `cached` is a recommendation or a requirement.

## 8c. Measured: the Node boundary, and a cache defect it exposed

A spike on 2026-08-17 (Node v24.19.0, N-API 10, Ryzen 9 8945HS; three agreeing
runs plus a fourth) settled the question §8b left open. **Two of §8b's
expectations were wrong, both in the same direction — the boundary is cheaper
than assumed and the mitigation was the problem.**

### The boundary is cheap

- **Round trip 1.7–2.0 µs p50.** The recorded ring READ for 4 KiB is 9.7 µs, so a
  host provider adds roughly **20 % to a 4 KiB read** — not an order of
  magnitude. A `Promise` costs ~0.2 µs over a synchronous return; a full loop
  turn ~0.8 µs. The tail is real and worth knowing: max 130–400 µs, and a cold
  worker wake is 31–47 µs.
- **4 KiB sequential, no cache:** ~1370–1510 MiB/s on the main loop, 1604 on a
  single worker, **8983–10353 across eight workers**. These are boundary
  ceilings — the JS provider does no work.
- **Concurrency scales with loops and only with loops.** Eight director threads
  against one loop give p50 17.8 µs ≈ 8 × 2.2 µs: exactly serialised. One to
  four loops is 7.7×.
- **A busy main loop is catastrophic; a worker loop is immune.** Under ~1 ms of
  work per turn, main-loop servicing falls 1507 → 3.8 MiB/s (**370×**). A
  worker-serviced provider is unaffected (1449 MiB/s, p50 still 2.0 µs).

**So the guidance is: service host providers on a dedicated worker loop.** That
is what makes `cached` an optimisation rather than a necessity, and it is the
difference between a host that degrades under its own UI work and one that does
not.

### `cached` at the default block size is actively harmful

4 KiB sequential through `cached` with the default 1 MiB block measures
**24 MiB/s** — sixty times slower than the raw boundary. The cause is not the
FFI boundary at all: `vfs-cache/src/store.rs:120` **clones the whole block on
every hit**, so a 4 KiB read memcpys 1 MiB. A pure-Rust harness with no Node
anywhere measures 25.3 MiB/s against the bridge's 24.5 — **the cache is ~70×
more expensive than the boundary it exists to protect.**

Two further defects in the same function: an O(n) LRU scan per hit
(`store.rs:116-118`), and one process-wide mutex (`store.rs:59`) — cached
throughput is flat at 24→26 MiB/s from one to eight threads while p50 grows
linearly, 155 → 1139 µs.

**A 64 KiB block is the best configuration measured (1094 MiB/s.)** No existing
test catches any of this: they assert correctness, and correctness is intact.
Recorded as a defect in its own right, not as Node's problem — every cached read
in every session pays it today.

### Consequences for the API shape

A `ThreadsafeFunction` reference must travel through a Rust static, so **a bridge
handle is a process-global integer, not a JS object.** Registration is therefore
necessarily *a module path resolved inside the worker*, not an object instance
handed across — isolates share no JS objects. §8's Python sketch
(`session.mount(0, SteamCdn(...))`) does not translate directly to Node, and the
Node API should not pretend otherwise.

Confirmed while establishing that: napi-rs is context-aware **by construction**
(it exports `napi_register_module_v1`), the addon loads in nine Workers, and Rust
statics *are* shared across isolates — a freshly registered worker observed the
process-wide bridge count.

### The zero-copy door is open

§8 declines a writable `memoryview` for Python, on the grounds that it makes
every provider harder to write. For Node the mechanism is better than expected:
`SharedArrayBuffer` backing stores have identical addresses across isolates, and
a bare Rust thread wrote 65536/65536 bytes visible from either side. §8's
*reasoning* for declining is unaffected — this only records that the mechanism is
not the obstacle.

## 9. The `panic = "abort"` conflict

The workspace sets `panic = "abort"` for both profiles (`rust/Cargo.toml`),
forced by `vfs-payload` being `#![no_std]` with a custom panic handler that
cannot unwind.

PyO3 converts Rust panics into Python exceptions using `catch_unwind`, **which
does nothing under `panic = "abort"`**. As written, a panic anywhere in the
library would abort the host Python process instead of raising. A library that
can kill its embedder on a bad path is not productized.

**Fix:** exclude `vfs-payload` from the workspace and give it its own
`[workspace]` table, so the main workspace can use `panic = "unwind"`.

`vfs-payload` is the only crate that needs `panic = "abort"`. It is the only
`#![no_std]` crate with a `#[panic_handler]`, it has **zero dependencies**, and
it is already not a normal dependency of anything — `vfs-inject` builds it with
a nested `cargo build -p` precisely because of this constraint
(`vfs-inject/Cargo.toml:36`). `vfs-shim-dll` is a plain std cdylib and stays
where it is.

The crate directory does not move. Adding `exclude = ["crates/vfs-payload"]` to
the root manifest and an empty `[workspace]` table to the payload manifest is
sufficient, and building it with `CARGO_TARGET_DIR` pointed at the main target
directory keeps every existing artifact-location path working unchanged.

Knock-on work items:

- Nested `cargo build -p vfs-payload` invocations must become
  `--manifest-path crates/vfs-payload/Cargo.toml`:
  `vfs-inject/tests/common/mod.rs:22-34`, `vfs-directord/tests/e2e.rs:71`.
- `.github/workflows/ci.yml:14` and `README.md:27,97` build lines.
- The wheel build must drive both workspaces.

**A side benefit, not just a cost:** with unwinding available, the shim's hook
entry points can wrap their bodies in `catch_unwind` and convert a panic into
an error status. Today a panic inside a hook aborts the game process
unconditionally.

This is a prerequisite for the Python binding, not a cleanup.

## 10. Testing and conformance

### The conformance suite is the center of gravity

Today it is 62 lines asserting a fixture tree
(`vfs-source/src/conformance.rs`). It becomes a capability-parameterized suite:
given a provider and its declared capabilities, run exactly the cases those
capabilities promise.

**Cases are defined once, in Rust, and exposed through each binding** —
`assert_conformance(p)` in Rust, `aethervfs.testing.assert_conformance(p)` in
Python — so a Python-authored provider is held to identical standards without a
second copy of the suite that can drift.

| Declared | Cases |
|---|---|
| All | `readdir("")`, not-found by `getattr` and by `open`, entry metadata (kind and size) agreeing between `readdir` and `getattr`, short reads, handle isolation, capabilities constant across calls, a non-default root handled coherently |
| `Read` | Positional reads, offset past EOF, zero-length, unaligned |
| `SeqRead` | Cursor advances, reopen resets, positional read refused |
| `ReadWrite` | create/excl/trunc/append, write-then-read-back, `set_len` grow and shrink, rename, remove, mkdir, cross-root rename rejected, flush/close durability |
| `ReadWrite + immutable` | Rejected at construction |

**Root awareness is deliberately not a universal case.** A provider over one
backing store — a zip, a directory — correctly ignores the root id and serves
the same tree everywhere, which is exactly what §5 permits. A multi-root
provider may just as correctly report not-found for a root it does not serve.
Asserting that a given relative path resolves under an arbitrary root would
forbid the second and be passed trivially by the first, so the universal case
only requires that a non-default root produce `Ok` or `ST_NOT_FOUND` rather
than a crash or an unrelated status. That `VPath` actually delivers the root id
to the provider is verified separately, by a fixture built to serve different
content per root.

**Case-insensitive matching is also not a universal case.** Providers are
expected to match case-insensitively on Windows, but the suite compares names
exactly; a case-sensitive provider gains correctness from the `casefold`
combinator rather than from every provider reimplementing folding.

### Combinators run the same suite

Every combinator wrapped around a reference provider must itself pass
conformance:

```rust
assert_conformance(cached(disk(..)));
assert_conformance(overlay(zip(..), memory()));
assert_conformance(casefold(seekable(seq_fixture())));
```

The suite that validates providers validates the primitive library for free,
and capability recomputation is checked by construction rather than by
inspection.

Targeted tests still cover what conformance cannot see: `overlay`'s concurrent
copy-up race, `router`'s dispatch-versus-`readdir` asymmetry, `cached`
collapsing nested caches and keying on root, `seekable` reopening on a backward
seek.

### Test migration

**Stays in Rust:** unit tests, conformance case definitions, provider
internals, wire codecs, shim and hook internals.

**Moves to pytest:** end-to-end scenarios — composing a session, launching a
fixture executable, asserting observed I/O, the write-path scenarios, the
rejected-writes discovery loop, and the Skyrim launch. These are composition and
orchestration, which is what the library now exposes.

**`tools/gamectl.ps1` is kept, not rewritten.** It already handles the DPI,
foreground-steal, and lock-screen problems that silently break capture and input
when driving the game headlessly. pytest invokes it.

### Acceptance criteria

1. Every built-in provider and every combinator passes conformance.
2. A Python-authored provider passes the same suite, unmodified.
3. Python end-to-end: Skyrim launches under a composed session, shows the
   expected load order, writes an INI into a `memory` provider, and the host
   reads back what the game wrote after exit.
4. No regression against the read-path figures recorded in
   `rust/docs/benchmarks/`.
5. Zero clippy warnings.

## 11. Breaking changes

Deliberate, and all pre-1.0:

- `Backend` → `Provider`, plus the `VPath` signature change. Every crate in the
  workspace is touched.
- `layer` disappears from config. The flat `[[source]]` list survives as
  deprecated sugar.
- `source.proto` gains capabilities and write ops, with an explicit version
  field. Existing out-of-process plugins break, but they break loudly at
  handshake.
- `Director`'s layer-ordered mount merge is deleted.
- `vfs-payload` leaves the workspace (§9), so `cargo build -p vfs-payload` from
  the workspace root stops working and becomes a `--manifest-path` invocation.
- `RootMap::new(root)` (`vfs-redirect/src/lib.rs:19`) becomes multi-root and
  yields `(root_id, rel)`.

## 12. Staging

Each stage has a gate that must be green before the next begins.

| Stage | Work | Gate |
|---|---|---|
| 1 | Workspace split; `vfs-provider` crate (`Capabilities`, `VPath`, `Provider`); port existing backends to the new trait | All existing tests green, no behavior change |
| 2 | One provider per root; root ids through director, shim, and cache keys | Existing end-to-end green under multi-root |
| 3 | Write path: provider write ops, director dispatch, `overlay` copy-up, rejection diagnostics, bulk writes | Rust write end-to-end passes |
| 4 | `vfs-embed`; PyO3 binding; Python conformance runner; packaging | Python-authored provider passes conformance |
| 5 | Python end-to-end port including Skyrim; CLI slimmed onto `vfs-embed` | Acceptance criteria 1–5 |

Stage 1 is deliberately a no-behavior-change refactor. It touches every crate,
and mixing it with semantic change is how a migration like this goes sideways.

## 13. Deferred

Named so they are decisions rather than omissions:

- Lazy per-block copy-up in `overlay`.
- Zero-copy `memoryview` reads across the Python boundary.
- TypeScript and C# bindings. The pattern is meant to generalize; proving it
  is a later project.
- Per-path append cursors.
- Linux hosting.
- Hook-boundary `catch_unwind`. §9 predicted that `panic = "unwind"` would let
  the shim's hook entry points wrap their bodies in `catch_unwind` and convert
  a panic into an error status instead of taking down the host process. That
  wrapping was never implemented. The practical effect of `panic = "unwind"`
  today is the opposite of a safety net: a panic inside a hooked NT syscall
  now unwinds through live Rust stack frames — running `Drop` impls — in a
  real game process before it reaches the `extern "system"` boundary and
  aborts there, whereas under the old `panic = "abort"` it aborted immediately
  at the panic site. Until hook entry points actually call `catch_unwind`, a
  hook panic unwinds further than it used to.

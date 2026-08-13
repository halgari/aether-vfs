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
| All | Root scoping (`[0,"a"]` ≠ `[1,"a"]`), `readdir("")`, not-found, short reads, handle isolation, capabilities constant across calls |
| `Read` | Positional reads, offset past EOF, zero-length, unaligned |
| `SeqRead` | Cursor advances, reopen resets, positional read refused |
| `ReadWrite` | create/excl/trunc/append, write-then-read-back, `set_len` grow and shrink, rename, remove, mkdir, cross-root rename rejected, flush/close durability |
| `ReadWrite + immutable` | Rejected at construction |

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

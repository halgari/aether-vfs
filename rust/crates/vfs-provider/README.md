# vfs-provider

The provider contract: what a filesystem provider can do, how it is
addressed, and the conformance suite that holds every implementation — Rust
or, eventually, a host language — to the same standard.

This crate has no dependencies and does no I/O. It is a pure contract:
capability types, an addressing type, the `Provider` trait, status codes, and
a suite of assertions that runs against any `Arc<dyn Provider>`.

Design background: [`docs/superpowers/specs/2026-08-13-pluggable-providers-design.md`](../../../docs/superpowers/specs/2026-08-13-pluggable-providers-design.md),
§5 (the contract) and §6 (primitives and composition). That spec describes the
full end state across five stages; this crate implements Stage 1 — the
contract, addressing, capabilities, and conformance. Writes, the registry, and
the combinators-as-designed are later stages and are not implemented here (see
[below](#what-stage-1-does-not-include)).

## What a provider is

A `Provider` is anything that can answer filesystem questions for a virtual
tree: does this path exist, what is in this directory, give me a handle to
this file, read some bytes from it. Concrete examples elsewhere in the
workspace: a zip archive (`vfs-zip`'s `ZipProvider`), a directory on disk
(`vfs-director`'s `DiskProvider`), an out-of-process plugin reached over gRPC
(`vfs-source`'s `RemoteProvider`), and several read-only combinators in
`vfs-compose` that build a provider out of other providers.

A provider does not decide how it is combined with others, does not know
about mount tables, and does not know about the ring protocol that eventually
carries its answers to a game process. It only answers questions about the
one (or several) trees it serves.

## Capabilities

Every provider declares its `Capabilities` once, at construction, and that
declaration does not change for the provider's lifetime (`assert_conformance`
checks this):

```rust
pub struct Capabilities {
    pub access: Access,               // SeqRead | Read | ReadWrite
    pub immutable: bool,               // content never changes
    pub slow: bool,                    // reads are expensive
    pub preferred_block: Option<u32>,  // block-size hint; None = "caller decides"
}
```

**`access`** is one of three tiers:

- `SeqRead` — forward-only reads via `read_next`. There is no random access;
  a caller that needs positional reads must wrap the provider in a `seekable`
  combinator first.
- `Read` — positional reads via `read_at`. The common case.
- `ReadWrite` — positional reads and writes via `read_at` and `write_at`.
  There is no write-without-seek tier.

**`immutable` and `slow` are orthogonal, and the pair is the point.** They
answer two different questions, and it takes both a "yes" to justify the
expensive answer:

- `immutable` says caching is **safe** — the content never changes, so a
  cached block is good forever, even across process restarts.
- `slow` says caching is **warranted** — reads are expensive enough that
  paying for a cache is worth it.

Only when both are true does persisting blocks to a disk cache across
sessions make sense. A provider can be either without the other, and both
combinations are real:

- A local disk file is `Read`, fast, and mutable — no cache is warranted at
  all.
- A network provider serving content that can change underneath it might be
  `slow` but not `immutable` — worth a RAM cache with invalidation, never a
  disk cache, because a persisted block could go stale.
- A Stored zip entry is fast to seek into (no decompression) but `immutable`
  for the archive's lifetime — safe to cache, not obviously worth it on its
  own.
- A large, static, slow-to-fetch download (a CDN-hosted game depot, say) is
  both — the case a disk cache actually pays for.

Getting this distinction backwards is the single easiest way to introduce a
correctness bug (serving stale bytes from a disk cache for content that
turned out to be mutable) or a silent performance bug (never caching content
that never changes). `Capabilities::validate()` rejects the one
self-contradictory combination at construction — `ReadWrite` plus `immutable`
— since a provider cannot accept writes and also promise its content never
changes.

**`preferred_block`** is a hint, not an obligation: `None` means "the caller
decides". It exists so a provider that wants large sequential fetches (a CDN
client wanting 1 MiB reads instead of 4 KiB ones) has a way to say so to
whatever cache sits above it.

## Addressing: `(RootId, relative path)`

Every path a provider receives is a [`VPath`], not a bare string:

```rust
pub struct RootId(pub u32);

pub struct VPath<'a> {
    pub root: RootId,
    pub rel: &'a str,   // normalized, '/'-separated, no leading slash, "" is the root
}
```

A **root** is one virtualized filesystem location — a session may have
several (for Skyrim: the game directory and the `Documents\My Games\Skyrim`
folder are two separate roots). `RootId` exists so a single provider
*instance* can serve more than one root and still tell `[1, "a"]` from
`[0, "a"]` apart. A provider that only ever serves one backing store — a
single zip, a single directory — is entitled to ignore `root` entirely and
answer the same way regardless of which root it is asked about; the
conformance suite accepts that as one of two legal behaviors (the other being
`ST_NOT_FOUND` for a root the provider does not serve). What is not legal is
panicking, or returning some unrelated status, when asked about a root a
provider does not recognize.

`rel` arrives already normalized: forward-slash separated, no leading slash,
the provider's own root is the empty string `""`, and original case is
preserved (case-insensitive matching, where wanted, is a combinator's job —
see the design spec's `casefold`, not yet implemented).

## The five-method floor

`Provider` has many methods, but everything past a small read core has a
default body that returns `ST_NOT_SUPPORTED`:

```rust
pub trait Provider: Send + Sync {
    fn capabilities(&self) -> Capabilities;

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32>;
    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32>;
    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32>;
    fn close(&self, h: Handle) -> Result<(), i32>;

    // Everything below this line defaults to Err(ST_NOT_SUPPORTED):
    fn read_at(&self, ...) -> Result<usize, i32> { ... }
    fn read_next(&self, ...) -> Result<usize, i32> { ... }
    fn write_at(&self, ...) -> Result<usize, i32> { ... }
    fn set_len(&self, ...) -> Result<(), i32> { ... }
    fn flush(&self, ...) -> Result<(), i32> { ... }
    fn mkdir(&self, ...) -> Result<(), i32> { ... }
    fn remove(&self, ...) -> Result<(), i32> { ... }
    fn rename(&self, ...) -> Result<(), i32> { ... }
    fn set_attr(&self, ...) -> Result<(), i32> { ... }
}
```

A read-only provider that declares `Access::Read` therefore implements
exactly five methods: `capabilities`, `getattr`, `readdir`, `open`, `close`,
plus `read_at` (which also has a default, but a provider that never
overrides it can never actually serve a byte, so in practice five methods
plus `read_at` is the real floor for a useful provider). A `SeqRead` provider
implements `read_next` instead of `read_at`. Nothing needs to implement the
write methods, `mkdir`, `remove`, `rename`, or `set_attr` unless it declares
`ReadWrite`.

## A minimal provider

This is [`Minimal`](src/provider.rs), copied verbatim from this crate's own
test suite (the one thing worth keeping byte-for-byte in sync, so the example
below cannot drift from what the crate actually tests) — with only the
crate-relative status-code imports adjusted for use from outside the crate:

```rust
use vfs_provider::{
    not_found, Capabilities, DirEntry, Handle, Provider, Stat, VPath,
};

/// The minimum a read-only provider must implement.
struct Minimal;

impl Provider for Minimal {
    fn capabilities(&self) -> Capabilities {
        Capabilities::read_only()
    }
    fn getattr(&self, _p: VPath) -> Result<Option<Stat>, i32> {
        Ok(None)
    }
    fn readdir(&self, _p: VPath) -> Result<Vec<DirEntry>, i32> {
        Ok(Vec::new())
    }
    fn open(&self, _p: VPath, _flags: u32) -> Result<(Handle, u64, bool), i32> {
        Err(not_found())
    }
    fn close(&self, _h: Handle) -> Result<(), i32> {
        Ok(())
    }
    fn read_at(&self, _h: Handle, _o: u64, _b: &mut [u8]) -> Result<usize, i32> {
        Ok(0)
    }
}
```

`Minimal` serves an always-empty tree, so it demonstrates the trait floor but
is not itself conformance-clean — `assert_conformance` requires a provider to
expose the reference fixture tree (see below), which an always-empty `getattr`
can never do. It is object-safe, though, which is the other thing worth
checking early:

```rust
let p: std::sync::Arc<dyn Provider> = std::sync::Arc::new(Minimal);
assert_eq!(p.capabilities().access, vfs_provider::Access::Read);
```

## Running the conformance suite against your own type

`assert_conformance` runs the case subset implied by a provider's *declared*
capabilities — a provider that declares `Access::Read` is held to the
positional-read cases, not the sequential ones; a `SeqRead` provider is held
to the reverse. It panics, naming the failing case, on the first violation.

To conformance-test a real provider, make it serve the reference tree defined
by [`FIXTURE_FILES`] under its default root — two files, `a.txt` and
`sub/b.txt` — then call [`assert_conformance`]:

```rust
use std::sync::Arc;
use vfs_provider::assert_conformance;

let provider: Arc<dyn vfs_provider::Provider> = Arc::new(MyProvider::new(/* ... */));
assert_conformance(provider);
```

For a disk-backed provider, [`write_fixture_tree`] writes that same reference
tree to a real directory so there is something on disk to point the provider
at:

```rust
let dir = std::env::temp_dir().join("my-provider-fixture");
vfs_provider::write_fixture_tree(&dir);
let provider: Arc<dyn vfs_provider::Provider> = Arc::new(MyProvider::new(&dir));
assert_conformance(provider);
```

This crate's own tests include two in-memory reference providers exercised
by the suite itself — `vfs_provider::conformance::MemFixture` (positional) and
`vfs_provider::conformance::SeqFixture` (sequential) — worth reading as
worked examples of a provider that does pass conformance, one for each access
tier.

Every provider ported to this contract elsewhere in the workspace is held to
this same suite: `ZipProvider`, `DiskProvider`, `RemoteProvider`, and the
read-only combinators in `vfs-compose` (`InlineProvider`, `LayeredProvider`)
all call `assert_conformance` in their own test suites.

## What Stage 1 does not include

The design spec describes a larger end state than what exists in this crate
and in `vfs-compose` today. To avoid documenting aspiration as fact:

- **No write path.** `write_at`, `set_len`, `mkdir`, `remove`, `rename`, and
  `set_attr` exist on the trait (so the wire opcodes they mirror have
  somewhere to route to later) but nothing in the workspace implements them
  yet, and the conformance suite has no `ReadWrite` cases.
- **`vfs-compose`'s combinators are narrower than the spec's catalog.**
  `layered`, `router`, `subdir`, and `inline` exist; `overlay` exists but is
  read-only for now (`OverlayProvider::open` rejects `OPEN_WRITE`, and
  `capabilities()` always reports `Access::Read` regardless of what its base
  provider declares); `router`'s `readdir` is single-dispatch (it returns one
  route's listing, not the union across routes the design calls for).
  `seekable`, `cached` (as a combinator — `vfs-cache` has a `CachingProvider`
  today, but not as a `vfs-compose` primitive), `casefold`, and `readonly` are
  not implemented.
- **No registry.** There is no `register_provider` and no `type` string →
  factory mapping; providers are constructed directly in Rust.
- **No `vfs-embed` and no Python binding.** Those are Stage 4 in the design
  spec.

See the design spec's §12 staging table for the full plan.

[`VPath`]: src/path.rs
[`FIXTURE_FILES`]: src/conformance.rs
[`write_fixture_tree`]: src/conformance.rs
[`assert_conformance`]: src/conformance.rs

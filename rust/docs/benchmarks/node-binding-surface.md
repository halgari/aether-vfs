# The Node binding's performance surface — and a live boundary number again

`pnpm bench` in `rust/crates/vfs-node`. It is a **gate**, not a report: it prints
tables and then asserts on them, and the exit code is the product.

That shape is deliberate. The number this directory most wanted —
[node-ffi-round-trip.md](./node-ffi-round-trip.md)'s 1.7–2.0 µs — carries a
banner reading *"the harness that produced these no longer exists"* and *"nothing
in the tree reproduces them"*, because it was measured by a throwaway spike that
was later deleted. A benchmark nobody re-runs stops being true. So these numbers
are held by assertions that CI runs.

## The three tiers, and why they are separate

Wall-clock thresholds on a shared runner are how a gate becomes a flake. This
follows what `vfs-cache` already does — `hit_copy_cost` is deterministic and
allocation-counted, `hit_scaling_cost` asserts *ratios* and documents its limits:

| tier | what it asserts | why it survives a loaded machine |
|---|---|---|
| 1 | **deterministic counters** — bridge crossings, cache hits/misses, export counts | load cannot move an integer. This is the real gate |
| 2 | **ratios** between two things measured seconds apart in the same run | machine speed cancels |
| 3 | **absolute wall clock** | asserted only with large headroom; recorded always |

Tier 3 ceilings are set at multiples of what was observed, never just above it.
Following `provider.test.cts`, which asserts 500 µs against ~63 µs.

## What it refuses to do

`assertReleaseAddon()` aborts unless `aethervfs.node` is byte-identical
(sha256) to the current `target/release/aethervfs.dll`. The default `pnpm build`
installs a **debug** cdylib, and absolute numbers off a debug build are not worth
recording.

This is not a hypothetical guard. On its first real run it fired: a concurrent
`pnpm test` in the same working tree had rebuilt debug and overwritten the release
addon **three seconds** after it was installed. Without the check the suite would
have reported debug numbers as release ones.

## Numbers

AMD Ryzen 9 8945HS, 16 logical CPUs, Windows 11 26200, node v24.19.0, release,
2026-08-17. Medians; the suite also prints per-case minima.

### The JavaScript layer

| case | median |
|---|---|
| property read, forwarded export (getter) | 2.6 ns |
| property read, shadowed export (data property) | 3.0 ns |
| `version()` | 61 ns |
| `provider.handle` | 78 ns |
| `provider.kind` | 146 ns |
| `provider.jsLeaves()` | 157 ns |
| `statusName(2)` | 380 ns |
| `provider.capabilities()` | 685 ns |
| **wrapper coercion** (`readonly(provider)` − `readonly(handle)`) | **1.4 ns** |

The wrapper that accepts a `Provider` where Rust wants a `u32` costs
approximately nothing — worth knowing, because `check-types.mts` leg 4 exists to
stop that wrapper being lost, and "it is free" is the reason to keep it.

Marshalling dominates the rest: `capabilities()` returns an object and costs 11x
`version()`, which returns a string. Nothing here is a problem; it is the shape
of N-API.

### A graph of Rust primitives, host-side

| case | median |
|---|---|
| `getattr` — `memory()` | 1.14 µs |
| `readFile` 64 B — `memory()` | 1.69 µs |
| `readFile` 64 B — `layered(mem, mem)` | 1.74 µs |
| `readFile` 64 B — `router(*.bin → mem)` | 2.20 µs |
| `getattr` — `disk()` | 17.8 µs |
| `readdir` — `disk()` | 31.7 µs |
| `readFile` 64 B — `disk()` | 345 µs |
| `readFile` 64 B — `cached(disk)` | 357 µs |
| `readFile` 256 KiB — `disk()` | 440 µs |

**Composition is nearly free**: `layered` over two `memory()` leaves is 1.74 µs
against 1.69 µs for one, and `router` adds ~0.5 µs for the glob match.

**Two disk numbers deserve comment.** A 64-byte `disk()` read is 345 µs, ~200x
the same read out of `memory()`, and reading 4,096x more data (256 KiB) costs only
1.3x more. Both say the same thing: the cost is NTFS `open`/`close` (and whatever
the virus scanner does with it), not the transfer, and not this binding.

That is also why `cached(disk)` shows no wall-clock win at 357 µs despite the
counters proving every re-read is a cache hit with zero bytes from source. `cached`
absorbs the **read**, and a `readFile` is open + read + close — the same finding
`primitives.test.cts` records. A host wanting those 345 µs back needs an open
cache, not a bigger block cache.

**So the tier-3 absolute assertion is made against `memory()`, and the disk rows
are recorded rather than gated.** Gating on the filesystem would be gating on
whichever runner CI schedules.

### The JavaScript provider bridge

| case | median |
|---|---|
| `getattr` — JS provider on a worker loop | 23.1 µs |
| `getattr` — `memory()` (the control) | 1.12 µs |
| `readFile` 64 B — JS provider | 65.7 µs |
| `readFile` 64 B — `memory()` | 1.57 µs |
| `readFile` 64 B — `disk()` | 362 µs |
| **boundary crossing, by difference** | **22.3 µs** |

The crossing figure is a subtraction, and the divisor is a **measured fact rather
than an assumption**: `provider.stats().calls` says a host-side `getattr` is
exactly **1** provider call and a `readFile` is exactly **3** (open, readAt,
close). Both are asserted at tier 1. That is the part `provider.test.cts` could
not do when it divided its 63 µs by "3.1 crossings each".

#### Which recorded number this should be compared against

**47 µs, not 1.7–2.0 µs.** This is easy to get wrong, and getting it wrong first
produced a failing check here that looked like a 10x regression.

`jsprovider.rs` records task 5's four measurements beside the deadlock guard they
motivated: `main → main-loop` never settles, **`main → worker` settles in 47 µs**,
`worker A → its own loop` never settles, `worker A → worker B` settles in 32 µs.
The benchmark drives a session from the main thread against a provider on a worker
loop — that is the `main → worker` row. **22.3 µs against a recorded 47 µs.**

`node-ffi-round-trip.md`'s 1.7–2.0 µs is a *different* configuration: a director
thread parked on a condvar with a hot loop, not a cross-thread wake from main.
Comparing against it makes this path look ten times worse than it is.

#### The comparison that matters

A JS provider `readFile` is **0.19x** of `disk()` — five times *faster* — because
its leaf is a `Buffer` slice and `disk()`'s is a real NTFS open. That independently
reproduces `provider.test.cts`'s 5.3x, from a different harness.

Against `memory()` it is 42x, and that ratio is **recorded, not gated**: `memory()`
is a `HashMap` lookup with no I/O and no bridge, so the ratio is a statement about
`memory()` rather than about the bridge. Spec §8b's bet — that a host-language
provider is a reasonable thing to build — is the `disk()` comparison, and it holds.

## What is not measured here

* **The ring.** Every number is host-side. An injected process reading through the
  ring is covered by the examples and `provider.test.cts`, not by this.
* **Concurrency.** Task 5's finding that throughput scales with event loops and
  only with event loops is recorded in `node-ffi-round-trip.md` and not reproduced.
* **A busy main loop.** The 370x degradation §8c measured for a main-loop provider
  is why `providerWorker()` is the recommended shape; this benchmark only ever uses
  that shape.

## Related

* [node-typescript-js-layer.md](./node-typescript-js-layer.md) — did the
  TypeScript migration cost anything? (No. 3.9 ns on a property read.)
* [node-ffi-round-trip.md](./node-ffi-round-trip.md) — the historical spike.
* [block-cache-hit-cost.md](./block-cache-hit-cost.md) — the `vfs-cache` hit path.

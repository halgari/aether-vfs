# Block cache hit cost — the 110x delta

**What changed:** `vfs-cache`'s hit path. Three defects, all measured, all in
`BlockCache::get`: it cloned the whole block on every hit, it scanned the LRU
ordering on every hit, and the whole cache sat behind one process-wide `Mutex`.

**Why it went unnoticed:** the correctness suite passed throughout, before and
after. It asserted hit counts, invalidation, and capability propagation, and
never measured cost. The defect was found by a Node FFI spike that was measuring
something else entirely and noticed that turning `cached` **on** made a provider
about 60x slower.

**Standing rule for this file:** do not regress these figures. The assertions
that hold them are `crates/vfs-cache/tests/hit_copy_cost.rs` (deterministic,
allocation-counted) and `crates/vfs-cache/tests/hit_scaling_cost.rs` (wall-clock
ratios, with their thresholds and known limits documented in the file).

---

## Host

AMD Ryzen 9 8945HS, 16 logical CPUs, Windows 11 26200. Release builds. Harness:
`spike-node/cache-cost` — a plain Rust binary, no Node, no N-API, whose leaf
provider memcpys from an owned `Vec`. 64 MiB swept per configuration.

**That harness has been deleted.** It lived in its own cargo workspace that
nothing in `rust/` compiled, held a path dependency on the most-churned crate of
stage 4, and produced a gitignored artifact — so a fresh clone could not run it
even by hand. The figures below are kept as the recorded before/after; what
*enforces* them is CI:

```powershell
cargo test -p vfs-cache --release --test hit_copy_cost --test hit_scaling_cost
```

Those two are the standing rule stated above, and they run on every push. Figures
below are from single runs that agreed with repeats to within a few percent,
except the 8-thread row, which is noted inline.

---

## 1. Block-size sweep, 4 KiB reads

The headline. `blk=1024K` is `DEFAULT_BLOCK_SIZE`.

| block | before MiB/s | after MiB/s | before p50 | after p50 |
|---|---|---|---|---|
| 4 K | 2880 | 2320 | 0.60 µs | 0.60 µs |
| 16 K | 1581 | 2445 | 1.40 µs | 0.10 µs |
| 64 K | 1357 | 2698 | 1.50 µs | 0.10 µs |
| 256 K | 600 | 3475 | 4.20 µs | 0.10 µs |
| **1024 K (default)** | **24.4** | **2692** | **152.40 µs** | **0.10 µs** |
| raw leaf, no cache | 51898 | 51898 | ~0 | ~0 |

**110x at the default block size**, and a 1524x drop in p50.

**Read the shape of the sweep, not just the last row.** Before, throughput
tracked block size across five configurations — because the per-hit cost *was*
the block-sized clone. After, the sweep is flat: 2320 to 3475 MiB/s with no trend
in block size, because a hit is now O(1) in it. That flattening is the evidence
that the clone was the whole mechanism, which the spike inferred from arithmetic
but could not confirm without the fix.

It also settles the block-size question the spike raised. 64 KiB won its sweep at
1094 MiB/s and looked like a better default; post-fix, 64 KiB and 1 MiB measure
at parity (2698 vs 2692) and 4 KiB is the *slowest* row, because small blocks buy
nothing and cost extra misses. `DEFAULT_BLOCK_SIZE` therefore stays at 1 MiB, and
what changed instead is that a provider's declared `Capabilities::preferred_block`
now selects the block size (clamped to 4 KiB..4 MiB) — it was declared,
propagated, and then ignored by the one component it was addressed to.

## 2. Concurrency — 4 KiB reads, 64 KiB blocks, one shared cache

Aggregate across threads, each sweeping the file through its own handle.

| threads | before MiB/s | after MiB/s | before p99 | after p99 |
|---|---|---|---|---|
| 1 | 2012 | 19670 | 4.0 µs | 0.5 µs |
| 2 | 1736 | 28684 | 18.2 µs | 0.5 µs |
| 4 | 1405 | 53434 | 46.2 µs | 0.5 µs |
| 8 | 1277 | 23322 | 123.1 µs | 8.3 µs |

Before, **throughput fell as threads were added** — the cache converted
thread-parallel reads into a queue, which is the same defect the spike saw as a
flat 24→26 MiB/s with p50 growing linearly 155→1139 µs. After, 1→4 threads scales
2.7x and p99 is flat. The 8-thread regression is CPU oversubscription with a
memcpy-bound leaf (8 readers plus the main thread on 16 logical cores), not a
cache limit; it was present in the spike's own thread sweep for the same reason.

## 3. Per-hit cost against resident block count

One hot block re-read while `n` other blocks stay resident, 4 KiB blocks. This is
the worst case for a front-to-back LRU scan and an ordinary access pattern (a
header or index re-read while the rest of a file stays cached).

| resident blocks | before p50 | after p50 |
|---|---|---|
| 64 | 0.20 µs | 0.10 µs |
| 1 024 | 0.70 µs | 0.10 µs |
| 16 384 | 6.80 µs | 0.10 µs |

**68x at 16 384 resident blocks, and flat where it was linear.**

A note for whoever measures this next: the *sequential* sweep does not show this
defect, and looking at it alone would have cleared the LRU scan wrongly. Cycling
through blocks in order means the block you want is the least recently used one,
sitting at the front of the deque, so `position()` finds it at index 0 and the
O(n) scan is accidentally O(1). The pattern has to re-read a block to expose it.

---

## What was done

* **The copy.** `BlockCache::get` returns `Block = Arc<[u8]>`. A hit is a refcount
  bump; the caller copies only the range it asked for, after the lock is
  released.
* **Eviction.** CLOCK (second chance) with a reference bit per entry. A hit sets
  one bit and never touches the ordering, so it is O(1) exactly rather than
  amortised. Eviction sweeps a hand over a ring and is amortised O(1). This
  trades exact LRU for an approximation, deliberately: it is what allows the
  reference bit to be set through a shared borrow.
* **The lock.** `RwLock` per shard, sharded by a hashed key, and **hits take the
  shared lock** — the more important half of the two. Measured separately: with
  one shard and an exclusive hit path, 4-thread scaling efficiency is 0.20; with
  one shard and a shared hit path, 0.47; sharded, 0.58-0.95.
* **The counters.** `hits` and `bytes_from_cache` moved onto the shard. Two
  process-wide `AtomicU64` increments per hit were, once the lock was fixed, the
  largest remaining limit: they held 4-thread scaling to 1.35x and moving them
  lifted it to 2.1x with nothing else changed. A hit is ~70 ns, so a contended
  counter is not a rounding error on it.

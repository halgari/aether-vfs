# Pre-merge review — `vfs-cache` block-cache rewrite (`feat/stage4-embed`)

Commits reviewed: `ad5c1e4` (perf), `d2e5b77` (spike deletion + benchmark doc).
Files: `src/store.rs`, `src/provider.rs`, `src/lib.rs`,
`tests/hit_copy_cost.rs`, `tests/hit_scaling_cost.rs`,
`docs/benchmarks/block-cache-hit-cost.md`, `docs/benchmarks/README.md`.

**Verdict: the concurrency and eviction machinery is sound and the three perf
defects are genuinely fixed and genuinely regression-gated — but the shipped
benchmark prose contains numbers that contradict each other and the code, a
pre-existing disk-tier invalidation hole survives in rewritten code that now
claims to be correct, and the new `preferred_block` plumbing can silently turn
the cache off entirely. Do not merge the docs as written.**

---

## What I ran

* `cargo test -p vfs-cache` — 19 unit + 4 integration tests, all pass (debug).
* `cargo test -p vfs-cache --release --test hit_copy_cost --test hit_scaling_cost` — pass.
* `cargo clippy -p vfs-cache --all-targets -- -D warnings` — clean.
* **Fail-first verification**: reconstructed the pre-`ad5c1e4` `store.rs`/`provider.rs`
  in a scratch crate, kept the new test files, ran them in release. Results below.
* **Scratch probes** (13 probes, scratchpad only, nothing committed): shard-budget
  admission, concurrent same-key insert at 2/8/16 threads, fixture capacity,
  byte-exact range arithmetic over 13 offsets × 9 lengths in cold and hot passes,
  global budget under an 8-thread put storm, hash-skew capacity loss, eviction of
  a borrowed block, eviction progress on an all-referenced ring, disk-tier
  invalidation, and a direct throughput sweep on the host the benchmark doc names.

## What held up (priorities 1–3)

Reporting these because they were the highest-risk items in the brief and they
check out:

* **Concurrent insert of the same key is clean.** 2/8/16 threads racing `put` and
  `get` on the same 37 keys, 2000 rounds each: no double admission (`ram_blocks`
  stayed at exactly 37), no torn reads, no partial blocks, and `ram_bytes ==
  ram_blocks * block_size` exactly in every run. The `map.remove` +
  `ring.retain` replace path in `insert_ram` (store.rs:377-385) does preserve the
  1:1 invariant.
* **Shard boundaries are computed by one function** (`BlockCache::shard`,
  store.rs:206) used by `get`, `insert_ram` and the test alike; `invalidate_file`
  sweeps all shards. No insert/lookup divergence exists.
* **No lock nesting.** `get`'s read guard is dropped at the end of its `let`
  statement before the disk read, and the disk path's second read guard closes
  before `insert_ram` takes the write lock. No read-then-write on one thread.
* **The memory bound is enforced, not merely intended.** Floor division makes
  `n * shard_budget <= ram_budget`; an 8-thread, 160,000-put storm landed at
  exactly 100% of budget and never above.
* **Eviction terminates and makes progress.** An all-referenced 8-entry ring
  accepting a budget-sized block completed in 1.1 µs, cleared every bit, evicted
  all 8, admitted the new block, and held the bound.
* **Evicting a borrowed block is memory-safe.** The reader's `Arc` keeps the
  payload alive; bytes are unchanged and the pointer is stable across eviction.
* **Range→block arithmetic is byte-exact.** 13 offsets (0, 1, block−1, block,
  block+1, 2·block±1, EOF−1, EOF, EOF+1, 1<<40) × 9 lengths (0, 1, 7, 4095, 4096,
  4097, 8192, whole file, file+block) × cold and hot passes, against a file
  deliberately sized `4096*3 + 137`: every return value matched the expected
  clamp, every byte matched, and nothing was written past the returned `n`. No
  off-by-one found.

## Findings

### Critical

**C1 — `store.rs:308-334` — `invalidate_file` misses blocks that live only on
disk, so a write is followed by a stale read. VERIFIED.**

The key list is built from `g.map.keys()`, so a block already evicted from RAM
keeps its `.blk` file, and the next `get` re-serves it from the disk tier.

Probe H (`block_size: 64, ram_budget: 64, disk_dir: Some(..)`): two puts, the
first evicted from RAM; before invalidation both `1_2_0.blk` and `1_2_1.blk` are
on disk; after `invalidate_file(1, 2)` **`1_2_0.blk` is still there** and
`get(k0)` returns 64 bytes of the pre-invalidation value.

Probe I is the end-to-end consequence through the real path: a `CachingProvider`
over a writable leaf, block 0 warmed then evicted, `write_at(0, [0x55; 16])`
returns success, and the immediately following `read_at(0, ..)` returns `0xAA` —
the pre-write bytes.

This is **pre-existing** (the pre-diff `invalidate_file` had the same structure:
keys from the RAM map, `remove_file` per key). But this diff rewrote the function
and `provider.rs:228-230` now asserts of the invalidation path *"this is coarser
than strictly necessary, but it is correct"* — which is verifiably false. And
`disk_tier_roundtrip` (store.rs:666) is the only disk test and never calls
`invalidate_file`, which is why it has stayed invisible.

Mitigating: `disk_dir` is only reachable from `cached({diskDir})`, whose doc
(`vfs-node/src/primitives.rs:257-262`) says it is only worth setting for a
provider declaring both `immutable` and `slow`. Nothing enforces that, and
`CachingProvider` accepts writes on any provider whose inner accepts them.

**C2 — `docs/benchmarks/block-cache-hit-cost.md:42-56` — the §1 "after" column,
and therefore the "110x" headline, contradict the same table's p50 column, the
document's own §2 table, and direct measurement. VERIFIED (as a
self-contradiction, and by independent measurement on the documented host).**

Three independent checks, all failing:

1. *Internally.* The `1024 K` row reads `after 2692 MiB/s` and `after p50
   0.10 µs` for a 4 KiB read. 4096 B / 0.10 µs = ~39,000 MiB/s. The two columns
   in one row disagree by ~14x. The **before** rows *are* self-consistent
   (4096 B / 152.40 µs = 25.6 MiB/s ≈ the 24.4 reported), so the instrument was
   coherent before the fix and is not after.
2. *Against §2.* Line 78 gives `1 thread, after = 19670 MiB/s` for 4 KiB reads
   through 64 KiB blocks. Line 50 gives `2698 MiB/s` for 4 KiB reads through
   64 KiB blocks. 7.3x apart, same document, same shape of measurement, no
   reconciliation offered.
3. *Against the code.* Measured on this machine — which is the host the doc names
   (Ryzen 9 8945HS, 16 logical CPUs, Windows 11 26200, release): a cached 4 KiB
   sequential sweep through `CachingProvider` runs at **34,407 MiB/s** (4 KiB
   blocks), **45,307** (64 KiB), **45,944** (1 MiB), against a raw leaf at
   **84,813**. The doc's after column (2320–3475) is 10–14x low. Its p50 column
   (0.10 µs) matches my measured 0.085–0.114 µs almost exactly.

Consequences: the title, the README index line (`README.md:15`), and line 55's
**"110x at the default block size"** are all computed as 2692/24.4 from a figure
that is wrong by ~14x. The real delta is larger, not smaller — ~1880x by my
numbers — so this **understates** the work, but it is still a wrong number under
a "do not regress these figures" standing rule. And **"a 1524x drop in p50"**
(line 55) is derived from a value pinned at the instrument's quantization floor:
every single "after" p50 in §1 and §3 is *exactly* 0.10 µs, which is a resolution
artifact, not a measurement. The harness is deleted, so none of §1 can be
re-derived.

**C3 — `block-cache-hit-cost.md:30-36`, `README.md:30-37`,
`node-ffi-round-trip.md:58` — all three present a `--release` command as what CI
enforces; CI runs no release test at all. VERIFIED.**

`block-cache-hit-cost.md` line 30 says *"what \*enforces\* them is CI:"*, shows
`cargo test -p vfs-cache --release --test hit_copy_cost --test hit_scaling_cost`,
and adds *"they run on every push"*. `README.md:32-37` repeats the block and adds
*"Both run on every push. Prefer **release** builds."* `node-ffi-round-trip.md:58`
says the two tests are the ones *"which CI runs"*.

`.github/workflows/ci.yml` has exactly two invocations that reach `vfs-cache`:
line 36 `cargo test` (Windows, whole workspace) and line 137
`cargo test -p vfs-ipc -p ... -p vfs-cache ...` (Linux). **Both are debug.**
Neither passes `--release`, neither names the tests.

Precisely: "they run on every push" is **true** (in debug); the shown release
invocation is **not** what CI runs, so the release figures the docs frame as
CI-enforced are not enforced by anything. The distinction matters because
`hit_scaling_cost.rs:29` itself concedes the debug caveat, and the wall-clock
gates are being measured on unoptimised code.

### Important

**I1 — `store.rs:365-373` + `provider.rs:50-54` — a `preferred_block` larger than
`ram_budget / shard_count` silently disables the cache and amplifies reads by up
to 1024x. VERIFIED.**

`insert_ram`'s guard is now `len > self.shard_budget`, where `shard_budget =
ram_budget / n`. `provider.rs` decoupled the *stored* block size from
`cfg.block_size`, so `len` is no longer bounded by what `n` was computed from.

Probe A, three of four realistic configurations:

| cache `block_size` | `ram_budget` | hint | shards | shard budget | result |
|---|---|---|---|---|---|
| 4 KiB | 64 MiB | 4 MiB | 64 | 1 MiB | `hits=0 ram_blocks=0`, **20 MiB pulled from the leaf for five 4 KiB reads** |
| 64 KiB | 64 MiB | 4 MiB | 64 | 1 MiB | same |
| 1 MiB (default) | 64 MiB | 4 MiB | 8 | 8 MiB | ok, `hits=4` |
| 4 KiB | 128 KiB | 1 MiB | 4 | 32 KiB | `hits=0`, 5 MiB pulled for five 4 KiB reads |

Every `put` returns at the guard, so nothing is ever cached, `get` always misses,
and every 4 KiB request re-fetches a full 4 MiB block from the source the cache
exists to protect — forever. Pre-diff the guard was `len <= cfg.ram_budget`
(64 MiB) and the same configurations cached fine, so this is a regression.

Reachable from `vfs-node`'s `cached({blockSize, ramBytes})`
(`primitives.rs:284`) whenever `blockSize < preferredBlock / 8` — e.g. a host that
lowers `blockSize` to cut read amplification while wrapping a remote source that
declares 4 MiB frames. The default 1 MiB `block_size` is safe only by arithmetic
accident (`shard_budget >= 8 * cfg.block_size` and the hint clamp is 4 MiB).

The comment at store.rs:367-370 — *"With more than one shard this is unreachable
(a shard holds at least MIN_BLOCKS_PER_SHARD blocks)"* — is false for exactly
this reason: it assumes stored blocks are `cfg.block_size`, which `provider.rs`
no longer guarantees. No test covers a mismatch beyond 8x.

**I2 — `store.rs:127-130, 145-147` — sharding silently makes 12–14% of
`ram_budget` unusable, including at the default config. VERIFIED.**

Probe F, filling exactly `ram_budget / block_size` blocks of one file:

| block | budget | shards | nominal | resident | kept | evicts |
|---|---|---|---|---|---|---|
| 1 MiB | 64 MiB (**default**) | 8 | 64 | 56 | 88% | 8 |
| 64 KiB | 32 MiB | 64 | 512 | 439 | 86% | 73 |
| 4 KiB | 64 MiB | 64 | 16384 | 16033 | 98% | 351 |

A working set that exactly fit before now evicts. `MIN_BLOCKS_PER_SHARD`'s doc
says it exists to stop *"hash imbalance evict[ing] blocks a single global budget
would have kept"* — it bounds that loss, it does not prevent it, and the residual
is nowhere documented. The `shard_budget` field doc's claim that the bound "still
holds exactly" is true in the direction it states (`ram_bytes <= ram_budget`) and
silent about the other direction.

**I3 — `tests/hit_scaling_cost.rs:200-292` — the branch's only concurrency
assertion is its flakiest test, and CI runs it in the configuration where its own
skip guard does not fire. VERIFIED.**

Fail-first check (against the reconstructed pre-diff cache, release): **FAILS at
0.24 vs the 0.40 gate** — it does catch defect 3.

Margin on the fixed code, 8 replications of the exact measurement on an idle
16-core box: per-attempt efficiency **0.438 … 1.458** — a 3.3x spread, driven by
the *control's* 1-thread number swinging 1.7x (20.5M–34.9M ops/s) and its 1→4
scaling swinging 1.77x–5.64x. Best-of-4 landed at 0.943 (2.36x above the gate);
the worst single attempt sat **1.1x** above it. Best-of-N is doing real work here.

CI runs this (a) in **debug**, and (b) on `ubuntu-latest`, whose standard hosted
runner is **4 vCPU** — exactly `THREADS`, so the `cores < THREADS` skip at
line 224 does *not* fire, and the test runs 4 readers plus a main thread on 4
shared vCPUs. That is the same oversubscription condition
`block-cache-hit-cost.md:86-88` blames for its own 8-thread regression.

**I4 — `tests/hit_scaling_cost.rs:16` — "the thresholds sit an order of magnitude
away from both outcomes" is false for the concurrency gate, and the same file
says so 200 lines later. VERIFIED.**

Measured: defect 0.24, gate 0.40, best-of-4 fixed 0.943 — 1.67x and 2.36x, under
4x total separation. Line 216 states the accurate figure (*"2x above the
defect"*), which contradicts line 16. The claim is accurate for the other two
tests (residency: 40.4x defect vs 4.0 gate vs ~1.05 fixed; allocation: 256 B/B
defect vs ~0 fixed). Ranked Important rather than Critical only because the
correct number is present in the same file for a reader who gets that far.

**I5 — `block-cache-hit-cost.md:86` vs `:127-129`, and `src/lib.rs:44-50` vs the
§1 table — four numbers for two measurements, none cross-referenced.**

* Post-fix 4-thread scaling is **2.1x** at line 129 ("moving them lifted it to
  2.1x with nothing else changed" — i.e. the final state) and **2.7x** at line 86
  (and 19670→53434 = 2.72x in the table). Both presented as the same quantity.
* `lib.rs:47-50` cites *"2387 MiB/s at 4 KiB blocks against 2646 at 1 MiB,
  measured on the same harness"*; the §1 table gives 2320 and 2692 for that
  sweep.
* `lib.rs:44` and `block-cache-hit-cost.md:65` both give the pre-fix 64 KiB
  figure as **1094 MiB/s**; the table two lines above line 65 gives **1357**.

**I6 — test gaps.** No test exercises a cache/provider block-size mismatch beyond
8x (I1); no test calls `invalidate_file` with `disk_dir` set (C1); nothing asserts
usable capacity against `ram_budget` (I2).

### Minor

**M7 — `store.rs:222-246`** — `stats()` reads shards one at a time, so it is not a
consistent snapshot, and it *silently omits poisoned shards* — the direction that
makes the suite's own `ram_bytes <= budget` assertions pass for the wrong reason.

**M8 — `store.rs:264, 374`** — RwLock poisoning turns one shard into a permanent
1/N cache hole with no signal (`Err(_) => None` on read, `else { return }` on
write). Same class as the pre-diff `Mutex`, but now silently partial rather than
total, which is harder to notice.

**M9 — `store.rs:145-147`** — `ram_bytes` under-reports live memory: an
evicted-but-borrowed block's payload stays resident (the reader holds the `Arc`)
while no longer counted. Bounded by concurrent borrowers, so the documented bound
is on *counted* bytes, not RSS. Verified in probe K; worth one sentence in the
field doc that currently says the bound "holds exactly".

**M10 — `store.rs:343-352`** — the `chances` counter is redundant. Because
`evict_to_fit` runs under the shard's *write* lock, no reader can re-set a
reference bit mid-sweep, so `swap(false)` alone bounds the sweep at two laps. The
doc comment's termination argument credits `chances` for what `swap` already
guarantees. Harmless, but the stated reasoning is not the real one.

**M11 — `store.rs:371`** — the early `return` on `len > shard_budget` now leaves
any pre-existing entry for that key in place, where the pre-diff code removed it
before deciding not to insert. Unreachable through `CachingProvider` (a `put`
always follows a `get` that returned `None`), but a behaviour change in a public
API.

**M12 — `tests/hit_copy_cost.rs:176`** — the tight bound `per_hit < BLOCK / 64` is
16 KiB. Against a 1 MiB block it separates cleanly (verified: the defect
allocated 1,048,576 B/hit against a 16,384 B gate) but it permits a 16 KiB
per-hit allocation — 4x the bytes delivered — so a reintroduced copy of a small
block, or a per-hit temp buffer, passes. The module doc's "expected value is ~0"
is right; the gate is looser than that.

**M13 — `provider.rs:157`** — `offset + buf.len() as u64` overflows in debug for
`offset` near `u64::MAX`. Unreachable in practice: it requires `rec.size` near
`u64::MAX`, and `offset >= rec.size` returns earlier.

**M14 — `store.rs:297**` — `bytes_from_source` counts every duplicate insert of
the same key, and counts blocks that were never cached at all (probe J: 12.5 MiB
attributed for 3 refused puts of one 4 MiB block). Pre-existing.

---

## Priority 4 answered: do the cost tests measure cost?

Verified by running each against the reconstructed pre-`ad5c1e4` cache, release:

| test | vs the original defect | margin on fixed code | flaky on a shared machine? |
|---|---|---|---|
| `hit_copy_cost::a_cache_hit_does_not_allocate_a_block_sized_buffer` | **FAILS** — "256.0 allocated bytes per delivered byte", exactly the documented signature | ~0 allocated vs a 16 KiB/hit gate | **No.** Allocation-counted, no timing, ratio-based, own test binary for the `#[global_allocator]`. |
| `hit_scaling_cost::per_hit_cost_does_not_grow_with_resident_block_count` | **FAILS at 40.4x** vs the 4.0 gate (151 ns/hit at 64 resident, 7032 at 16384) — matches the doc's "34x" | measured 0.99–1.10 over 3 rounds; ~4x headroom | **Low.** Both sides touch the same one L1-resident 4 KiB block; the only difference is HashMap size. |
| `hit_scaling_cost::concurrent_hits_scale_as_well_as_the_same_work_without_a_cache` | **FAILS at 0.24** vs the 0.40 gate | best-of-4 0.943; **worst single attempt 0.438**, 1.1x above the gate | **Yes — see I3.** |
| `hit_scaling_cost::repeated_reads_of_a_slow_source_stay_cheap` | **passes** on the old code too | 8x gate against a predicted 64x | No. Correctly labelled a regression guard, not fail-first. |

Also confirmed: the fixtures the timing tests depend on do not evict. All three
`filled()` configurations reach full residency (64/64, 64/64, 16384/16384), so
`filled`'s `assert_eq!(resident, blocks)` is not silently converting these into
measurements of eviction — but note the 64×64 KiB-in-32 MiB fixture sits at
8 blocks per shard against a per-shard capacity of exactly 8, which is one hash
collision away from tripping that assertion if `MIN_BLOCKS_PER_SHARD` or the hash
ever changes.

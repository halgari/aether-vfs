# Node ↔ Rust provider round trip — a recorded historical measurement

**Status: historical. The harness that produced these numbers no longer exists.**

These figures come from `spike-node/`, a throwaway cargo workspace built for
stage 4 task 5 to answer one question before the `aethervfs` addon was designed:
*how expensive is it to serve a provider call from JavaScript?* The spike was
deleted once its findings had all landed — the deadlock guard it motivated is in
`vfs-node/src/jsprovider.rs`, the `vfs-cache` defect it stumbled onto is fixed
and regression-tested in `vfs-cache/tests/`, and the addon itself supersedes the
bench harness. What could not be re-derived from anything that remains is the
**bare round-trip number**, so it is written down here.

Nothing in the tree reproduces these. Treat them as a recorded observation with a
named date and machine, not as a benchmark to re-run. The closest live figure is
in `vfs-node/test/provider.test.cts`, which reports microseconds per `readFile`
through a JS provider — a whole-call number, not a boundary-crossing one.

## Host and date

AMD Ryzen 9 8945HS, 16 logical CPUs, Windows 11 26200. Node v24.19.0, N-API 10.
Release builds. `napi` 2.16 `ThreadsafeFunction`; a director thread parks on a
condvar until JS calls back. 2026-08-17. Three agreeing runs plus a fourth.

## The numbers

| measurement | value |
|---|---|
| Blocking round trip, director thread → JS → back, p50 | **1.7–2.0 µs** |
| Same, with the provider returning a Promise instead of a value | +0.2 µs |
| Same, with a full event-loop turn in between | +0.8 µs |
| Tail (max observed) | 130–400 µs |
| Cold worker wake | 31–47 µs |
| For scale: ring `READ` of 4 KiB, p50 (recorded elsewhere) | 9.7 µs |

**The headline is the comparison, not the number.** A JavaScript provider adds
roughly **20%** to a 4 KiB read, not an order of magnitude — which is what made a
host-language provider a reasonable thing to build at all.

Two more findings from the same harness, both since confirmed by things that do
still exist:

* **Concurrency scales with event loops and only with event loops.** Eight
  director threads against one worker loop gave p50 17.8 µs ≈ 8 × 2.2 µs, exactly
  serialised. One loop to four was 7.7× throughput. This is why
  `providerWorker()` is the recommended shape.
* **A busy main loop is catastrophic; a worker loop is immune.** Under ~1 ms of
  work per turn, main-loop servicing fell from 1507 to 3.8 MiB/s (370×), while a
  worker-serviced provider was unaffected (1449 MiB/s, p50 still 2.0 µs).

## Why the harness went

It was its own cargo workspace, listed in neither `rust/`'s `members` nor its
`exclude`, so nothing in the workspace ever compiled it — while it held a path
dependency on `vfs-embed`, the crate whose surface changed in every task of stage
4. Its `spike.node` output was gitignored, so a fresh clone could not run the
benchmark even by hand. Its `cache-cost` binary is superseded by
`vfs-cache/tests/hit_scaling_cost.rs` and `hit_copy_cost.rs`, which CI runs.

See [block-cache-hit-cost.md](./block-cache-hit-cost.md) for the defect it found
and the fix, and the stage 4 task reports under `.superpowers/sdd/` for the full
spike write-up.

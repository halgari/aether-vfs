# Benchmarks

Numbers for the director FUSE control ring (shared-memory RPC) and related deltas.

| Doc | Description |
|-----|-------------|
| [fuse-rpc-latest.md](./fuse-rpc-latest.md) | Last full machine report from `vfs-fuse-bench` |
| [fuse-rpc-performance.md](./fuse-rpc-performance.md) | Early performance summary |
| [a-optimizations-delta.md](./a-optimizations-delta.md) | A1–A5 (open handle, unlock I/O, decode-into, geom, pipeline) |
| [b-optimizations-delta.md](./b-optimizations-delta.md) | B1–B5 (bulk arena, payload cap, workers, events, readahead) |
| [c-throughput-delta.md](./c-throughput-delta.md) | C-set (read_into bank, larger arena, bench bulk path) |
| [phase12-game-buffer-delta.md](./phase12-game-buffer-delta.md) | Into-game-buffer path notes (bulk preferred) |
| [load-debug-vs-release.md](./load-debug-vs-release.md) | Real game load: time-to-window, debug vs release |
| [hollow-removal.md](./hollow-removal.md) | Launch cost with and without process hollowing |
| [block-cache-hit-cost.md](./block-cache-hit-cost.md) | `vfs-cache` hit path: 110x at the default block size, and why the sweep flattened |
| [node-ffi-round-trip.md](./node-ffi-round-trip.md) | Node ↔ Rust provider round trip: 1.7–2.0 µs. **Historical** — the harness is gone, but see the two below |
| [node-binding-surface.md](./node-binding-surface.md) | The `aethervfs` binding's performance surface, held by `pnpm bench` as a tiered gate. Includes a live `main → worker` crossing figure (22.3 µs against a recorded 47 µs) |
| [node-typescript-js-layer.md](./node-typescript-js-layer.md) | Did the TypeScript migration cost anything? No — 3.9 ns on a forwarded property read, and 1.00–1.02x on everything that crosses into Rust |

Architecture context: [../architecture.md](../architecture.md) §5.

## Run

```powershell
cargo run -p vfs-launch --bin vfs-fuse-bench --release
cargo run -p vfs-launch --bin vfs-fuse-bench --release -- --zip
```

The block-cache figures were produced by `spike-node/cache-cost`, a throwaway
harness **that has been deleted** (its own workspace, compiled by nothing, and
superseded by the tests below). What holds those numbers now is these two tests:

```powershell
cargo test -p vfs-cache --release --test hit_copy_cost --test hit_scaling_cost
```

`hit_copy_cost` is deterministic and allocation-counted; `hit_scaling_cost`
measures wall-clock ratios and documents its own thresholds and known limits.

**Corrected 2026-08-17:** this section previously said CI runs the command above
on every push. Both tests do run on every push, but **in debug — `--release`
appears nowhere in `.github/workflows/ci.yml`.** That matters unevenly:
`hit_copy_cost` counts allocations and is build-independent, so it is a real gate
either way; `hit_scaling_cost` measures wall-clock ratios in a debug build on a
4-vCPU runner, which is a much weaker gate than the release run its thresholds
were chosen against. Prefer **release** when running these by hand.

Note also the correction at the top of
[block-cache-hit-cost.md](./block-cache-hit-cost.md): that file's `after MiB/s`
column is unreliable and its old "110x" headline should not be cited.

SpinNotifier in-process numbers understate production event-wake latency.

## The Node binding

Its numbers are held the same way — by a gate rather than by a document — and the
tier split above is exactly where its three tiers came from:

```powershell
cd rust/crates/vfs-node
pnpm bench        # the durable gate: builds release, then measures and asserts
pnpm bench:ab     # a one-shot: the emitted JS layer against the hand-written one
```

Two things it does that the correction above argues for. It **builds release
itself** rather than hoping, and it then **refuses to run** unless
`aethervfs.node` is byte-identical (sha256) to `target/release/aethervfs.dll` —
so it cannot silently report debug numbers the way `hit_scaling_cost` does in CI.
That guard is not theoretical: it fired on its first real run, having caught a
concurrent `pnpm test` rebuilding debug over the release addon three seconds
after it was installed.

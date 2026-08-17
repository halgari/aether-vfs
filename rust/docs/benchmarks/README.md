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
| [node-ffi-round-trip.md](./node-ffi-round-trip.md) | Node ↔ Rust provider round trip: 1.7–2.0 µs. **Historical** — the harness is gone |

Architecture context: [../architecture.md](../architecture.md) §5.

## Run

```powershell
cargo run -p vfs-launch --bin vfs-fuse-bench --release
cargo run -p vfs-launch --bin vfs-fuse-bench --release -- --zip
```

The block-cache figures were produced by `spike-node/cache-cost`, a throwaway
harness **that has been deleted** (its own workspace, compiled by nothing, and
superseded by the tests below). What holds those numbers now is CI:

```powershell
cargo test -p vfs-cache --release --test hit_copy_cost --test hit_scaling_cost
```

`hit_copy_cost` is deterministic and allocation-counted; `hit_scaling_cost`
measures wall-clock ratios and documents its own thresholds and known limits.
Both run on every push. Prefer **release** builds. SpinNotifier in-process numbers understate production event-wake latency.

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

Architecture context: [../overview.md](../overview.md), [../vfs-summary.md](../vfs-summary.md) §13.

## Run

```powershell
cargo run -p vfs-launch --bin vfs-fuse-bench --release
cargo run -p vfs-launch --bin vfs-fuse-bench --release -- --zip
```

Prefer **release** builds. SpinNotifier in-process numbers understate production event-wake latency.

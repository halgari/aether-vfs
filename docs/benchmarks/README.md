# Benchmarks

| Doc | Description |
|------|-------------|
| [fuse-rpc-performance.md](./fuse-rpc-performance.md) | Director FUSE control-ring latency & throughput summary |
| [fuse-rpc-latest.md](./fuse-rpc-latest.md) | Last full machine report from `vfs-fuse-bench` |

## Run

```powershell
# Release (recommended for numbers)
cargo run -p vfs-launch --bin vfs-fuse-bench --release

# Zip-window path (GameLayers Skyrim.esm if present)
cargo run -p vfs-launch --bin vfs-fuse-bench --release -- --zip
```

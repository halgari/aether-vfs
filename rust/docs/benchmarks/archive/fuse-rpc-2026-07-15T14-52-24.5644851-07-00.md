# VFS Director FUSE RPC Benchmark

- Date: 2026-07-15T14:52:23.3564983-07:00
- Host: WIN11-RUST
- Build: release
- payload_cap: 262144 bytes
- Notifier: SpinNotifier (same-process client/server threads)
- File: zip-window Skyrim.esm (249753412 bytes) from C:\GameLayers\1. Skyrim Special Edition.zip
- Warmup: 50, timed iters: 500 (latency); throughput separate

## Latency (control plane)

```
HEARTBEAT RTT                n=500   min=    0.40µs  p50=    0.70µs  mean=    0.74µs  p95=    0.90µs  p99=    1.10µs  max=    1.10µs
GETATTR RTT                  n=500   min=    3.90µs  p50=    5.20µs  mean=    5.47µs  p95=    6.60µs  p99=   19.00µs  max=   24.10µs
OPEN RTT                     n=500   min=    3.20µs  p50=    3.90µs  mean=    4.07µs  p95=    4.70µs  p99=    9.80µs  max=   23.20µs
CLOSE RTT                    n=500   min=    1.60µs  p50=    2.30µs  mean=    2.46µs  p95=    2.90µs  p99=    4.30µs  max=   19.30µs
```

## READ RTT (single RPC, data fits in payload_cap)

```
READ 64 B                    n=500   min=   38.30µs  p50=   39.80µs  mean=   43.33µs  p95=   65.60µs  p99=   80.20µs  max=  114.20µs
  └─ implied throughput @ mean: 1.4 MiB/s  (p50 RTT 39.80 µs)
READ 512 B                   n=500   min=   38.90µs  p50=   40.50µs  mean=   45.43µs  p95=   66.90µs  p99=   83.30µs  max=  109.10µs
  └─ implied throughput @ mean: 10.7 MiB/s  (p50 RTT 40.50 µs)
READ 4096 B                  n=500   min=   40.50µs  p50=   44.50µs  mean=   47.29µs  p95=   69.80µs  p99=   79.20µs  max=  112.70µs
  └─ implied throughput @ mean: 82.6 MiB/s  (p50 RTT 44.50 µs)
READ 16384 B                 n=500   min=   54.20µs  p50=   59.20µs  mean=   63.42µs  p95=   87.70µs  p99=   99.40µs  max=  109.50µs
  └─ implied throughput @ mean: 246.4 MiB/s  (p50 RTT 59.20 µs)
READ 65536 B                 n=500   min=  299.90µs  p50=  336.60µs  mean=  343.81µs  p95=  401.80µs  p99=  442.00µs  max=  814.60µs
  └─ implied throughput @ mean: 181.8 MiB/s  (p50 RTT 336.60 µs)
READ 262136 B                n=500   min=  340.50µs  p50=  534.70µs  mean=  532.23µs  p95=  773.00µs  p99=  925.20µs  max=  986.80µs
  └─ implied throughput @ mean: 469.7 MiB/s  (p50 RTT 534.70 µs)
```

## Sequential throughput (fragmented READ RPCs)

- Bytes read: 33554432 (32.00 MiB) in 129 RPC(s)
- Wall time: 160.2869ms (160.29 ms)
- Throughput: **199.6 MiB/s**
- Avg time per RPC: 1242.53 µs

## Baselines (same host, for context)

- **OpenTable::read direct** (no IPC, same Server tree): **1324.6 MiB/s** over 33554432 bytes
- **std::fs::File sequential** on bench.bin: **2640.4 MiB/s** over 33815544 bytes
- **IPC overhead factor**: 6.6× slower than OpenTable direct (throughput ratio)

## Notes

- Latency numbers use **SpinNotifier** in one process (two threads). Production event wait will add OS wake latency (typically tens of µs) on idle rings.
- Each READ response is capped at `payload_cap - 8` bytes; large files are fragmented into multiple RPCs.
- Debug builds are substantially slower; prefer `--release` for published numbers.

## Summary table

| Metric | Value |
|--------|-------|
| HEARTBEAT p50 | 0.70 µs |
| GETATTR p50 | 5.20 µs |
| OPEN p50 | 3.90 µs |
| CLOSE p50 | 2.30 µs |
| Sequential RPC throughput | **199.6 MiB/s** |
| OpenTable direct throughput | **1324.6 MiB/s** |
| std::fs throughput | **2640.4 MiB/s** |
# VFS Director FUSE RPC Benchmark

- Date: 2026-07-15T16:56:42.2014935-07:00
- Host: WIN11-RUST
- Build: release
- payload_cap: 1048576 bytes
- bulk arena: 8388608 bytes (1048576 B banks × 8 slots)
- Notifier: SpinNotifier (same-process client/server threads)
- File: zip-window Skyrim.esm (249753412 bytes) from C:\GameLayers\1. Skyrim Special Edition.zip
- Warmup: 50, timed iters: 500 (latency); throughput separate

## Latency (control plane)

```
HEARTBEAT RTT                n=500   min=    0.70µs  p50=    0.80µs  mean=    0.82µs  p95=    1.10µs  p99=    1.20µs  max=    1.50µs
GETATTR RTT                  n=500   min=    2.00µs  p50=    2.90µs  mean=    3.01µs  p95=    3.60µs  p99=    4.20µs  max=   12.90µs
OPEN RTT                     n=500   min=   21.60µs  p50=   22.80µs  mean=   25.04µs  p95=   30.70µs  p99=   41.10µs  max=  628.80µs
CLOSE RTT                    n=500   min=   10.70µs  p50=   11.40µs  mean=   11.86µs  p95=   13.20µs  p99=   24.90µs  max=   29.40µs
```

## READ RTT (single RPC, data fits in payload_cap)

```
READ 64 B                    n=500   min=    4.10µs  p50=    4.40µs  mean=    4.45µs  p95=    4.60µs  p99=   11.10µs  max=   13.50µs
  └─ implied throughput @ mean: 13.7 MiB/s  (p50 RTT 4.40 µs)
READ 512 B                   n=500   min=    4.30µs  p50=    4.70µs  mean=    4.76µs  p95=    4.90µs  p99=   11.40µs  max=   14.20µs
  └─ implied throughput @ mean: 102.5 MiB/s  (p50 RTT 4.70 µs)
READ 4096 B                  n=500   min=    5.60µs  p50=    6.70µs  mean=    6.80µs  p95=    7.30µs  p99=   12.90µs  max=   17.60µs
  └─ implied throughput @ mean: 574.6 MiB/s  (p50 RTT 6.70 µs)
READ 16384 B                 n=500   min=   12.30µs  p50=   13.90µs  mean=   15.86µs  p95=   25.40µs  p99=   29.40µs  max=  340.40µs
  └─ implied throughput @ mean: 985.0 MiB/s  (p50 RTT 13.90 µs)
READ 65536 B bulk            n=500   min=    8.40µs  p50=   11.80µs  mean=   14.70µs  p95=   22.40µs  p99=   28.80µs  max=   73.00µs
  └─ implied throughput @ mean: 4251.9 MiB/s  (p50 RTT 11.80 µs)
READ 262144 B bulk           n=500   min=   20.90µs  p50=   21.60µs  mean=   22.98µs  p95=   27.30µs  p99=   32.60µs  max=  450.80µs
  └─ implied throughput @ mean: 10878.9 MiB/s  (p50 RTT 21.60 µs)
READ 1048568 B bulk          n=500   min=  208.10µs  p50=  216.30µs  mean=  218.74µs  p95=  232.00µs  p99=  238.90µs  max=  243.20µs
  └─ implied throughput @ mean: 4571.7 MiB/s  (p50 RTT 216.30 µs)
```

## Sequential throughput (bulk arena READ RPCs)

- Bytes read: 33554432 (32.00 MiB) in 32 RPC(s)
- Wall time: 39.367ms (39.37 ms)
- Throughput: **812.9 MiB/s**
- Avg time per RPC: 1230.22 µs
- Chunk size: 1048576 B (bulk bank), pipeline depth 4

## Sequential throughput (remote WPM READ RPCs)

- Bytes read: 33554432 (32.00 MiB) in 32 RPC(s)
- Wall time: 38.9404ms (38.94 ms)
- Throughput: **821.8 MiB/s**
- Avg time per RPC: 1216.89 µs
- Path: disk/zip → director staging → WriteProcessMemory into client buffer

## Baselines (same host, for context)

- **OpenTable::read_into direct** (no IPC, same Server tree): **2768.7 MiB/s** over 33554432 bytes
- **std::fs::File sequential** on bench.bin: **2356.6 MiB/s** over 33554432 bytes
- **IPC overhead factor (bulk)**: 3.4× slower than OpenTable direct
- **IPC overhead factor (remote WPM)**: 3.4× slower than OpenTable direct

## Notes

- Latency numbers use **SpinNotifier** in one process (two threads). Production event wait will add OS wake latency (typically tens of µs) on idle rings.
- Bulk sequential: **FLAG_READ_BULK** disk/zip → arena → client `copy_to` (phase 1 into user buffer).
- Remote sequential: **FLAG_READ_REMOTE** disk/zip → staging → `WriteProcessMemory` into client buffer (phase 2).
- Debug builds are substantially slower; prefer `--release` for published numbers.

## Summary table

| Metric | Value |
|--------|-------|
| HEARTBEAT p50 | 0.80 µs |
| GETATTR p50 | 2.90 µs |
| OPEN p50 | 22.80 µs |
| CLOSE p50 | 11.40 µs |
| Sequential bulk throughput | **812.9 MiB/s** |
| Sequential remote WPM throughput | **821.8 MiB/s** |
| OpenTable direct throughput | **2768.7 MiB/s** |
| std::fs throughput | **2356.6 MiB/s** |
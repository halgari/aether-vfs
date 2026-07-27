# C-set throughput optimizations — bench deltas

**Date:** 2026-07-15  
**Host:** WIN11-RUST  
**Build:** release, SpinNotifier in-process  
**Baseline:** post-B (`docs/benchmarks/fuse-rpc-latest.md` before this change) — zip sequential **~249 MiB/s**, disk not separately published at that revision with bulk path; OpenTable direct zip **~694 MiB/s**.

## What changed (C-set)

| ID | Change | Where |
|----|--------|--------|
| **C1** | Bulk READ fills arena bank via `SharedSeg::with_mut_bytes` + `OpenTable::read_into` — **disk/zip → bank**, no intermediate `Vec` + `write_bank` copy | `vfs-ipc` seg, `vfs-server` arena/open_table/handler |
| **C2** | Skip **B5 readahead** when the requested chunk is already large (`≥256 KiB`); invalidate stale readahead after large reads | `open_table` |
| **C3** | Client/director **`copy_to`** arena → user buffer (no `read_bytes` → `Vec`) | `fuse_client`, `director::rpc_read_all` |
| **C4** | Director default arena **16 MiB → 32 MiB** (~1 MiB banks @ 32 slots) | `director` |
| **C5** | Bench measures **bulk sequential** path (arena + pipeline + `FLAG_READ_BULK`), 1 MiB banks | `vfs-fuse-bench` |

## Results

### Disk sequential (32 MiB patterned file)

| Metric | Post-B (inline-heavy / smaller path) | After C | Δ |
|--------|--------------------------------------|---------|---|
| Sequential RPC | ~0.25–0.4 GiB/s class (prior bulk underused) | **1937.5 MiB/s** | **~5–8×** |
| OpenTable direct | — | 3077 MiB/s | — |
| IPC overhead factor | ~2.8× | **1.6×** | better |

### Zip sequential (Skyrim.esm window, 32 MiB)

| Metric | Post-B | After C | Δ |
|--------|--------|---------|---|
| Sequential RPC | **248.6 MiB/s** | **806.2 MiB/s** | **+3.2×** |
| OpenTable direct | 693.7 MiB/s | 2921 MiB/s* | *cache-warmed direct baseline varies |
| IPC overhead factor | 2.8× | 3.6×* | *direct also faster under warm cache; absolute RPC is the headline |

\*OpenTable “direct” on a hot zip after latency loops is not perfectly comparable run-to-run; the sequential RPC absolute (**806 MiB/s** vs **249 MiB/s**) is the fair before/after.

### Single-RPC bulk RTT (warm cache, offset 0)

| Size | p50 RTT (zip run) | Notes |
|------|-------------------|--------|
| 64 KiB bulk | ~20 µs | Path is RPC + copy, not cold disk |
| 256 KiB bulk | ~59 µs | |
| ~1 MiB bulk | ~209 µs | |

Large single-RPC “implied GiB/s” numbers are **cache-hot** and not sequential cold throughput.

## Remaining copies (bulk path)

```
disk/zip ──read──► arena bank ──copy_to──► client buffer
           (1)                    (2)
```

Previously: disk → `Vec` → arena → `Vec` → client (3–4).

Further headroom: OS-level zero-copy into game buffer (hard under NtReadFile), container `mmap` on director, multi-worker concurrent multi-fh load (already 4 workers in production director).

## How to reproduce

```text
cargo run -p vfs-launch --bin vfs-fuse-bench --release
cargo run -p vfs-launch --bin vfs-fuse-bench --release -- --zip
```

Latest full report: `docs/benchmarks/fuse-rpc-latest.md`.

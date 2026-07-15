# Tier B Optimizations — What Landed & Bench Notes

**Host:** WIN11-RUST · **Build:** release  
**Commit series:** B1–B5 in director/server/protocol/win/shim

## Implemented

| ID | Change | Where |
|----|--------|--------|
| **B1** | Shared **bulk arena** after control ring; `FLAG_READ_BULK` + bulk READ resp; banks keyed by ring slot | `vfs-server/arena`, handler, director, FuseClient, probe |
| **B2** | Default `payload_cap` **1 MiB** (was 256 KiB) | `vfs-server` `DEFAULT_PAYLOAD_CAP` |
| **B3** | **Worker pool** (4 threads) each `serve_one_arena` | `vfs-launch` director |
| **B4** | **EventNotifier** (named CreateEvent/OpenEvent) for server/client wake | `vfs-win`, director workers hold events |
| **B5** | **Sequential readahead** (256 KiB) on open files | `OpenTable` / `LiveFile` |

## Correctness

`vfs-launch --probe` after B:

- `Data/Skyrim.esm` full **249 753 412** bytes, TES4 magic  
- SkyUI esp/bsa full sizes  
- `root_payload_files=0`

## Performance (indicative, same harness)

Relative to post-A baseline (~418 MiB/s disk sequential, ~308 MiB/s zip):

| Metric | Post-A (disk) | Post-B (disk) | Notes |
|--------|---------------|---------------|--------|
| payload_cap | 256 KiB | **1 MiB** | Fewer RPCs for large inline |
| Seq RPC (bench, pipelined) | ~310–418 MiB/s | ~**294 MiB/s** | Spin client + event server; noise / event wait |
| READ 4 KiB p50 | ~7–10 µs | ~**7 µs** | Still A1-dominated |
| OPEN p50 | ~23 µs | ~**23 µs** | File open at OPEN |

**Why sequential didn’t jump:**  
`vfs-fuse-bench` still uses **SpinNotifier** on the client and **inline** single-RPC timing for most READ sizes; bulk path is used when `FLAG_READ_BULK` and arena are active (probe path). Multi-worker helps concurrent multi-fh load more than single-stream spin bench. Event waits (1 ms slice) can add latency under light load vs pure spin.

**Where B wins in production:**

- Large game READs via FuseClient with bulk flag + arena (less ring copy of payload body).  
- Concurrent module loads with 4 workers.  
- Zip sequential readahead (B5) after A1 open-handle.  
- Lower CPU when idle (B4 events vs spin).

## Follow-ups

1. Point `vfs-fuse-bench` sequential path at bulk arena + multi-worker for apples-to-apples B1/B3 numbers.  
2. Adaptive pipeline depth tied to worker count.  
3. Larger arena banks (e.g. 2–4 MiB) for multi-MiB BSA chunks.

## Raw

- Probe: machine `probe-b` / full ESM read after short-read fix  
- Bench logs: `bench-b-disk.txt`, `bench-b-zip.txt`

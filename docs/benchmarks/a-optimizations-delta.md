# Tier A Optimizations — Benchmark Deltas

**Host:** WIN11-RUST  
**Build:** release (`vfs-fuse-bench`)  
**Ring:** 8 slots, `payload_cap = 256 KiB`, SpinNotifier (same-process client/server)  
**Sequential test:** 32 MiB via fragmented READs  

Harness: `cargo run -p vfs-launch --bin vfs-fuse-bench --release [--zip]`

---

## What shipped

| Item | Change | Crates |
|------|--------|--------|
| **A1** | OPEN keeps OS `File` open (no per-READ `File::open`) | `vfs-server` open_table |
| **A2** | Map mutex only for `Arc` clone; I/O outside lock | `vfs-server` open_table |
| **A3** | `decode_read_resp_into` — no second data `Vec` | `vfs-protocol`, bench, shim client |
| **A4** | Cache ring `Geom`; `RingClient::with_geom` | `vfs-ipc`, `vfs-shim` FuseClient |
| **A5** | `submit_many` + pipeline depth 4 for sequential READs | `vfs-ipc`, FuseClient, bench |

A1 and A2 were implemented together (open handle stored as `Arc<LiveFile>` so A2 can drop the map lock).

---

## Disk layer (`bench.bin`, 64 MiB temp)

| Metric | Baseline (pre-A) | After A1+A2 | After A1–A5 | Δ vs baseline |
|--------|------------------|-------------|-------------|----------------|
| HEARTBEAT p50 | 1.0 µs | 0.4 µs | 1.1 µs | noise |
| GETATTR p50 | 5.0 µs | 2.9 µs | 5.0 µs | noise |
| OPEN p50 | **7.3 µs** | **22.7 µs** | **24.7 µs** | **+OPEN cost** (file open moved to OPEN) |
| CLOSE p50 | 2.6 µs | 10.9 µs | 12.4 µs | +drop File |
| READ 4 KiB p50 | **44.7 µs** | **6.7 µs** | **9.7 µs** | **~4.6–6.7× faster** |
| READ ~256 KiB p50 | 603 µs | 527 µs | 453 µs | ~1.3× faster |
| **Seq RPC throughput** | **383 MiB/s** | **418 MiB/s** | **311 MiB/s*** | A1+A2 +9%; A5 pipeline regresses under 1 server thread |
| OpenTable direct | 1289 MiB/s | 1836 MiB/s | 1596 MiB/s | A1 helps direct path too |
| std::fs | ~3032 MiB/s | ~3027 MiB/s | ~3010 MiB/s | ceiling |

\*With **pipeline depth 4** on a **single** server thread, `submit_many` still waits slot-by-slot while the server drains serially—extra claim/publish overhead can **reduce** sequential MiB/s vs serial submit. A5 still helps once the server has a worker pool (see analysis doc).

---

## Zip-window layer (`Data/Skyrim.esm` from GameLayers)

| Metric | Baseline (pre-A) | After A1+A2 | After A1–A5 | Δ vs baseline |
|--------|------------------|-------------|-------------|----------------|
| OPEN p50 | 3.9 µs | 23.5 µs | 24.2 µs | open File at OPEN |
| READ 4 KiB p50 | **44.5 µs** | **6.9 µs** | **9.5 µs** | **~4.7–6.5× faster** |
| READ ~256 KiB p50 | 535 µs | 394 µs | 504 µs | improved then noisy |
| **Seq RPC throughput** | **200 MiB/s** | **308 MiB/s** | **326 MiB/s** | **+54–63%** |
| OpenTable direct | 1325 MiB/s | 1890 MiB/s | 2166 MiB/s | kept handle + no map lock |

**Biggest zip win:** sequential ~**200 → ~310+ MiB/s** from not reopening the 16 GB zip on every READ.

---

## Interpretation

### A1 (kept File) — **clear win**
- Small READ latency collapsed (~45 µs → ~7 µs) because reopen+seek was the dominant cost.
- Zip sequential +50%+ (reopen of huge container was brutal).
- OPEN got slower (~7 → ~23 µs): expected—work moved from READ to OPEN (correct FUSE cost model).

### A2 (no map lock during I/O) — **correctness + scalability**
- Enables concurrent READs on different `fh`s without blocking the whole table.
- Hard to isolate in single-fh bench; required for A5 multi-slot later.

### A3 (decode into buffer) — **modest / hygiene**
- Removes a heap alloc + copy on the client for READ data.
- Bench sequential uses `decode_read_resp_into` into a reusable scratch buffer.

### A4 (cache Geom / with_geom) — **micro**
- Avoids re-parsing ring header every call in the shim.
- Latency noise-level in this harness.

### A5 (pipeline depth 4) — **infrastructure; win deferred**
- With **one** `serve_one` thread, pipelining does not overlap I/O.
- Disk sequential dropped vs A1+A2 serial path in this run (~418 → ~311 MiB/s).
- Keep A5; pair with **server worker pool** next for real gains.

---

## Recommended next knobs

1. Server worker pool (N× `serve_one`) so A5 pipeline fills.  
2. Optional adaptive depth: depth=1 for single-thread, depth=4 when workers>1.  
3. Bulk arena (Tier B) for multi-MiB transfers.

---

## Raw logs

| Stage | Scratch / artifact |
|-------|--------------------|
| A1+A2 disk | `bench-a1a2-disk.txt` (local run) / summary above |
| A1+A2 zip | `bench-a1a2-zip.txt` |
| A1–A5 disk | `bench-a3a5-disk.txt` |
| A1–A5 zip | `bench-a3a5-zip.txt` |
| Pre-A baseline | [fuse-rpc-performance.md](./fuse-rpc-performance.md) |

---

## Code map

| Optimization | Primary files |
|--------------|---------------|
| A1+A2 | `crates/vfs-server/src/open_table.rs` |
| A3 | `crates/vfs-protocol/src/lib.rs` (`decode_read_resp_into`) |
| A4 | `crates/vfs-ipc/src/endpoint.rs` (`with_geom`), `fuse_client.rs` |
| A5 | `crates/vfs-ipc/src/endpoint.rs` (`submit_many`), `fuse_client.rs`, bench |

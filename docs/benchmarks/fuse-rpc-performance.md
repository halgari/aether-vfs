# Director FUSE RPC Performance

Measured **round-trip** cost of the shipped director control-ring path (`vfs-server` + `vfs-ipc` + `vfs-protocol`) used by the thin shim.

**How to reproduce**

```powershell
cargo run -p vfs-launch --bin vfs-fuse-bench --release
cargo run -p vfs-launch --bin vfs-fuse-bench --release -- --zip   # zip-window Skyrim.esm if present
```

The harness starts a real `Server` on an in-process ring (`SpinNotifier`), drives `RingClient` opcodes, and writes timestamped reports under `docs/benchmarks/fuse-rpc-*.md` plus `fuse-rpc-latest.md`.

---

## Hardware / build (this capture)

| | |
|--|--|
| Host | `WIN11-RUST` |
| Date | 2026-07-15 |
| Build | **release** (`--release`) |
| Ring | 8 slots, `payload_cap = 262144` (256 KiB) |
| Notifier | `SpinNotifier` (client + server threads in one process) |

> **Note on notifier:** Spin wake is the lower bound for ring RTT. A production event wait on a quiet ring typically adds tens of microseconds of OS wake latency on top of these numbers.

---

## Latency (control plane)

500 timed samples after 50 warmups. Times are **full client→server→client RTTs**.

### Disk-backed layer (`bench.bin`, 64 MiB temp file)

| Op | min | p50 | mean | p95 | p99 | max |
|----|-----|-----|------|-----|-----|-----|
| **HEARTBEAT** | 0.8 µs | **1.0 µs** | 1.1 µs | 1.3 µs | 1.4 µs | 28 µs |
| **GETATTR** | 4.6 µs | **5.0 µs** | 5.4 µs | 5.6 µs | 21 µs | 28 µs |
| **OPEN** | 6.6 µs | **7.3 µs** | 7.8 µs | 8.1 µs | 30 µs | 49 µs |
| **CLOSE** | 2.2 µs | **2.6 µs** | 2.9 µs | 3.1 µs | 18 µs | 26 µs |

### Zip-window layer (`Data/Skyrim.esm` from GameLayers base zip, ~238 MiB)

| Op | min | p50 | mean | p95 | p99 | max |
|----|-----|-----|------|-----|-----|-----|
| **HEARTBEAT** | 0.4 µs | **0.7 µs** | 0.7 µs | 0.9 µs | 1.1 µs | 1.1 µs |
| **GETATTR** | 3.9 µs | **5.2 µs** | 5.5 µs | 6.6 µs | 19 µs | 24 µs |
| **OPEN** | 3.2 µs | **3.9 µs** | 4.1 µs | 4.7 µs | 9.8 µs | 23 µs |
| **CLOSE** | 1.6 µs | **2.3 µs** | 2.5 µs | 2.9 µs | 4.3 µs | 19 µs |

Metadata ops stay in the **~1–8 µs p50** range for this harness.

---

## READ latency (single RPC)

One `OP_READ` per sample; response must fit `payload_cap − 8`.

### Disk source

| READ size | p50 RTT | mean RTT | Implied @ mean |
|-----------|---------|----------|----------------|
| 64 B | 41.5 µs | 62 µs | ~1 MiB/s |
| 512 B | 41.0 µs | 44 µs | ~11 MiB/s |
| 4 KiB | 44.7 µs | 49 µs | ~79 MiB/s |
| 16 KiB | 59.2 µs | 63 µs | ~250 MiB/s |
| 64 KiB | 308 µs | 314 µs | ~199 MiB/s |
| ~256 KiB (cap) | 603 µs | 662 µs | ~378 MiB/s |

### Zip-window (`Skyrim.esm`)

| READ size | p50 RTT | mean RTT | Implied @ mean |
|-----------|---------|----------|----------------|
| 64 B | 39.8 µs | 43 µs | ~1.4 MiB/s |
| 512 B | 40.5 µs | 45 µs | ~11 MiB/s |
| 4 KiB | 44.5 µs | 47 µs | ~83 MiB/s |
| 16 KiB | 59.2 µs | 63 µs | ~246 MiB/s |
| 64 KiB | 337 µs | 344 µs | ~182 MiB/s |
| ~256 KiB (cap) | 535 µs | 532 µs | ~470 MiB/s |

**Takeaway:** small READs are **latency-bound** (~40 µs fixed cost). Larger READs amortize ring + seek overhead; full-payload READs sit around **~0.5–0.7 ms p50**.

---

## Sequential throughput (fragmented READ)

Client walks the file with max-sized READs (`payload_cap − 8` ≈ 256 KiB each), 32 MiB total.

| Path | Throughput | Wall (32 MiB) | RPCs |
|------|------------|---------------|------|
| **Ring RPC, disk source** | **~383 MiB/s** | ~84 ms | 129 |
| **Ring RPC, zip-window Skyrim.esm** | **~200 MiB/s** | ~160 ms | 129 |
| OpenTable::read direct (no IPC) | ~1.3 GiB/s | — | — |
| std::fs sequential (bench.bin) | ~2.6–3.0 GiB/s | — | — |

IPC sequential is roughly **3–7×** slower than director-local `OpenTable::read` on this machine, and far below raw `std::fs` — expected for pure FUSE-style RPC with copy-in/copy-out payloads.

---

## Interpretation for the game path

1. **Metadata** (open/getattr/readdir/close) is cheap: low single-digit to ~10 µs RTT in-process with spin wake.
2. **Bulk data** should use large READ sizes (prefer near `payload_cap`) to approach hundreds of MiB/s over the ring.
3. **BSA / ESM streaming** at ~200–400 MiB/s pure-RPC is often enough for load screens; hot paths may still want future bulk mapping (out of scope for the pure-RPC phase).
4. Cross-process **event** notifiers will raise idle RTT; busy multi-threaded games that keep the ring hot are closer to these spin numbers.

---

## Methodology details

| Item | Value |
|------|--------|
| Binary | `vfs-fuse-bench` (`crates/vfs-launch/src/bin/vfs-fuse-bench.rs`) |
| Server | `vfs_server::Server::serve_one` |
| Client | `vfs_ipc::RingClient::submit` |
| Disk fixture | 64 MiB patterned temp file registered as `data/bench.bin` |
| Zip fixture | `C:\GameLayers\1. Skyrim Special Edition.zip` → `Data/Skyrim.esm` via `encode_zip_window` |
| Stats | min / p50 / mean / p95 / p99 / max over 500 samples |

Raw machine captures: `docs/benchmarks/fuse-rpc-latest.md` (overwritten each run) and timestamped siblings from the same binary.

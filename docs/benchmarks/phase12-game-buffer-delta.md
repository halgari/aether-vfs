# Phase 1 + 2: read into game buffers — bench deltas

**Date:** 2026-07-15  
**Host:** WIN11-RUST  
**Build:** release, SpinNotifier in-process  

## What shipped

| Phase | Mechanism | Isolation |
|-------|-----------|-----------|
| **1** | `NtReadFile` buffer passed to `FuseClient::read_fragmented`; bulk path `copy_to` arena → **game buffer** (no intermediate `tmp` Vec in the hook) | Shim never opens files; only director-owned section |
| **2** | `FLAG_READ_REMOTE` + `target_va`; director `OpenProcess` + `WriteProcessMemory` after `OP_REGISTER_PROCESS` | Shim still file-blind; only ships VA |

Phase 2 does **not** supersede phase 1: hybrid prefers **bulk** when the arena is available (faster). Remote is used when there is no arena, or when `VFS_PREFER_REMOTE=1`.

## Policy

```
fragment ≥ 256 KiB:
  if remote_ok && (no arena || PREFER_REMOTE) → FLAG_READ_REMOTE (WPM)
  else if arena → FLAG_READ_BULK (phase 1 copy_to)
  else → inline ring
fragment ≥ 64 KiB with arena → bulk
else → inline
```

Env:

- `VFS_REMOTE_READ=0` — do not register / never remote  
- `VFS_PREFER_REMOTE=1` — force remote for large fragments even when arena exists  

## Disk sequential (32 MiB)

| Path | Throughput |
|------|------------|
| Bulk arena + phase-1 style `copy_to` | **~1876 MiB/s** |
| Remote WPM | **~604 MiB/s** |
| OpenTable direct | ~2559 MiB/s |
| std::fs | ~2526 MiB/s |

IPC overhead (bulk): **~1.4×** vs OpenTable.

## Zip sequential (Skyrim.esm window, 32 MiB)

| Path | Throughput |
|------|------------|
| Bulk arena + phase-1 style `copy_to` | **~813 MiB/s** |
| Remote WPM | **~822 MiB/s** |
| OpenTable direct | ~2769 MiB/s |

On zip, bulk and remote are **about even** (I/O-bound); disk still favors bulk strongly.

## Takeaway

On this host, **shared bulk arena + local memcpy into the game buffer wins** over director WPM by ~3× for **disk** sequential; zip is roughly tied. Phase 2 remains valuable when:

- arena is unavailable,  
- a consumer wants zero shared-data mapping in the game, or  
- future cross-process paths need WPM for other reasons.

Default production path: **phase 1 bulk into game buffer**.

## Reproduce

```text
cargo run -p vfs-launch --bin vfs-fuse-bench --release
cargo run -p vfs-launch --bin vfs-fuse-bench --release -- --zip
```

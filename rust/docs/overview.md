# VFS — Project Overview

> **Partly out of date (2026-08-13).** PE hollowing has been removed in favour of
> the staged launch. See [architecture.md](./architecture.md) for the current design.

**Status:** Active development (2026-07)  
**Repo:** private `halgari/vfs`  
**Primary mission:** Run a Windows game (Skyrim SE + SKSE + SkyUI) with base game and mods served **directly from Stored ZIP archives**, with **no durable extract** of archive content onto the managed install root.

For the full technical narrative (architecture, PE loading, performance, lessons learned), see **[vfs-summary.md](./vfs-summary.md)** — written as multi-page whitepaper source material.

---

## One-sentence pitch

**A userspace FUSE-like VFS for Windows game processes:** the **director** owns content (zip/disk backends), the **thin shim** remaps NT file APIs under a managed root, and **process inject + PE hollow** satisfy CreateProcess and Steam DRM without writing archive PE/BSA/ESP to disk.

---

## How a host uses it (today)

```text
1. Session::new / vfs_director_create
2. set_root / set_overlay / set_state_dir
3. mount_zip (or custom Backend / C ops) for each layer
4. serve()  — shared-memory control ring + bulk arena
5. launch() — CreateProcess + dual-layer inject; optional PE hollow from VFS
```

The **game** never calls host open/read. It sees normal paths under the virtual root; hooks + ring deliver bytes from backends.

CLI: `cargo run -p vfs-launch --release` (GameLayers layout under `C:\GameLayers`).

---

## Architecture at a glance

```text
┌────────────────── Host process (vfs-launch / language host) ──────────────────┐
│  Session: mounts, paths, IpcServe workers, launch                              │
│  Backends: ZipBackend (CD index + Stored windows), Disk, C callbacks           │
└────────────────────────────────────┬───────────────────────────────────────────┘
                                     │ shared section: control ring + bulk arena
┌────────────────────────────────────▼───────────────────────────────────────────┐
│  Game process                                                                   │
│  Dual-layer inject: no_std payload (pre-init) + full shim (post-loader hooks)   │
│  NtCreateFile/ReadFile/… under root → FuseClient RPC → director backends        │
│  EXEs: hollow real Steam host image with zip PE (WriteProcessMemory only)       │
└─────────────────────────────────────────────────────────────────────────────────┘
```

**Isolation:** the shim does **not** open layer zips. Only the director (and PE/inject helpers) touch archive containers.

---

## Crate map (product vs legacy)

| Product path | Role |
|--------------|------|
| **vfs-director** | Session, FUSE kernel, ring serve, C ABI, launch |
| **vfs-protocol** | Wire codecs + `Backend` ops contract |
| **vfs-ipc** / **vfs-win** | Control ring, bulk arena, Windows section/events |
| **vfs-zip** | Zip **backend** (Stored + ZIP64 CD) |
| **vfs-inject** / **vfs-payload** / **vfs-shim** | Process create, pre-init, hooks, hollow |
| **vfs-launch** | Skyrim-oriented CLI host |

| Legacy / transitional | Role |
|------------------------|------|
| **vfs-core** / **shared** / **redirect** / **server** | Snapshot tree, old Serve path, fuse-bench baselines |

---

## Key technical bets

1. **Stored-only zips** — files are byte windows; no inflate on the hot path.  
2. **Userspace FUSE, not WinFsp** — full control inside the game process; no kernel driver.  
3. **Parent director** — content authority out of the game; multi-language C ABI.  
4. **Bulk shared arena** — large READs skip ring payload copies.  
5. **Steam host hollow** — real ProcessImageFileName for DRM; zip PE in RAM only.

---

## Related docs

| Doc | Use |
|-----|-----|
| [vfs-summary.md](./vfs-summary.md) | Full deep dive / whitepaper source |
| [superpowers/specs/](./superpowers/specs/) | Design specs by feature |
| [benchmarks/](./benchmarks/) | RPC latency/throughput |
| [performance-rpc-analysis.md](./performance-rpc-analysis.md) | Performance analysis notes |

---

## Non-goals (current)

- Kernel FUSE / WinFsp  
- Deflated zip entries on the data path  
- Multi-session long-lived daemon  
- Full write path as primary product  

---

*Last updated: 2026-07-16*

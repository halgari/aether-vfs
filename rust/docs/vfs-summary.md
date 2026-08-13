# VFS: Technical Summary for Architecture & Whitepaper Use

> **Superseded in part (2026-08-13).** Sections on process hollowing describe a
> mechanism that has since been removed — the staged launch replaced it. See
> [architecture.md](./architecture.md) §4.2 for what the launch path does now.

**Document type:** Deep technical summary (whitepaper source material)  
**Codebase:** `C:\oss\vfs` / private `halgari/vfs`  
**Last updated:** 2026-07-16  
**Audience:** Engineers writing papers, design reviews, or onboarding to the stack  

This document describes **what** the system is, **why** it exists, **how** it works end-to-end, and **what was learned** building a usermode virtual filesystem that launches a modern DRM-bound Windows game with mods served from multi-gigabyte ZIP archives without extraction.

A short product overview lives in [overview.md](./overview.md). Design history lives under [superpowers/specs/](./superpowers/specs/).

---

# Part I — Problem, goals, and constraints

## 1. The problem space

### 1.1 Modding and virtual filesystems

PC game modding often requires **overlaying** many packages of files onto a single “install tree.” Tools such as Mod Organizer 2 (USVFS lineage) solve this by interposing Windows file APIs so the game believes a merged tree exists, while files still live in separate mod folders or archives.

That approach has hard constraints:

- The game is a **third-party binary** you cannot recompile.
- Windows I/O is **NT-native** (`NtCreateFile`, `NtReadFile`, …), not only Win32 `CreateFile`.
- **CreateProcess** and the loader demand a real PE image on disk for the initial process.
- **Steam DRM** inspects process image identity (paths, signatures, authenticity checks)—naive hollowing of `cmd.exe` fails.
- Large base games are shipped as **16+ GB ZIP64** archives; extracting them to disk is slow, duplicates storage, and defeats “run from zip” goals.

### 1.2 The concrete mission

The milestone that drives design and validation:

> Launch **Skyrim Special Edition** with **SKSE** and **SkyUI**, serving base game + mods **straight from Stored ZIP archives** under `C:\GameLayers`, with **zero durable extract** of PE/BSA/ESP content under the managed root (no TEMP staging of archive PE as the content store).

Three layers (bottom → top):

| Order | Archive | Typical content |
|------|---------|-----------------|
| 1 | `1. Skyrim Special Edition.zip` (~16 GB, ZIP64) | `SkyrimSE.exe`, `Data/*.bsa`, masters |
| 2 | `2. SKSE 2.2.6.zip` | `skse64_loader.exe`, SKSE DLLs, scripts |
| 3 | `3. SkyUI 6.11.zip` | `Data/SkyUI_SE.esp`, BSA, translations |

**Decisive physical fact:** entries used on the content path are **Stored** (ZIP method 0). A virtual file is a **byte window** `[offset, offset+size)` inside a real container file. Deflated entries are rejected rather than inflated on the hot path.

### 1.3 What “zero extract” means (and does not)

| Allowed | Forbidden (for archive content) |
|---------|----------------------------------|
| Empty directory skeleton under managed root | Writing PE/BSA/ESP payload from zip to root or TEMP as the source of truth |
| AppData `Plugins.txt` / loadorder config | Materializing full archives for play |
| Real **Steam** `SkyrimSE.exe` as CreateProcess host | Using a non-Steam host for DRM’d Skyrim and expecting DRM to accept it |
| RAM buffers and process image writes (hollow) | Leaving extracted PE beside the game for LoadLibrary |

---

## 2. Design goals and non-goals

### 2.1 Goals

1. **Correctness under a real game stack** — SKSE + Steam + large BSAs, not only unit tests.  
2. **Isolation of content authority** — zip containers opened by a **director** process (or host), not by the injected game shim on the pure FUSE path.  
3. **Composable layers** — later mounts override earlier; plugins and PE discovery work across overlays.  
4. **Multi-language hostability** — C ABI: configure mounts, serve IPC, launch; optional host read for tools.  
5. **Performance** — bulk sequential reads in the hundreds of MiB/s class over IPC; small-metadata RTT in microseconds on spin notifiers.  
6. **Fail-safe hooks** — undecidable opens pass through to the real kernel; avoid crashing the game.

### 2.2 Non-goals (current)

- Kernel FUSE / WinFsp / filter drivers  
- Deflated zip as first-class content  
- Multi-tenant long-lived daemon for many games  
- Full write-path productization (overlay exists; not the focus)  
- Replacing Steam or SKSE  

### 2.3 Why usermode FUSE (not a driver)

| Approach | Pros | Cons for this project |
|----------|------|------------------------|
| WinFsp / Dokan | Familiar mount letter | Driver install, signing, different failure modes |
| Filter driver | Deep integration | Complexity, stability, distribution |
| **Usermode NT hooks + parent director** | No driver, full control, process-scoped | Must handle CreateProcess, SEC_IMAGE, Steam carefully |

The bet: **the game process is the only place that must see the virtual world**, and a **parent director** can own archives and answer RPC. That is “FUSE completely in userland”: the director is the kernel; the shim is the client.

---

# Part II — Architecture

## 3. Two eras of content authority

The codebase contains **two generations** of content serving. Both matter for understanding history and residual code.

### 3.1 Generation A — Snapshot + in-process Serve (legacy / transitional)

```text
Director/build time:  zips → vfs-zip → vfs-core Layer → merge → vfs-shared snapshot
Game process:         shim Engine + snapshot → Decision::Serve
                      zipserve maps container, synthetic handles, NtReadFile memcpy
```

Principles:

- Zip CD parse **outside** the game’s hot path when building the snapshot.  
- Shim never walks central directories; it only sees **resolved windows**.  
- Pure `vfs-core` / `vfs-redirect` for merge and decisions.

This path still appears in inject PE helpers, some shim paths, and fuse-bench baselines.

### 3.2 Generation B — Director userspace FUSE + thin shim (product path)

```text
Host Session:   mount ZipBackend layers → serve IpcServe (ring + bulk arena)
Game process:   thin FuseClient → OPEN/READ/CLOSE over ring → director backends
Launch:         inject dual-layer + optional PE hollow from Session::read_file
```

Principles:

- **No zip types in the director kernel** — zip is a **backend** implementing `Backend`.  
- Shim under managed root uses **RPC**, not local zip maps, when FUSE is live.  
- Hosts configure and **launch**; they rarely stream game data themselves.

**Current launch path (`vfs-launch`)** uses Generation B for content: `Session::mount_zip` + `serve` + `launch`.

---

## 4. End-to-end runtime topology

```text
┌──────────────────────────── Host process ─────────────────────────────┐
│  vfs-launch / custom host / C language binding                          │
│                                                                         │
│  Session                                                                │
│   ├─ virtual_root, overlay, state_dir                                   │
│   ├─ Director kernel (mount table, fh table, overlay resolve)         │
│   ├─ ZipBackend / DiskBackend / C vfs_backend_ops                     │
│   └─ IpcServe: N workers, named section, events                         │
│                                                                         │
│  launch(): locate shim+payload, hollow PE from VFS, run_target_with_shim│
└───────────────────────────────┬─────────────────────────────────────────┘
                                │  Named shared section
                                │  [ control ring | bulk data arena ]
                                │  Named events (wake, not file pipes)
┌───────────────────────────────▼─────────────────────────────────────────┐
│  Game process (Steam host image + hollowed zip PE + injected DLLs)      │
│                                                                         │
│  vfs-payload (early): NtCreate/Open/QueryAttributes* pre-init stubs     │
│  vfs-shim (full):     remaining NT/kernel32 hooks after loader lock     │
│  FuseClient:          OPEN/READ/CLOSE → ring → arena copy_to user buf   │
│  Synth handles:       map fh ↔ NT handle for game code                  │
└─────────────────────────────────────────────────────────────────────────┘
```

### 4.1 Isolation invariant

| Component | Opens layer zips? | Sees game user buffers? |
|-----------|-------------------|-------------------------|
| Director / ZipBackend | **Yes** | Only if remote WPM were used (removed); bulk uses shared arena |
| Thin FuseClient / hooks | **No** (FUSE path) | Yes — NtReadFile buffer |
| Payload / early redirects | Redirect table only | N/A |

This is the security/architecture story for a whitepaper: **the game process is not trusted with archive layout**; it only speaks a FUSE-like protocol over shared memory the parent created.

---

## 5. Userspace FUSE kernel (`vfs-director`)

### 5.1 Roles

| Type | Role |
|------|------|
| **`Director`** | Mount table, path normalize, overlay resolve, global file handles |
| **`Backend`** | `getattr` / `readdir` / `open` / `read` / `release` |
| **`Session`** | Paths + kernel + `IpcServe` + **`launch`** (host entrypoint) |
| **`IpcServe`** | Shared section, ring init, worker pool, env for child |

### 5.2 Mount and resolve semantics

1. Paths normalized to `/` separators; `..` escaping rejected.  
2. **Later mounts win** on `getattr` / `open` (walk mounts high → low).  
3. **`readdir`** merges children from all mounts that have the directory; same name → later wins (case-folded merge keys).  
4. **Zip backends** perform **case-insensitive** path lookup (Windows game paths).  
5. OPEN allocates a **global `fh`** mapping to `(backend, backend_handle, size, is_dir)`.  
6. READ/CLOSE dispatch only to that backend.

### 5.3 Host API (launch-centric)

Rust:

```rust
let mut s = Session::new();
s.set_root(r"C:\GameLayers\runtime");
s.set_overlay(r"C:\GameLayers\overlay");
s.set_state_dir(r"C:\GameLayers\vfs-state");
s.mount_zip(r"C:\GameLayers\1. Skyrim Special Edition.zip")?;
// … SKSE, SkyUI …
s.serve()?;
s.launch(&LaunchOpts {
    image: "skse64_loader.exe".into(),
    wait: true,
    hollow_pe: true,
    ..Default::default()
})?;
```

C: `crates/vfs-director/include/vfs.h` — `vfs_director_create`, `set_*`, `mount` / `mount_zip`, `serve`, **`vfs_launch`**. Host open/read are optional inspection tools.

### 5.4 Why backends are separate from the kernel

Zip knowledge (CD, ZIP64, local headers, Stored windows) is confined to **`vfs-zip::ZipBackend`**. The director only sees `Backend`. That enables:

- Custom language backends via C function pointers  
- Disk backends for tests  
- Future pack formats without rewriting the kernel  

The ops contract lives in **`vfs-protocol`** (`Backend`, `Stat`, KIND_*, status codes) so zip does not depend on the heavy host stack.

---

## 6. Zip backend and Stored windows

### 6.1 ZIP64 central directory

`ZipBackend::open`:

1. Open container; locate EOCD / ZIP64 EOCD.  
2. Read central directory entries.  
3. For each Stored file, compute **true data offset** from the **local file header** (local name/extra lengths can differ from the central directory).  
4. Build path → `{ data_off, size, mtime, is_dir }` plus a **casefold index**.  
5. OPEN keeps an OS `File` per open handle; READ seeks to `base + offset`.

No full-archive extract. The 16 GB base zip is **never** copied as a whole; the OS page cache faults container pages as windows are read.

### 6.2 Why Stored-only is a feature, not a limitation (for this product)

For distribution of already-built game/mod packages as Stored archives:

- Random access is seek + read.  
- No CPU inflate on every BSA page.  
- IPC throughput can approach local disk for warm cache.  

Deflated packages would require a different design (inflate-to-cache, seekable compression, or extract).

### 6.3 Legacy `read_layer`

`vfs-zip::read_layer` still builds a `vfs-core::Layer` of zip-window **source blobs** for transitional inject/snapshot code. The **launch content path** no longer double-parses every zip solely to discard PE bytes: mounts use `ZipBackend` once; PE hollow reads through the Session/VFS.

---

## 7. IPC: control ring and bulk arena

### 7.1 Control ring (`vfs-ipc`)

- Caller-owned shared segment; no OS in the pure ring crate.  
- Slots: FREE → CLAIMED → REQUESTED → PROCESSING → COMPLETED → free.  
- Fixed payload capacity (default **1 MiB**).  
- Spin notifiers for tests; **Windows events** in production (`vfs-win::EventNotifier`) so idle rings do not burn a core forever.  

**Recursion rule:** IPC must not use hooked `NtCreateFile`/`NtReadFile` (no pipes/sockets through the game’s own hooks). Shared memory + events.

### 7.2 Bulk arena

Large READs set `FLAG_READ_BULK`:

1. Director fills a **per-slot bank** in the shared section (disk/zip → bank via `fill_bank` / `read_into`, no intermediate `Vec` on the happy path).  
2. Ring response is tiny: status + bytes_read + arena offset (BULK bit).  
3. Client `SharedSeg::copy_to` into the game buffer (NtReadFile buffer on the phase-1 path).

Default arena ~**32 MiB** → ~1 MiB banks at 32 slots.

**Copies (bulk):** typically two — container → arena, arena → user buffer.

### 7.3 Performance (order of magnitude)

Release spin-bench numbers (host-dependent; see `docs/benchmarks/`):

| Metric | Ballpark |
|--------|----------|
| HEARTBEAT p50 | ~1 µs |
| Small READ p50 | ~5–10 µs |
| Sequential bulk (disk-like) | ~1–2 GiB/s class in-process spin |
| Sequential bulk (zip ESM) | ~0.8 GiB/s class after bulk optimizations |
| vs OpenTable direct | ~1.4–3× overhead depending on cache |

Optimizations that mattered: open-file reuse, unlock during I/O, bulk arena, 1 MiB payload, worker pool, skip readahead on large bulk, decode-into / copy_to.

Remote `WriteProcessMemory` into game buffers was prototyped and **removed**: bulk arena + local copy was simpler and faster for the measured disk path.

---

## 8. Thin shim and FUSE client

### 8.1 Hooks (full shim)

| API | Role under FUSE |
|-----|-----------------|
| `NtCreateFile` / `NtOpenFile` | Path under root → OPEN; synth handle |
| `NtReadFile` | `read_fragmented` into **user buffer** (no intermediate tmp) |
| `NtQuery*Attributes*` / directory enum | GETATTR / READDIR |
| `NtClose` | CLOSE |
| CPIW / GMF | Child inject, path spoofing |

When FUSE is not live, legacy Engine/snapshot/zipserve paths may still apply (transitional).

### 8.2 FuseClient

- Connects via env: section name, map size (`VFS_RING_BYTES` = **full mapping**), arena length, events.  
- Large fragments: bulk + pipeline (`submit_many`).  
- Small fragments: inline ring payload.  

### 8.3 Dual-layer inject (why two DLLs)

| Layer | When | Owns |
|-------|------|------|
| **vfs-payload** (`no_std`) | Pre-`LdrpInitializeProcess` complete | Permanent 14-byte jumps on 4 ntdll stubs: Create/Open/QueryAttributes* |
| **vfs-shim** | After loader lock; kernel32 available | Full Engine + remaining detours via `retour`; **does not** re-patch the four early stubs |

**Handoff:** early body uses a redirect table for static-import DLLs, then secondary dispatch into full Engine once live. Double-patching the same stubs is a classic failure mode this design avoids.

Sequence sketch:

1. CreateProcess host (Steam Skyrim) suspended / gated.  
2. Arm payload + inject full shim.  
3. Ready event / ready file.  
4. Optional OEP late-entry sync bootstrap.  
5. Hollow primary PE from zip RAM.  
6. Resume.

---

## 9. Process hollowing and Steam DRM

### 9.1 Why hollow

- Managed root has **no** durable `skse64_loader.exe` / `SkyrimSE.exe` payload files.  
- CreateProcess needs a real image file.  
- Steam checks process image authenticity; hollowed arbitrary system EXEs fail.

**Approach:** CreateProcess **real Steam** `SkyrimSE.exe`, then **WriteProcessMemory** zip PE image into the process (and fix PEB/relocs as needed). Archive PE never becomes a durable file under GameLayers.

### 9.2 Game-local DLLs and SEC_IMAGE

Zip-backed PE loaded as **SEC_IMAGE** is not a normal file mapping. The stack has evolved through:

- Synthetic sections + local manual map  
- Remote LoadLibrary of Steam/system modules  
- Stage pipelines to retarget LDR entries and prove zip PE identity in remote process memory  

These details are highly Windows-version and Steam-sensitive; the whitepaper-relevant claim is:

> **File content** for data files can be pure windows; **process images** still require a real host file for CreateProcess and careful remote PE management for DRM and the loader.

### 9.3 Plugins enablement

SSE enablement files under `%LOCALAPPDATA%\Skyrim Special Edition\` (`Plugins.txt`, `loadorder.txt`) are **config**, not archive extract. Launch enumerates top-level `Data/*.{esm,esp,esl}` via director readdir and writes enable lists so SkyUI and masters load.

---

## 10. Crate architecture (after consolidation)

Workspace root uses `panic = "abort"` for the no_std payload.

### 10.1 Product path

| Crate | Responsibility |
|-------|----------------|
| **vfs-protocol** | Opcodes, status, wire codecs, **`Backend` ops** |
| **vfs-ipc** | Control ring, **DataArena**, default caps |
| **vfs-win** | Named sections, event notifiers |
| **vfs-zip** | ZipBackend + legacy `read_layer` |
| **vfs-director** | Director, Session, IpcServe, C ABI, launch |
| **vfs-inject** | CreateProcess, inject, hollow, PE tools |
| **vfs-payload** | Early no_std stubs |
| **vfs-shim** / **vfs-shim-dll** | Hooks + injectable DLL |
| **vfs-launch** | Skyrim CLI host |

### 10.2 Legacy / transitional

| Crate | Responsibility |
|-------|----------------|
| **vfs-core** | Pure merge tree, Source disk/zip-window |
| **vfs-shared** | Flattened snapshot |
| **vfs-redirect** | Decision core for snapshot Engine |
| **vfs-server** | Legacy tree Server + OpenTable (benches/e2e) |

### 10.3 Dependency rationale (why not one crate)

- **payload** isolation (`no_std`)  
- **protocol** leaf for zip without host/inject  
- **ipc** free of Win32 except through vfs-win  
- **inject** vs **shim** (parent vs child)  
- **director** is the multi-language host surface  

Recently merged: `vfs-ops` → `vfs-protocol`; `DataArena` → `vfs-ipc` (director no longer depends on legacy server for the arena).

---

## 11. Launch workflow (concrete)

Expected layout:

```text
C:\GameLayers\
  1. Skyrim Special Edition.zip
  2. SKSE 2.2.6.zip
  3. SkyUI 6.11.zip
  runtime\          # virtual root (payload wiped before launch)
  overlay\
  vfs-state\        # fuse.cfg, shim.cfg, ready.flag
```

`vfs-launch` steps:

1. Discover numbered `*.zip`.  
2. Wipe residual payload files under runtime.  
3. `Session::mount_zip` each layer (single CD parse per zip).  
4. Verify PE exists via getattr; enable plugins from readdir.  
5. `serve()` — section, workers, env for child.  
6. `launch(skse64_loader.exe | SkyrimSE.exe)` with hollow.  
7. Detach: `mem::forget(session)` so IPC outlives the call (host process must stay alive).

`--probe` validates TES4 heads and zero root payload files without starting the game.

---

## 12. Security, isolation, and trust model

| Trust boundary | Claim |
|----------------|--------|
| Game process | Untrusted with archive layout; may only use FUSE protocol |
| Shared section | Parent-created; child maps by name/env |
| C backends | Host must provide Sync-safe userdata; session C API is single-threaded |
| PE hollow | Uses elevated parent rights on the child; expected for inject tools |

Threat model is **modding convenience and anti-extract**, not multi-tenant sandboxing of hostile games. Still, keeping zip I/O out of the game reduces the surface of “what the game can accidentally touch.”

---

## 13. Performance story (whitepaper-ready claims)

### 13.1 Cost centers

1. Disk/zip sequential bandwidth  
2. Memory copies (container → arena → user)  
3. Per-RPC fixed costs (slot claim, atomics, wake)  
4. Open/seek vs pooled handles  

### 13.2 What moved the needle

- Keep OS handles on OPEN  
- Do not hold global maps across I/O  
- Bulk arena + 1 MiB-class chunks  
- Multiple server workers  
- Avoid intermediate Vec on bulk fill  
- Skip useless readahead on large sequential bulk  
- Measure with release builds and real zip windows  

### 13.3 What did not win

- Director WPM into game buffers for large sequential (worse/noisier than bulk arena on disk)  
- Pipelining alone with a single overloaded server thread  

Evidence lives under `docs/benchmarks/` and `docs/performance-rpc-analysis.md`.

---

## 14. Lessons learned (engineering narrative)

1. **CreateProcess is not a VFS problem alone** — DRM and image path force a real host PE.  
2. **Static imports race the loader** — pre-init ownership of a few NT stubs is mandatory.  
3. **Do not double-patch ntdll** — dual-layer ownership model.  
4. **Snapshot Serve vs FUSE RPC** — local zip maps in the game fight the isolation story; director RPC is cleaner and multi-language friendly.  
5. **Stored zip windows scale** — ZIP64 + local header fixup is enough for 16 GB bases.  
6. **Measure copies** — “zero copy” marketing fails if you still Vec through the ring.  
7. **Migration debt is real** — dual stacks (tree Server vs IpcServe) must be documented and retired deliberately.  
8. **Casefold on Windows is not optional** for game paths.  

---

## 15. Future directions

| Direction | Notes |
|-----------|--------|
| Retire legacy tree Server content path | Point all benches at IpcServe |
| Drop remaining `read_layer` from inject | PE only via backends |
| Feature-split director (kernel vs launch) | Smaller pure-kernel cdylib |
| Readahead in backends | Recover OpenTable B5 benefits |
| Overlay write productization | Spec exists historically |
| Deflated zip / other packs | New Backend implementations |
| Multi-session env | Avoid process-global set_var |

---

## 16. Related documents

| Path | Content |
|------|---------|
| [overview.md](./overview.md) | Short overview |
| [superpowers/specs/2026-07-15-director-fuse-thin-shim-design.md](./superpowers/specs/2026-07-15-director-fuse-thin-shim-design.md) | Thin shim + director FUSE |
| [superpowers/specs/2026-07-15-userspace-fuse-director-c-abi-design.md](./superpowers/specs/2026-07-15-userspace-fuse-director-c-abi-design.md) | Session + C ABI |
| [superpowers/specs/2026-07-14-zip-backed-layers-design.md](./superpowers/specs/2026-07-14-zip-backed-layers-design.md) | Zip windows |
| [superpowers/specs/2026-07-14-dual-layer-inject-handoff-design.md](./superpowers/specs/2026-07-14-dual-layer-inject-handoff-design.md) | Dual-layer inject |
| [superpowers/specs/2026-07-13-vfs-ipc-control-ring-design.md](./superpowers/specs/2026-07-13-vfs-ipc-control-ring-design.md) | Control ring |
| [benchmarks/](./benchmarks/) | Latency/throughput |
| [performance-rpc-analysis.md](./performance-rpc-analysis.md) | Perf analysis |

---

## 17. Glossary

| Term | Meaning |
|------|---------|
| **Director** | Parent/host userspace FUSE kernel |
| **Session** | Host entrypoint: mounts + serve + launch |
| **Backend** | Zip/disk/C provider of getattr/open/read |
| **Stored window** | Uncompressed zip payload region |
| **Bulk arena** | Shared banks for large READ payloads |
| **Synth handle** | Fake NT handle for virtual files |
| **Hollow** | Replace suspended process image with zip PE |
| **Dual-layer inject** | Pre-init payload + full shim without double-patch |
| **Snapshot** | Flattened vfs-core tree for legacy Serve path |

---

## 18. Closing thesis (for a whitepaper abstract)

> Modern Windows games can be virtualized for modding **without kernel drivers** and **without extracting multi-gigabyte archives**, by combining (1) **Stored ZIP byte windows**, (2) a **parent-process FUSE authority** over shared memory, (3) a **thin NT-API client** in the game, and (4) **careful PE process creation** that satisfies CreateProcess and platform DRM. The decisive architectural split is that **content I/O belongs to the director**, while the game process only remaps paths and consumes a FUSE-like protocol—yielding isolation, multi-language hosts, and competitive sequential throughput once bulk transfer is engineered as carefully as the hooks.

---

*End of summary. For implementation plans and historical task breakdowns, see `docs/superpowers/plans/`.*

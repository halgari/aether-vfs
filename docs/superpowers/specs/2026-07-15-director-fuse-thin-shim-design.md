# Director-Centric Userland FUSE + Thin Shim — Design Spec

**Status:** Approved (design dialogue 2026-07-15); ready for implementation planning.  
**Date:** 2026-07-15  
**Type:** Architecture / next-phase production design  
**Depends on:** `vfs-core`, `vfs-zip`, `vfs-ipc`, `vfs-server` (starter opcodes), `vfs-win`, `vfs-shim` hooks, `vfs-launch`  
**Supersedes (for data path under managed root):** in-shim `SnapshotReader` + `Decision::Serve` + `zipserve` container maps as the authority for file content.

---

## 1. Goal

Move **all virtual filesystem data access and retrieval** into the **director** (parent process). The injected shim becomes a **very thin FUSE-like client**: it translates NT calls into IPC opcodes and applies responses. It does **not**:

- open or map layer zip containers,
- hold a vfs-core tree or published snapshot for Serve,
- implement local zip-window reads.

The mental model:

> Fully userland FUSE. The **director is the kernel**. The **shim is the FUSE client** inside the game process.

### 1.1 Decisions locked in dialogue

| Decision | Choice |
|----------|--------|
| Data plane | **Pure FUSE RPC** for every READ (optimize IPC later; no shared bulk maps in this phase) |
| Process topology | **Parent director** — `vfs-launch` (or a process it owns) runs the server for the game lifetime |
| Open identity | **FUSE-style file handles** — `OPEN(path) → fh`; `READ(fh, offset, len)`; `CLOSE(fh)` |
| First slice | **Thin shim + full read path** (GETATTR/READDIR/OPEN/READ/CLOSE). PE hollow / game-local Stage B–D stay parent-local zip reads for one more slice |
| Approach | **Extend existing `vfs-ipc` + `vfs-server`** (not a greenfield protocol) |

### 1.2 Non-goals (this phase)

- Shared-memory bulk maps / zero-copy READ optimizations  
- Moving PE hollow or game-local DLL Stage B–D to RPC  
- Write path / overlay mutations via FUSE opcodes (`WRITE`/`RENAME`/`DELETE`/`MKDIR`)  
- Permanent dual-mode (snapshot Serve **and** FUSE) in production  
- Deflated zip entries  
- Multi-session long-lived daemon (multiple games on one server)

---

## 2. Roles

```
┌──────────────── vfs-launch (director / parent) ────────────────┐
│  vfs-zip → layers → vfs-core tree (+ overlay state later)        │
│  Open-file table: fh → source window / overlay path              │
│  RingServer loop (dedicated thread + event notifier)             │
│  CreateProcess + inject thin shim; pass section name + root      │
│  PE hollow: parent still reads zip PE bytes locally (unchanged)  │
└────────────────────────────┬─────────────────────────────────────┘
                             │ shared-memory control ring
                             │ (events/futex — not hooked file APIs)
┌────────────────────────────▼─────────────────────────────────────┐
│  Game process — thin shim                                        │
│  Nt* hooks → RingClient.submit(opcode, payload) → fill buffers   │
│  Synth handles store only { fh, size, is_dir, pos }              │
│  No vfs-core tree, no snapshot Serve, no zip maps                │
└──────────────────────────────────────────────────────────────────┘
```

| Component | Owns |
|-----------|------|
| **Director** | Layer merge, zip I/O, overlay resolution, open-file table, all content bytes, ring server |
| **Shim** | Root path classification, NT↔protocol marshalling, synth handle table (metadata + fh only), pass-through outside root |
| **vfs-inject (parent)** | Process create, inject, hollow PE from parent-local zip read (this phase) |

---

## 3. Transport

Reuse **`vfs-ipc`** control ring on a **caller-owned shared section** (`vfs-win` mapping).

### 3.1 Invariants (G11 / recursion)

- Client and server **must not** perform hooked `NtCreateFile` / `NtReadFile` / etc. on the IPC path.
- Wakeup via **Windows events** (or equivalent non-file wait), not pipes/sockets that route through create/read hooks.
- Spin notifier remains for unit tests; production launch uses a real `Notifier` implementation.

### 3.2 Geometry (MVP defaults — tunable)

| Parameter | Suggested default | Rationale |
|-----------|-------------------|-----------|
| `slot_count` | 32–64 | Concurrent reads from game worker threads |
| `payload_cap` | 256 KiB (or 64–512 KiB) | One READ response fits a practical chunk; shim fragments larger I/Os |
| Ring magic/version | existing `VFIP` / v1 | Stay compatible with current layout |

If a single READ request asks for more than `payload_cap - header`, the **client** issues multiple READ RPCs and concatenates into the NT buffer.

### 3.3 Section discovery

Director creates a named section (or anonymous section + handle inheritance — prefer **named** for simplicity):

- Name pattern: e.g. `Local\vfs_ring_{director_pid}` or random suffix written into shim config.
- Shim config (path or env) carries: section name, `root` NT/Win32 path, `payload_cap`, optional timeout.

No snapshot file path is required for the thin shim.

---

## 4. Protocol

All little-endian. Decode is total (never panic): malformed → `ST_BAD_REQUEST`.

Opcodes already reserved in `vfs-ipc::layout` (extend handlers; do not renumber):

| Opcode | Const | MVP |
|--------|-------|-----|
| GETATTR | 1 | yes |
| READDIR | 2 | yes |
| OPEN | 3 | yes |
| READ | 5 | yes |
| CLOSE | 11 | yes |
| HEARTBEAT | 13 | yes |
| MATERIALIZE / WRITE / … | 4, 6, … | **no** (deferred) |

### 4.1 Status codes

| Code | Meaning |
|------|---------|
| `ST_OK` (0) | Success |
| `ST_NOT_FOUND` (-1) | Path missing |
| `ST_NOT_A_DIRECTORY` (-2) | READDIR on file |
| `ST_BAD_REQUEST` (-3) | Malformed payload / unknown opcode |
| `ST_IO_ERROR` (-4) | **new** — director I/O failure |
| `ST_IS_DIR` (-5) | **new** — READ on directory fh |
| `ST_BAD_FH` (-6) | **new** — unknown/closed fh |
| `ST_NO_SPACE` (-7) | **new** — response exceeds payload_cap (should not happen if client fragments correctly; server may still enforce) |

Map to NTSTATUS in the shim (illustrative):

| ST_* | NTSTATUS sketch |
|------|-----------------|
| NOT_FOUND | `STATUS_OBJECT_NAME_NOT_FOUND` |
| BAD_FH / director down | `STATUS_INVALID_HANDLE` / `STATUS_DEVICE_NOT_READY` |
| IO_ERROR | `STATUS_UNEXPECTED_IO_ERROR` |
| IS_DIR | `STATUS_INVALID_DEVICE_REQUEST` or `STATUS_FILE_IS_A_DIRECTORY` |

### 4.2 Message shapes

**Path request** (GETATTR, READDIR, OPEN path part): UTF-8 virtual path relative to managed root **or** full Win32/NT path that the director normalizes to vpath. Prefer **vpath** (`Data/Skyrim.esm`) after shim strips root prefix — keeps payloads small and stable.

**GETATTR response** (existing 18-byte form):  
`found:u8, is_dir:u8, size:u64, mtime:i64`

**READDIR response** (existing):  
`count:u32` + entries `(name_len, name, is_dir, size, mtime)`  
MVP: single response must fit `payload_cap`; if overflow, return `ST_NO_SPACE` or a paged extension in a follow-up slice. Games rarely readdir huge dirs at once; acceptable risk for MVP.

**OPEN request:**

```
flags:u32          // bit0 read, bit1 write (write → ST_BAD_REQUEST this phase unless overlay write later)
path: UTF-8        // remainder of payload
```

**OPEN response:**

```
fh:u64
size:u64
is_dir:u8
_pad: [u8;7]       // align
```

**READ request:**

```
fh:u64
offset:u64
len:u32
_pad:u32
```

**READ response:**

```
bytes_read:u32
_pad:u32
data: [u8; bytes_read]
```

EOF: `ST_OK` with `bytes_read == 0` (or short read). Offset past end → `bytes_read == 0`.

**CLOSE request:** `fh:u64`  
**CLOSE response:** empty payload, `ST_OK` or `ST_BAD_FH`.

**HEARTBEAT:** empty / empty; used at shim bootstrap and liveness.

### 4.3 Virtual path rules

- Casefolding and layer precedence stay in **vfs-core** on the director.
- Shim: if NT path is under managed root, strip to vpath (forward slashes or as core expects), else **pass-through** to real ntdll.
- Paths outside root never call the ring.

---

## 5. Director open-file table

```text
struct OpenFile {
  kind: File | Dir,
  size: u64,
  mtime: i64,
  // File:
  source: ZipWindow { container: PathBuf, data_offset: u64, length: u64 }
        | Disk { path: PathBuf }   // overlay or legacy disk source
  // Dir: no data plane
}
fh: u64 → OpenFile   // fh allocator: monotonic counter, never zero
```

### 5.1 OPEN algorithm

1. Normalize path → vpath.  
2. Resolve overlay (if configured): present file → Disk source; whiteout → NOT_FOUND.  
3. Else `tree.resolve(vpath)` / getattr:  
   - File + `Source::ZipWindow` → bind window  
   - File + disk source → Disk path  
   - Dir → Dir entry  
   - NotFound → ST_NOT_FOUND  
4. Allocate `fh`, insert table, return OPEN response.

### 5.2 READ algorithm

1. Lookup fh → else ST_BAD_FH.  
2. If Dir → ST_IS_DIR.  
3. Clamp `len` to remaining bytes and to max data that fits in payload (`payload_cap - 8`).  
4. ZipWindow: open container read-only (or use a **director-side** map cache — never in shim), seek `data_offset + offset`, read.  
5. Disk: same against overlay/disk path.  
6. Encode READ response.

Director may cache `File` handles or full-file maps **internally** for performance; that is an implementation detail and must not leak into the shim.

### 5.3 CLOSE / session teardown

- CLOSE removes fh.  
- Single-session MVP: when game process exits, director drops **all** fhs.  
- Optional later: `REGISTER_PROCESS` + per-pid fh namespaces.

### 5.4 Concurrency

- Server thread pool optional later; MVP single `serve_one` loop is OK if game is mostly serialized on I/O — but Skyrim can multi-thread reads. **Prefer multi-slot ring + mutex around open table + allow concurrent READ on different fhs.**  
- Spec requirement: open table guarded; zip reads may use per-container locks or independent `File` handles per READ.

---

## 6. Thin shim

### 6.1 Removed under managed root (end state of this slice)

- Publishing/consuming **snapshot** for `Decision::Serve`  
- **`zipserve`** synthetic opens that map zip containers in the game  
- Local SEC_IMAGE-from-zip for **Data** content (PE hollow path separate)

### 6.2 Synth handle table (shim-local only)

```text
synth HANDLE → {
  fh: u64,          // director file handle
  size: u64,
  is_dir: bool,
  position: u64,    // for NtReadFile without explicit offset
}
```

Tagged high-bit handles remain so hooks distinguish synth vs kernel handles (same idea as today, without zip window pointers).

### 6.3 Hook mapping

| NT API | Behavior under root |
|--------|---------------------|
| NtCreateFile / NtOpenFile | OPEN RPC → mint synth handle; dirs allowed |
| NtReadFile | fragment READ RPCs into user buffer; update position |
| NtQueryInformationFile (size, position, …) | from cached open fields; refresh via GETATTR if needed |
| NtQueryDirectoryFile(Ex) | READDIR RPC → marshal FILE_*_DIR_INFORMATION |
| NtQueryAttributesFile / Full | GETATTR RPC |
| NtClose | CLOSE RPC + free synth |
| NtCreateSection on synth | Data section: either deny and force read path, or buffer via READ into private commit (MVP: **private commit via READ**, no director section). SEC_IMAGE for arbitrary files under root: not required for BSA path; PE still hollowed by parent |
| Outside root | trampoline unchanged |

### 6.4 Failure policy

- Director dead / HEARTBEAT fail / ring submit timeout → opens under root fail with `STATUS_DEVICE_NOT_READY` (or similar). **Do not** pass through to empty managed root (false empty tree).  
- Outside root: unaffected.

### 6.5 Bootstrap

1. Read thin config (section name, root, caps).  
2. Open shared section; construct `RingClient`.  
3. HEARTBEAT.  
4. Signal ready (existing ready event / file).  
5. Install hooks (dual-layer early payload may still own path stubs; secondary dispatch calls into thin client instead of Engine::decide Serve).

**Dual-layer note:** early payload redirect table for static-import DLLs may remain for PE names; content for those may still be disk/Steam until PE slice. Data/ and general root files use FUSE only.

---

## 7. Launch wiring (`vfs-launch`)

1. Discover layers; `read_layer`; build tree; optional overlay root.  
2. Create shared section + `Ring::init`; spawn director server thread.  
3. Write thin shim config (no snapshot blob required for Serve).  
4. Inject shim; wait ready.  
5. Hollow skse/Skyrim as today (parent `read_source_bytes` from zip).  
6. Resume; all managed-root data I/O is RPC.  
7. On game exit: stop server thread; drop open table; close section.

`--probe` mode: can run a small in-process or sibling client against the same director without GPU game — assert sizes/magic for `Data/Skyrim.esm` and SkyUI paths **only via OPEN/READ**.

---

## 8. Crate / module impact

| Crate | Change |
|-------|--------|
| `vfs-server` | Extend `proto` + `dispatch` with OPEN/READ/CLOSE; stateful `Server` with open table + source I/O; not pure tree-only |
| `vfs-ipc` | Real Windows `Notifier` if missing; document payload_cap guidance; opcode consts already exist |
| `vfs-win` | Named section create/open helpers if not sufficient |
| `vfs-shim` | Thin client module; Engine Serve/zipserve unused for root; hooks call client |
| `vfs-shim-dll` | Config parsing for ring section |
| `vfs-launch` | Start server thread; thin config; remove snapshot-only assumption for data path |
| `vfs-redirect` | May remain for pure unit tests; production shim need not load full snapshot |
| `vfs-shared` | Optional; not required for thin data path |

Prefer keeping protocol encode/decode in `vfs-server::proto` (or extract `vfs-fuse-proto` later if shared with shim — **shim must not depend on vfs-core**). Shared wire types: either:

- thin `vfs-protocol` crate (`forbid(unsafe)`, no core), or  
- duplicate minimal encode in shim (worse), or  
- shim depends on `vfs-server` proto module via a small shared crate.

**Recommendation:** add `crates/vfs-protocol` with opcodes + encode/decode only; both server and shim depend on it. If too heavy for first PR, put encode in `vfs-server` and re-export a `protocol` module that shim can depend on without pulling zip — but `vfs-server` currently pulls core. Cleanest: **`vfs-protocol`** pure crate.

---

## 9. Testing & acceptance

### 9.1 Automated

1. Protocol round-trips for OPEN/READ/CLOSE.  
2. Director unit: open zip-window source from a tiny fixture zip; READ full content; CLOSE; BAD_FH after.  
3. Threaded IPC: client OPEN/READ/CLOSE against server thread on real shared segment.  
4. Fragmentation: READ 1 MiB file with payload_cap 64 KiB → byte-identical.  
5. Shim/integration: under test harness, Nt-level or client-level path returns correct bytes **without** loading zipserve maps in the client process (assert no container CreateFile in client — optional).  

### 9.2 Manual / launch

1. `vfs-launch --probe` (or extended probe): `Data/Skyrim.esm`, SkyUI esp/bsa size+magic via director RPC.  
2. Managed root still **zero** archive payload files.  
3. Stretch: dual game launch still meets prior SKSE/SkyUI bar.

### 9.3 Explicit non-regression

- Hollow still logs zip PE write for main EXEs.  
- Game-local Stage B–D still parent/inject-side until later phase.

---

## 10. Migration plan (implementation order)

1. **`vfs-protocol`** (or extend `vfs-server::proto`) — OPEN/READ/CLOSE codecs + new status codes.  
2. **Stateful director** — open table + zip/disk READ; unit tests with fixture zip.  
3. **Windows notifier + named section** — playable wait path.  
4. **`vfs-launch` embeds server thread** — probe client without full game.  
5. **Thin shim client** — replace Serve path under root; keep pass-through.  
6. **Delete/disable** in-shim zipserve for root files; snapshot publish optional for debug only.  
7. **Probe + game smoke**.  

Each step should leave `cargo test` green for touched crates.

---

## 11. Risks

| Risk | Mitigation |
|------|------------|
| RPC latency on BSA streams | Accepted for this phase; later bulk maps / larger payload_cap / pipelining |
| READDIR overflow | Cap + error; page in follow-up |
| Deadlock if server uses hooked APIs | Server only in parent; parent does not install game hooks |
| Early payload still patches create/open | Secondary dispatch must call thin client, not old Engine Serve |
| Multi-threaded game + single server thread | Multi-slot ring; consider worker pool if blocked |

---

## 12. Follow-on phases (out of scope now)

1. Zero-copy OPEN → shared view mapping (director maps window; shim READ is local memcpy).  
2. PE hollow / game-local DLL content via OPEN/READ of PE paths from director only.  
3. WRITE path + overlay opcodes.  
4. Long-lived multi-client daemon.  

---

## 13. Summary

The next phase turns the existing control ring into a **real FUSE control plane**: parent director owns the tree and all zip I/O; the shim only tracks opaque `fh`s and shuttles bytes over RPC. Pure-RPC READ is intentional simplicity; PE hollow remains parent-local until a later slice. Success is probe/game data paths working with **no zip access and no file tree inside the shim**.

# VFS: Deep Technical Summary

A full write-up of the `C:\oss\vfs` workspace: architecture, how zip-backed layers work, how Skyrim + SKSE + SkyUI launch without extracting archives, and the hard lessons from getting PE/DLL loading correct under Windows and Steam DRM.

---

## 1. What this system is

**VFS** is a usermode, MO2/USVFS-lineage virtual filesystem for game modding. Its practical mission for the current milestone is:

> Launch **Skyrim Special Edition** with **SKSE** and **SkyUI**, serving base game + mods **straight from Stored ZIP archives under `C:\GameLayers`**, with **zero archive→disk extract** of PE/BSA/ESP content (no durable materialize under the managed root, no TEMP staging of archive PE).

The three layer archives (bottom → top):

| Order | Archive | Role |
|------|---------|------|
| 1 | `1. Skyrim Special Edition.zip` (~16 GB, ZIP64) | Base game: `SkyrimSE.exe`, `Data/*.bsa`, masters, … |
| 2 | `2. SKSE 2.2.6.zip` | `skse64_loader.exe`, SKSE DLLs, script PEXes |
| 3 | `3. SkyUI 6.11.zip` | `Data/SkyUI_SE.esp`, `Data/SkyUI_SE.bsa`, translations |

**Decisive physical fact:** every entry in all three archives is **Stored** (method 0, no compression). A “file” in a layer is just a **byte window** `[offset, offset+size)` inside a real zip file on disk. Deflated entries are rejected, not decompressed.

The managed install root is typically:

```
C:\GameLayers\runtime\     # empty skeleton dirs only (Data/, Scripts/, …)
C:\GameLayers\overlay\     # optional write overlay (creates/deletes)
C:\GameLayers\vfs-state\   # shim config, ready flags, GMF spoof logs
C:\GameLayers\1…3 *.zip    # the only archive-backed content on disk
```

AppData `Plugins.txt` (plugin enablement) is allowed as non-archive config.

---

## 2. Design principles

1. **Director builds; shim serves.** Zip parsing and tree merge happen **out of process** (or at least outside the game’s hot path) when building a **snapshot**. The injected shim never walks zip central directories; it only sees resolved **windows** or **redirect targets**.
2. **Pure core.** `vfs-core` is `#![forbid(unsafe_code)]`, does no I/O, and only merges layered trees.
3. **Fail-safe hooks.** Unknown paths, bad snapshots, or undecidable opens → **pass through** to the real kernel (or soft success on synth queries), not crash the game.
4. **Zero extract for content.** No PE/BSA/ESP bytes from archives written under managed root or TEMP. Empty directory skeletons are fine.
5. **Windows still needs a real host image for CreateProcess.** Pure VFS paths cannot be the kernel’s `ProcessImageFileName` for Steam DRM. Solution: **hollow** a real on-disk host (Steam’s `SkyrimSE.exe`) and **WriteProcessMemory** zip PE bytes into it.
6. **Dual-layer inject.** Static imports of the primary EXE must be virtualizable **before** `LdrpInitializeProcess` finishes; full Engine (dir enum, overlay, CPIW, …) installs after loader lock is released.

---

## 3. Crate map

Workspace root: `C:\oss\vfs` (Cargo workspace, `panic = "abort"` for `no_std` payload).

| Crate | Role |
|-------|------|
| **vfs-core** | Pure VFS tree: layers, tombstones, resolve, casefold, path normalize, **source encoding** (disk vs zip-window). |
| **vfs-zip** | ZIP64 central-directory reader → `Layer` of zip-window sources. Stored only. |
| **vfs-shared** | Flattened **snapshot** bytes (seqlock-friendly layout) + builder/reader. Published for shim/server. |
| **vfs-redirect** | Pure **decision core**: NT path + snapshot → `PassThrough` / `Redirect` / `Serve` / `Deny`; dir merge; whiteouts. |
| **vfs-shim** | In-process **engine** + **ntdll/kernel32 hooks** + **zipserve** synthetic handles + CPIW hollow. |
| **vfs-shim-dll** | Injectable DLL entry (`DllMain` / sync bootstrap for dual-layer). |
| **vfs-payload** | `no_std` early payload: patches 4 ntdll stubs **pre-init**; redirect table for static-import DLLs. |
| **vfs-inject** | CreateProcess suspended, inject, pre-init arm, dual-layer handoff, **ghostly** PE hollow/map, game-local DLL pipeline. |
| **vfs-launch** | End-user director: discover zips, build snapshot, skeleton root, hollow-launch SKSE/Skyrim. |
| **vfs-win** | Shared-memory section helpers for IPC rings. |
| **vfs-ipc** | Recursion-free control ring over caller-owned segments. |
| **vfs-server** | Optional out-of-process authoritative VFS (snapshot + IPC). |
| **vfs-fixture-*** | Test fixtures (static-import EXE, vproxy). |

Rough dependency flow:

```
                    vfs-launch (director)
                         │
         ┌───────────────┼────────────────┐
         ▼               ▼                ▼
      vfs-zip        vfs-core         vfs-inject
         │               │                │
         └──────► vfs-shared ◄──── vfs-redirect
                         │                │
                         ▼                ▼
                    vfs-shim ◄──── vfs-payload
                         │
                    vfs-shim-dll (in game process)
```

---

## 4. Layer model and snapshot

### 4.1 Inputs

Each layer is a list of `InputEntry`:

- `vpath` — virtual path under the managed root (`Data/Skyrim.esm`, …)
- `kind` — `File` | `Dir` | `Tombstone`
- `source` — opaque blob (disk path **or** zip-window encoding)
- `size`, `mtime`

Later layers win on conflict. Tombstones hide lower-layer names (first-class deletes).

### 4.2 Source encoding (`vfs-core::source`)

Disk paths never start with NUL. Zip windows use a tag:

```
Disk:       <UTF-8 path bytes>
ZipWindow:  0x00 | u64 LE offset | <UTF-8 container path>
```

Size lives on the tree node, not in the blob. Decode is fail-safe: truncated zip blobs fall back to “disk” interpretation rather than panic.

### 4.3 Zip reader (`vfs-zip`)

- Opens the zip; finds EOCD / ZIP64 EOCD.
- Walks central directory; requires compression method **Stored**.
- Reads **local file header** for true data offset:
  `data_offset = local_header_off + 30 + local_name_len + local_extra_len`
  (local name/extra can differ from central directory).
- Emits `encode_zip_window(data_offset, zip_path)` per file.

No full-file extract; the 16 GB base zip is never copied. Only directory structures and window metadata are materialized in the snapshot.

### 4.4 Tree + snapshot

`vfs-core::build` merges layers into a `VfsTree` (casefolded lookup, dir walks, cache keys).

`vfs-shared` flattens that into a compact **snapshot** buffer the shim can mmap or receive as config. The shim’s `SnapshotReader` validates layout and answers resolve queries without re-parsing zips.

---

## 5. Redirect decision core (`vfs-redirect`)

Given an NT path (e.g. `\??\C:\GameLayers\runtime\Data\Skyrim.esm`) and the snapshot:

1. Is the path under the **managed root**?
2. Overlay first (if configured): present → redirect to overlay file; whiteout → deny.
3. Else snapshot resolve:
   - Disk source → `Decision::Redirect { target_nt }`
   - Zip-window → `Decision::Serve { container_nt, offset, length }`
   - Missing → pass-through or deny depending on policy
4. Directory opens merge real OS children + virtual children + whiteouts.

Also pure helpers for:

- Attribute queries (`NtQueryAttributesFile` family)
- Directory info marshaling (`FILE_FULL_DIR_INFORMATION`, …)
- Write classification (create disposition + access → copy-on-write to overlay)

The shim’s `Engine` wraps `RootMap` + snapshot + optional `Overlay`.

---

## 6. Shim: hooks and zip serve

### 6.1 What gets hooked

| API | Purpose |
|-----|---------|
| `NtCreateFile` / `NtOpenFile` | Path virtualization: redirect / serve / deny |
| `NtQueryAttributesFile` / `NtQueryFullAttributesFile` | Fake size/mtime for zip files |
| `NtQueryDirectoryFile(Ex)` | Merged directory listings under root |
| `NtReadFile` | Synth zip windows → memcpy from mapped container |
| `NtQueryInformationFile` / `NtSetInformationFile` | Synth size/position; rename/delete → overlay |
| `NtClose` | Drop synth handles |
| `NtCreateSection` / `NtMapViewOfSection` / `NtUnmapViewOfSection` | Data sections over synth files; **SEC_IMAGE** special-case for zip PE |
| `CreateProcessInternalW` | Child process: inject + hollow virtual PE |
| `GetModuleFileName(W/A)`, related | Spoof paths toward GameLayers (GMF) |

Early path/attr stubs can be owned by **vfs-payload** (pre-init inline patches); full shim attaches remaining detours post-loader via `retour`.

### 6.2 Decision path in hooks

On create/open:

```
decision_for(OA, access, disposition)
  → PassThrough  → real ntdll trampoline
  → Redirect     → rewrite ObjectAttributes path, trampoline
  → Serve        → zipserve::open_synth(container, offset, length) → synthetic handle
  → Deny         → STATUS_OBJECT_NAME_NOT_FOUND
```

### 6.3 Synthetic handles (`zipserve`)

- **Container map cache:** first Serve for a zip opens it read-only, `CreateFileMapping` + `MapViewOfFile` whole file (pages fault in lazily). Cached by container NT/Win32 path.
- **Synth file handle:** tagged high bit (`2^46`) so real kernel handles never collide. Stores window base, length, file position.
- **NtReadFile:** if synth → copy `[position, position+len)` from mapped window; advance position.
- **Query info:** fake `FileStandardInformation`, `FileBasicInformation`, stable fake index, etc.
- **Synth sections:** data sections over zip windows; MapView returns pointer into the already-mapped zip (unmap is bookkeeping-only).

### 6.4 SEC_IMAGE on zip windows

Windows will not create a true image section on a non-file or fake handle. The shim intercepts `NtCreateSection(…, SEC_IMAGE)` on a synth file:

1. Read the zip-window PE bytes into a buffer.
2. `map_image_from_pe_bytes_local` (manual map in the **current** process).
3. Register a synthetic section whose `MapView` returns that base.

This is how LoadLibrary-like paths through VFS **can** get PE images without staging a PE file—**when** the open goes through synth handles. Static imports of a Steam-hosted EXE still use the real Steam disk files unless rewritten later (see §9).

---

## 7. Dual-layer inject and pre-init

### 7.1 The problem

| Approach | Static EXE imports | Full Engine |
|----------|--------------------|-------------|
| LoadLibrary shim after resume | Too late | Yes |
| Pre-init payload only | Yes (redirect table) | No |
| Naïve both | Race + **double-patch** of same ntdll stubs | Broken |

### 7.2 Ownership model

- **Early (`vfs-payload`):** permanently owns 4 stubs via 14-byte absolute jumps:
  - `NtOpenFile`, `NtCreateFile`, `NtQueryAttributesFile`, `NtQueryFullAttributesFile`
- Early body: suffix redirect table **or** secondary dispatch into full Engine once the full shim is live.
- **Full (`vfs-shim`):** never rewrites those 4 prologues; installs dir enum, close, QIF, setinfo, CPIW via `retour` after kernel32 is present.

### 7.3 Handoff sequence (`run_target_with_shim`)

1. `CreateProcess` target (often suspended or gated).
2. Arm **pre-init payload** (config file / shared page with redirect entries for known static-import DLLs).
3. Inject full shim DLL.
4. Wait for ready event (`Local\vfs_shim_ready_{pid}` or path-based ready file).
5. Dual-layer: OEP late-entry stub may call `vfs_shim_sync_bootstrap` so hooks are live before EXE main.
6. Optional: **hollow** primary PE from zip RAM into the process.
7. Resume / wait as configured.

`VFS_DUAL_LAYER` changes `DllMain` behavior (no async bootstrap race; sync install from late-entry).

---

## 8. Process hollowing (“ghostly”) for EXEs

### 8.1 Why hollow

- Managed root has **no** `skse64_loader.exe` / `SkyrimSE.exe` files on disk (by design).
- Windows `CreateProcess` requires a real file for the initial image.
- Steam DRM checks **ProcessImageFileName** / authenticity of the host binary: hollowed `cmd.exe` → Steam error `3:0000065558`.
- **Fix:** CreateProcess the **real Steam** `SkyrimSE.exe` (or a real system host for non-DRM tools), then **overwrite** the process image with zip PE bytes via `WriteProcessMemory` only—no archive extract.

### 8.2 Hollow algorithm (simplified)

1. Pick host via `hollow_host_exe_for(image_path)`:
   - Prefer Steam `…\Skyrim Special Edition\SkyrimSE.exe` when target looks like Skyrim.
   - Never use a path under `GameLayers\runtime` (may be VFS-spoofed / empty).
2. `CreateProcessW(host, cmdline_with_virtual_path, CREATE_SUSPENDED)`.
3. Optionally inject shim **before** hollow so VFS hooks exist during import preload.
4. Query PEB ImageBase; compare host `SizeOfImage` to zip PE.
5. **Preload remote imports** (system DLLs via remote `LoadLibraryA`; game-locals via Stage A—see §9).
6. Prefer **in-place** overwrite when `SizeOfImage` matches (SkyrimSE Steam host matches zip SE)—preserves one main module base for SKSE.
   - Else `VirtualAllocEx` + unmap optional + write new base; fix PEB ImageBase; apply relocs.
7. `resolve_imports_ex_with_bases` against remote process + forced game-local bases.
8. Write full flat PE image; set thread context entry (RCX/RIP as needed); TLS/cookie/unwind helpers for MSVC CRT where required.
9. **Finalize game-local modules** (Stages B–D).
10. Spoof PEB/LDR/GMF paths toward `C:\GameLayers\runtime\…`.

Primary EXE provenance is always logged as:

```text
vfs-inject: wrote N zip PE bytes to 0x… (source=archive RAM)
```

### 8.3 Child processes (CPIW)

When the game or SKSE calls `CreateProcess` for a virtual path under the root:

1. Shim’s CPIW hook sees the virtual ApplicationName.
2. May CreateProcess a real host + inject + hollow zip PE for that child.
3. Result: SKSE loader can “launch” `SkyrimSE.exe` at a GameLayers path while the kernel host remains DRM-safe Steam image hollowed with zip content.

---

## 9. Game-local DLLs: the four-stage pipeline

Game-local imports of the main EXE (`steam_api64.dll`, `bink2w64.dll`, …) are the hardest part of “everything from zips.”

### 9.1 Why not “just LoadLibrary GameLayers\steam_api64.dll”

- Root has **no** file on disk → LoadLibrary fails unless VFS SEC_IMAGE path works end-to-end.
- Steam host’s static imports already map **Steam-disk SEC_IMAGE** modules with **DllMain already run**.
- FreeLibrary / NtUnmap of those modules is unreliable (static pin, ghost LDR).
- Manual-map to a **new** base and point IAT there → titled Skyrim window dies (DllMain globals, DRM session tied to original HMODULE).
- Full WPM of zip layout **including writable sections** after DllMain → destroys initialized data → window dies.
- “Privatize” (unmap Steam SEC_IMAGE, re-alloc same base, **copy live pages**) preserves DllMain but **does not** put zip PE provenance on code pages—skeptics correctly rejected it as re-labeling Steam content.

### 9.2 Working pipeline

**Stage A — Bootstrap HMODULE (preload, before main-image IAT resolve)**

- Never bare-`LoadLibrary("steam_api64.dll")` as the content strategy.
- If already mapped (Steam host): record base; log Stage A + zip source path for later WPM.
- Else: try LoadLibrary full GameLayers path (VFS), else **manual-map zip PE** + optional DllMain.
- Push `(name, base)` into `forced_bases` for main-image import resolve.

**Stage B — Zip materialize (`overwrite_remote_module_zip_preserve_iat`)**

After main EXE zip PE is written:

1. `pe_layout(zip)` → flat image; apply relocs to host base.
2. Copy **remote IAT** (and FirstThunk chains) into the image (loader-resolved addresses stay valid).
3. For each **writable** section: copy **from remote** into image (preserve DllMain-dirtied data).
4. Re-copy IAT after writable restore.
5. `VirtualProtectEx` + **WPM entire image** → non-writable sections are now **exactly zip pe_layout**.
6. Log: `wrote N zip PE bytes to 0x… for steam_api64.dll (IAT+writable preserved, non-writable from zip)`.

**Stage C — Identity proof (`remote_image_matches_zip_layout`)**

- Rebuild expected layout + reloc + IAT mask from remote.
- Compare **headers** (ImageBase field masked) and **every non-writable section** byte-for-byte.
- Require ≥ 0x1000 bytes compared.
- **Must fail** if a single byte at section VA `+0x1000` is flipped (unit-tested).
- Log: `zip PE identity OK … nonwritable_fnv=… source=zip-window:…zip!steam_api64.dll`.

**Stage D — LDR path spoof**

- Rewrite PEB LDR `FullDllName` / `BaseDllName` to `C:\GameLayers\runtime\<dll>`.
- `EnumProcessModules` / `GetModuleFileNameEx` report GameLayers, not `steamapps`.

### 9.3 Unit test that gates the path

`game_local_zip_overwrite_preserves_iat_and_identity`:

1. Suspended Steam SkyrimSE host.
2. Remote LoadLibrary Steam `steam_api64.dll` (Stage A).
3. Stage B zip overwrite from layer zip window.
4. Stage C identity must pass; IAT first slot unchanged.
5. Flip one byte at `base+0x1000` → Stage C must fail.

This fails on privatize-only / header-only “proof” paths.

---

## 10. End-to-end launch path (`vfs-launch`)

```text
vfs-launch [--layers C:\GameLayers] [--wait] [--probe]
```

1. Discover numbered zips `1.…`, `2.…`, …
2. `read_layer` each → `prepare_layer` creates **dirs only** under `runtime\`.
3. Wipe any leftover payload files under root.
4. Build merged tree + snapshot; write shim config under `vfs-state`.
5. Enable plugins in AppData `Plugins.txt` (masters + SkyUI if present).
6. Set `SteamAppId` / `SteamGameId` = `489830`.
7. Locate `vfs_shim_dll.dll` + `vfs_payload.dll` near the launcher.
8. Read zip PE bytes for `skse64_loader.exe` (or `SkyrimSE.exe` with `--se`).
9. `run_target_with_shim` with `target_pe_bytes` = zip loader PE, virtual image path under `runtime\`.
10. SKSE (hollowed) CPIW-launches virtual SkyrimSE → Steam host + hollow zip SE + Stages A–D for game-locals.
11. Game runs: `Data/` opens are **Serve** from zip windows; no BSA/ESP on disk under root.

Probe mode (`--probe` / `vfs-game-probe`) validates sizes/magic for key paths without GPU game loop.

---

## 11. What content is where at runtime

| Artifact | Origin | On disk under runtime? |
|----------|--------|-------------------------|
| `skse64_loader.exe` image | Zip RAM hollow into host | No |
| `SkyrimSE.exe` image | Zip RAM hollow into **Steam host** | No (host file is Steam install) |
| `steam_api64.dll` code/const | Zip WPM Stage B after Steam DllMain | No |
| `bink2w64.dll` code/const | Same | No |
| SKSE runtime DLL | Loaded via SKSE/VFS paths (GameLayers LDR) | No payload file required |
| `Data/*.bsa`, `*.esm`, SkyUI | Zip-window Serve / NtReadFile | No |
| Empty `Data/`, `Scripts/`, … | mkdir skeleton | Dirs only |
| Overlay writes | `C:\GameLayers\overlay` | Yes (user writes) |
| Layer zips | `C:\GameLayers\*.zip` | Yes (containers only) |

---

## 12. Hard lessons (empirical)

### 12.1 Steam DRM is host-path sensitive

Hollow into `cmd.exe` / random system EXEs → Steam fails early. Prefer real Steam `SkyrimSE.exe` as CreateProcess ApplicationName while cmdline and PEB still advertise GameLayers virtual paths.

### 12.2 CreateRemoteThread exit codes are 32-bit

Remote `LoadLibrary` return via thread exit code truncates 64-bit HMODULEs (`0x7ff8…` → low 32 bits). Always re-resolve bases with `EnumProcessModulesEx`.

### 12.3 IAT is sacred after loader bind

Any zip PE write over a live module must **preserve the remote IAT** (directory + FirstThunks). Zeroing IAT from a fresh `pe_layout` kills imports immediately.

### 12.4 DllMain state lives in writable sections

Code can come from zip; **`.data` / writable** may be dirtied after `DLL_PROCESS_ATTACH`. Overwriting writable with pristine zip zeros breaks Steam API session / bink state. Preserve writable; prove identity on **non-writable** sections.

### 12.5 Same HMODULE, different content strategy

Clone-to-new-base + IAT retarget breaks the titled window. Same-base overwrite (Stage B) keeps HMODULE stable for DRM and for anything that stashed the module handle.

### 12.6 Path spoof ≠ provenance

LDR / GMF strings saying `C:\GameLayers\…` prove **enumeration**, not **byte origin**. Identity must RPM-compare remote pages to `pe_layout(zip)`. Header-only MZ/PE checks are dishonest.

### 12.7 “Privatize” is not enough for “from zips”

Unmap SEC_IMAGE + private re-map of **live** pages drops the Steam file object but content is still “whatever was live,” not necessarily a deliberate zip materialize. Stage B WPM from zip is the content path.

### 12.8 CREATE_SUSPENDED does not guarantee static imports mapped

On some hosts/builds, Steam SkyrimSE suspended has no `steam_api` yet. Stage A must LoadLibrary when missing.

### 12.9 Preload order vs hollow

Inject shim **before** hollow when possible so remote LoadLibrary of game-locals can hit VFS. Finalize zip-overwrite **after** main-image IAT is bound to Stage A bases.

### 12.10 SKSE wants one coherent main module

In-place hollow (matching SizeOfImage) keeps SKSE’s image expectations happier than dual bases / unmap host. Logs “outside of loader control” appear when image identity/handshake is wrong—not only when inject fails.

---

## 13. Verification bar (what “done” meant for the goal)

Acceptance-style checks that were used repeatedly:

1. **Dual consecutive launches:** titled window `Skyrim Special Edition` ≥ ~30s; SKSE log `init complete`; SkyUI referenced in SKSE log.
2. **Zero extract:** managed root recursive file count for archive payloads = 0; no new `vfs-run-*` / `vfs-sse-*` / `vfs-sec-*` TEMP staging.
3. **Module paths:** `EnumProcessModules` shows `steam_api64`, `bink2w64`, `skse*`, `SkyrimSE` under `C:\GameLayers\runtime\…`, not `steamapps`.
4. **Zip provenance logs:** `wrote N zip PE bytes` for main EXEs and game-local DLLs; `zip PE identity OK … source=zip-window:…`.
5. **In-repo tests:** `cargo test -p vfs-inject --lib` including Stage B/C identity test; zip-serve / hollow no-stage tests.

Scratch evidence typically under:

`%LOCALAPPDATA%\Temp\grok-goal-…\implementer\`  
(`dual-launch-transcript.txt`, `launch-1.log`, `module-paths.txt`, `no-temp-extract.txt`, `cargo-zip.txt`, `skse-runtime.log`).

---

## 14. IPC / server path (architecture beyond the game launch)

Though the Skyrim path is largely **in-process snapshot + shim**, the workspace also has:

- **vfs-win:** section-backed shared memory.
- **vfs-ipc:** lock-free control ring (no OS file APIs inside the ring code—G11 constraint for recursion safety).
- **vfs-server:** authoritative process builds/publishes snapshot and answers IPC.

These support multi-process directors and future tools without baking zip parsers into every client.

---

## 15. Security / threat framing (explicit non-goals)

This stack is **modding infrastructure** (USVFS/MO2 successor thinking), not a red-team kit:

- Process hollowing and PEB spoofing exist to satisfy **Windows loader + Steam DRM + empty install root**, not to hide malware.
- No kernel drivers.
- No claim of anti-cheat bypass as a product goal.
- Stored-zip-only; no general archive unpacker for arbitrary malware samples.

---

## 16. Open edges and future work

| Topic | Notes |
|-------|--------|
| Deflated zip entries | Out of scope; would need decompress cache (conflicts with zero-extract purity). |
| True VFS LoadLibrary for game-locals from cold start | Would need early Ldr hooks before Steam static imports; Steam DRM still wants a real host EXE. |
| Writable-section provenance | Intentionally left as DllMain live pages; only non-writable proven vs zip. |
| Full `SEC_IMAGE` kernel semantics | Synth SEC_IMAGE is manual-map, not a real section object; some APIs may still distinguish. |
| Child dual-layer | Same handoff as parent; document/complete parity. |
| Performance | Whole-file map of 16 GB zip relies on OS demand paging; fine for reads, watch working set. |
| Extract `game_local_dll.rs` | Strategist recommendation: keep Stage A–D in one module for maintainability. |

---

## 17. Mental model (one page)

```
                    ┌─────────────────────────────┐
                    │  C:\GameLayers\*.zip (Stored)│
                    └──────────────┬──────────────┘
                                   │ vfs-zip → layers
                                   ▼
                    ┌─────────────────────────────┐
                    │  vfs-core tree + vfs-shared  │
                    │  snapshot (no PE on root)    │
                    └──────────────┬──────────────┘
                                   │ config to shim
         CreateProcess(Steam host) │
                    ┌──────────────▼──────────────┐
                    │  Game process               │
                    │  ┌─ early payload (4 stubs) │
                    │  ├─ full shim + Engine      │
                    │  ├─ hollow: zip PE → main   │
                    │  ├─ Stage A–D game-locals   │
                    │  └─ Data/ reads → zipserve  │
                    └─────────────────────────────┘
                                   │
                    User sees GameLayers paths;
                    kernel may still have Steam host file
                    for DRM; live code/const from zip WPM;
                    assets from zip windows.
```

**One sentence:**  
VFS is a pure layered FS core plus an injected Windows shim that **serves Stored zip windows as files** and **materializes executables in memory** (hollow + zip-overwrite DLLs), so a full SKSE Skyrim stack runs from three GameLayers archives with an empty managed root.

---

## 18. Key file index

| Path | Why it matters |
|------|----------------|
| `crates/vfs-launch/src/main.rs` | Director / launch entry |
| `crates/vfs-zip/src/lib.rs` | ZIP64 Stored → windows |
| `crates/vfs-core/src/source.rs` | Disk vs zip-window encoding |
| `crates/vfs-redirect/src/lib.rs` | Decisions, dir merge, whiteouts |
| `crates/vfs-shim/src/engine.rs` | Overlay + snapshot engine |
| `crates/vfs-shim/src/hook.rs` | All NT hooks, CPIW hollow |
| `crates/vfs-shim/src/zipserve.rs` | Synth handles + maps |
| `crates/vfs-inject/src/ghostly.rs` | Hollow, preload, Stage B–D |
| `crates/vfs-inject/src/map.rs` | pe_layout, relocs, imports |
| `crates/vfs-inject/src/inject.rs` | Dual-layer / pre-init arm |
| `crates/vfs-payload/src/lib.rs` | Early 4-stub patches |
| `docs/superpowers/specs/*` | Design history (zip layers, dual-layer, hooks, …) |

---

## 19. Glossary

| Term | Meaning |
|------|---------|
| **Stored** | Zip compression method 0; raw bytes contiguous in archive |
| **Zip window** | `(container_path, offset, length)` into a zip file |
| **Serve** | Decision: open returns synthetic handle over a window |
| **Redirect** | Decision: rewrite path to another real file (disk/overlay) |
| **Snapshot** | Flattened published VFS image for the shim |
| **Hollow** | CreateProcess real host; WPM different PE into process |
| **Stage A–D** | Game-local DLL bootstrap → zip WPM → identity → LDR spoof |
| **Forced bases** | Remote HMODULEs for game-locals used when resolving main IAT |
| **GMF** | GetModuleFileName spoof toward virtual root |
| **CPIW** | CreateProcessInternalW hook for child hollow |
| **Whiteout** | Overlay tombstone hiding a lower-layer name |

---

*Document generated from the `C:\oss\vfs` codebase, design specs under `docs/superpowers/`, and the Skyrim zip-launch iteration work (hollow, dual-layer inject, game-local Stage B/C identity). Reflects the shipping approach as of the zip-overwrite + dual-launch green path.*

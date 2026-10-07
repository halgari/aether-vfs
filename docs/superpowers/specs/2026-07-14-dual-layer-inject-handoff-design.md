# Dual-Layer Inject Handoff — Design Spec

**Status:** Ready for planning / implementation.
**Date:** 2026-07-14
**Type:** Production design (modding infrastructure — USVFS/MO2 lineage).
**Depends on:** [Pre-init injection](2026-07-14-preinit-injection-design.md)
(`vfs-payload`, `arm_preinit_payload`, static-import proof).
**Parent goal:** Every director-launched target gets pre-init hooks (EXE static
imports virtualized) **and** the full Engine (snapshot, overlay, dir enum, …)
without double-patching ntdll stubs.

---

## 1. Problem

Two layers exist today on **separate APIs**:

| API | When hooks install | Static EXE imports | Full VFS Engine |
| --- | --- | --- | --- |
| `run_target_with_preinit` | Before `LdrpInitializeProcess` | Yes (redirect table) | No |
| `run_target_with_shim` | After LoadLibrary wakes loader | **No** (too late) | Yes |

Combining them naively fails in two ways:

1. **Timing:** `CreateRemoteThread(LoadLibraryW)` on a suspended process runs
   process init on the remote thread before our code — undoes the pre-init win
   if used *instead of* RIP-redirect. If used *after* resume without a gate,
   it races the EXE entry point.
2. **Double-patch:** both layers today would install on the same four ntdll
   stubs (`NtOpenFile`, `NtCreateFile`, `NtQueryAttributesFile`,
   `NtQueryFullAttributesFile`). Second patcher sees a 14-byte abs-jmp prologue
   and corrupts the trampoline chain.

---

## 2. Goals

1. **Single director entry** (`run_target_with_shim`) always arms the early
   payload first, then brings up the full shim, then runs the target body.
2. **Early layer owns the four path/attr stubs permanently** — only one
   inline-patch owner.
3. **Full VFS behavior** (RootMap + snapshot + overlay + open/attr decisions)
   is reachable through those four hooks after the full shim is live.
4. **Remaining detours** (dir enum, close, qif, setinfo, CPIW) stay on the
   full shim via `retour`, installed only after process init (kernel32 present).
5. **In-process tests** (`install(engine)` with no early layer) keep working
   unchanged: full set of detours, including the four.
6. **No disk writes** into the game/app directory (zero-footprint constraint).
7. Existing acceptance suite stays green under the unified path.

### Non-goals (this slice)

- Porting Engine / snapshot into `no_std` inside the payload.
- Child-process dual-layer cutover (document only; same handoff later).
- Growing the early redirect table into a full RootMap (can land redirects for
  known static DLLs as data; Engine still owns general VFS paths via secondary).
- Spike B / instrumentation callback.

---

## 3. Ownership model

```
                    ntdll stubs
         ┌─────────────────────────────────────┐
         │  NtOpenFile / NtCreateFile          │  ◄── ONLY early payload patches
         │  NtQueryAttributesFile / QFull      │      (inline 14-byte abs jmp)
         └──────────────┬──────────────────────┘
                        │
              early hook body
                        │
         ┌──────────────┼──────────────────────┐
         │              │                      │
    redirect table  secondary ≠ 0          else
    (suffix match)  (full shim live)     trampoline
         │              │                   (original)
         ▼              ▼
    rewrite OA     full create/open/
    → trampoline   qattr/qfull logic
                   (Engine)
```

| Stub | Owner | Mechanism |
| --- | --- | --- |
| NtOpenFile, NtCreateFile, NtQueryAttributesFile, NtQueryFullAttributesFile | **Early** (`vfs-payload`) | Inline patch at pre-init |
| NtQueryDirectoryFileEx, NtClose, NtQueryInformationFile, NtSetInformationFile | **Full** (`vfs-shim`) | `retour::RawDetour` post-init |
| CreateProcessInternalW | **Full** | `retour` post-init (best-effort) |

**Invariant:** after dual-layer arm, the four early stubs' first 14 bytes are
never rewritten by the full shim.

---

## 4. Secondary dispatch (how Engine rides on early hooks)

### 4.1 Why not “full shim patches the four”

`retour` and the early abs-jmp both assume a standard syscall stub prologue.
Once early has patched, the prologue is no longer stealsafe. Disarm-then-rearm
is possible but racy during the loader/EXE window and easy to get wrong.
Permanent early ownership + secondary is simpler and keeps pre-init continuous.

### 4.2 Payload Config extension

Extend the existing `#[repr(C)]` Config (must stay shared with injector):

```text
// existing fields …
// NEW (all initially 0):
secondary_open:    usize   // NtOpenFileFn
secondary_create:  usize   // NtCreateFileFn
secondary_qattr:   usize   // NtQueryAttributesFileFn
secondary_qfull:   usize   // NtQueryFullAttributesFileFn
```

Early hook bodies (conceptually):

```text
fn open_hook(...):
  bump(open)
  if let Some(e) = match_redirect(oa):
    return redirect_via_trampoline(e, ...)
  if secondary_open != 0:
    return secondary_open(...)   // full shim's open_hook
  return trampoline(...)         // original ntdll
```

Same pattern for create / qattr / qfull.

**Ordering:** redirect table is checked **before** secondary so static-import
entries remain effective even after Engine is live (and so a backing path
outside the RootMap still works for the fixture DLL case).

### 4.3 Publishing secondary (full shim → payload)

The early image is **reflectively mapped** — not in the PEB module list — so
`GetProcAddress(payload)` is unavailable.

Contract:

1. Injector places Config in the target and records its address `cfg_remote`.
2. Injector sets inherited env **`VFS_PAYLOAD_CFG=<hex cfg_remote>`** (and
   existing `VFS_SHIM_CONFIG` / `VFS_SHIM_READY`).
3. Full shim bootstrap, after `ENGINE.set` and **before** enabling any
   remaining detours, writes the four secondary function pointers into
   `cfg_remote` (same process after LoadLibrary — pointer is valid).
4. Use `Release` stores (or volatile writes) so early hooks on other threads
   observe a consistent non-zero pointer after publish.

Optional hardening: a single `secondary_ready: u32` flag set last, after all
four pointers are written.

### 4.4 In-process / no-early path

When there is no early layer (`VFS_PAYLOAD_CFG` unset):

- `install(engine)` behaves as today: detours **all** stubs including the four.
- Secondary fields unused.
- All existing `vfs-shim` integration tests stay on this path.

When dual-layer:

- `install_late(engine)` (name TBD): set ENGINE, publish secondary, detour
  **only** the non-early set.

Bootstrap chooses: if `VFS_PAYLOAD_CFG` present → `install_late`, else →
`install`.

---

## 5. Director timeline (unified `run_target_with_shim`)

**Production sequence uses a post-install spin gate** (not OEP patching). An
OEP late-entry prototype remains in `vfs-inject::oep_gate` for experiments;
LoadLibrary-at-OEP proved fragile (AV) on this host.

### 5.1 Sequence (spin gate)

```
1. Set env: VFS_SHIM_CONFIG, VFS_SHIM_READY, VFS_DUAL_LAYER=1,
            VFS_PAYLOAD_CFG_FILE=<path>  (file written after arm)
2. CreateProcessW(CREATE_SUSPENDED)
3. arm_preinit_payload_ex(..., with_release_gate=true)
     - reflective map, Config, RIP → stub
     - stub: shim_install → spin until *release_flag != 0 → jmp orig RIP
4. Write hex(cfg_remote) to VFS_PAYLOAD_CFG_FILE
5. ResumeThread(primary)
6. Primary: stub runs shim_install (early hooks live), then spins
7. Injector polls counters sentinel 0xC0DE
8. CreateRemoteThread(LoadLibraryW(vfs-shim-dll))
     - remote thread drives process init WITH early hooks live
     - DllMain spawns bootstrap → install_late(cfg) if cfg usable
9. Injector polls VFS_SHIM_READY
10. Injector writes release_flag=1 → primary leaves spin → RtlUserThreadStart
11. Main runs with full VFS + static-import redirects
```

### 5.2 Why spin (not OEP)

- Early hooks must install **before** any thread runs `LdrpInitializeProcess`.
- `CreateRemoteThread(LoadLibrary)` while the primary is still in the stub
  (post-install, pre-`RtlUserThreadStart`) is safe: the remote thread performs
  process init under early hooks; static imports virtualize.
- Releasing the spin only after full-shim ready avoids racing EXE main.
- Child processes inherit `VFS_PAYLOAD_CFG_FILE` but the address is foreign —
  bootstrap **validates** cfg (VirtualQuery + `nt_protect` == local ntdll
  export) and falls back to full `install()` when invalid.

### 5.3 Bootstrap dual-layer selection

| Condition | Install path |
| --- | --- |
| Usable `payload_cfg` (arg or validated cfg file) | `install_late` |
| Otherwise | full `install` (in-process tests, children) |

---

## 6. Redirect table vs Engine (what each handles)

| Open kind | Handler |
| --- | --- |
| Static-import / suffix match in early table (e.g. `d3d11.dll`, fixture `vproxy.dll`) | Early redirect table → trampoline |
| Virtual path under RootMap (mod files, overlays, tombstones) | Secondary → Engine |
| Everything else | Trampoline (passthrough) |

Director fills the early table from:

- Explicit list on `RunConfig` (static-import DLLs the game needs), and/or
- Future: derived from snapshot entries that are PE images named like DLLs
  (out of scope for this slice — table remains caller-supplied).

Backing paths for the table stay **outside** the managed root (existing
self-redirect footgun).

---

## 7. API surface (crates)

### `vfs-payload`

- Config + secondary fields; hook order: table → secondary → trampoline.
- Keep `shim_install`; no secondary publish helper required in payload (full
  shim writes Config memory directly).

### `vfs-shim`

- `install(engine)` — **full** detour set (in-process / no early). Unchanged
  semantics for tests.
- `install_late(engine, payload_cfg: *mut PayloadConfig)` — ENGINE + secondary
  publish + detours **excluding** the four early stubs.
- `bootstrap_from_config_path` branches on dual-layer.
- `vfs-shim-dll`: export `vfs_shim_sync_bootstrap`; DllMain spawn gated.

### `vfs-inject`

- `RunConfig` gains: `payload_path`, `preinit_redirects: Vec<PreinitRedirect>`
  (may be empty).
- `run_target_with_shim`:
  1. Create suspended  
  2. `arm_preinit_payload`  
  3. Write late-entry gate (dll path = existing `dll_path`)  
  4. Resume; wait process (and/or ready)  
- `run_target_with_preinit` remains for payload-only tests.
- Modules: extend with `oep_gate.rs` (PEB/OEP parse, late stub).

### Tests

| Test | Expectation |
| --- | --- |
| `static_import` | Still passes (preinit-only path OK) |
| `end_to_end` / `acceptance` | Use unified `run_target_with_shim` with payload path set; still green |
| New: dual-layer static + virtual | EXE static import of vproxy **and** a virtual data file redirect via Engine in one run |
| `vfs-shim` hook_* tests | Unchanged (`install` full) |

---

## 8. Failure modes & diagnostics

| Symptom | Likely cause |
| --- | --- |
| Static import fails, virtual files work | Early not armed / redirect table empty / OEP gate skipped |
| Static import works, virtual files fail | Secondary not published / `install_late` used wrong / ENGINE missing |
| Crash in ntdll open | Double-patch or secondary points at wrong ABI |
| Hang before main | Sync bootstrap deadlock; DllMain spawn + sync both running |
| Exit before ready | Late-entry LoadLibrary failed (path wrong) |

Counters page (early) remains useful: matched redirects, secondary hit count
(optional new index).

---

## 9. Child processes (landed)

`CreateProcessInternalW` uses the same **spin-gate dual-layer** as the director
(`vfs_shim::inject::inject_child` → `inject_child_dual_layer`):

1. Force-suspend child (existing).
2. `arm_preinit_payload_ex(..., with_release_gate=true)` using `vfs_payload.dll`
   next to the shim DLL (or `VFS_PAYLOAD_PATH`).
3. Write `%TEMP%\vfs_payload_cfg_<child_pid>.txt` with hex `cfg_remote`.
4. Resume → early install + spin; LoadLibrary full shim; wait ready; release.
5. If caller asked for `CREATE_SUSPENDED`, re-suspend after inject.

Bootstrap resolves cfg via env file **or** the per-PID temp file, with
`payload_cfg_usable` validation. Fallback: classic LoadLibrary-only if payload
is missing or arm fails.

Child early redirect table is loaded from the same config static-import
section (`VFS1` in `VFS_SHIM_CONFIG`) as the director — so child EXEs get the
same d3d/dxgi-style pre-init redirects when the parent inherits that env.

---

## 10. Success criteria

1. `run_target_with_shim` with a non-empty payload path **always** arms early
   before any full-shim code runs in the target.
2. Automated test: one process gets **both** a static-import redirect (early
   table) and a snapshot virtual file redirect (Engine via secondary).
3. Full acceptance suite green on the unified path.
4. In-process `vfs-shim` tests green without early payload.
5. `dumpbin /imports` on `vfs_payload.dll` still empty.
6. No proxy DLL written into the target app directory.

---

## 11. Framing

Game modding / usermode VFS. Vocabulary: shim, injector, redirect, early
payload, late-entry — not security-research framing.

*End of design.*

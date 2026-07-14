# Pre-init Injection — Reflective-map + RIP-redirect

**Status:** Production design (not a spike).
**Date:** 2026-07-14
**Context:** Virtualize a game EXE's own static PE imports (d3d/dxgi-style)
with zero files written into the game directory. Spike B (instrumentation
callback) is shelved. Disk side-loading is disqualified.

---

## 1. Problem

`CreateRemoteThread(LoadLibraryW)` on a `CREATE_SUSPENDED` process wakes the
loader: process init (including binding the EXE's own IAT) runs on that
remote thread before our DllMain/bootstrap can install hooks. The EXE's
static imports are therefore never virtualized under the classic inject path.

Thread-context redirect never starts a loader-serviced thread, so nothing
forces that init before our install runs.

---

## 2. Vehicle

```
CreateProcessW(CREATE_SUSPENDED)
VirtualAllocEx + WriteProcessMemory:
  - flat PE image of vfs-payload (sections + BASE_RELOC; zero imports)
  - trampolines, counters, Config, backing path strings
  - PIC stub
GetThreadContext(primary) → orig RIP (RtlUserThreadStart)
SetThreadContext(primary, RIP = stub)   // CONTEXT 16-byte aligned
ResumeThread(primary)
stub: sentinel → shim_install(Config) → jmp orig RIP
LdrpInitializeProcess runs with hooks live → static imports virtualized
```

No `LoadLibrary` of the early payload. Addresses for ntdll stubs and
`NtProtectVirtualMemory` are resolved in the injector and passed in Config
(ntdll shares its base across the session).

---

## 3. Two layers

| Layer | Role |
| --- | --- |
| **vfs-payload** (early) | `no_std`, zero-import cdylib. Owns NtOpenFile / NtCreateFile / NtQueryAttributesFile / NtQueryFullAttributesFile via inline patches. Redirect table in Config (path suffix → backing NT path). |
| **vfs-shim** (full) | Existing std Engine (snapshot, overlay, dir enum, …). Loaded post-init when needed for features beyond the early redirect table. Must **not** re-patch the four early stubs while early owns them. |

**Unified director path (follow-on):** early payload owns the four path/attr
stubs permanently; full shim publishes **secondary** handlers into the early
Config and installs only the remaining detours; an OEP late-entry gate
LoadLibrary's the full shim after loader init and before EXE main. See
[Dual-layer inject handoff](2026-07-14-dual-layer-inject-handoff-design.md).

Until that handoff lands:

- Static-import targets use `run_target_with_preinit` (proven).
- Full VFS acceptance uses legacy LoadLibrary-only `run_target_with_shim`.

Child processes **should** use the same reflective-map + RIP vehicle for
suspended creates (`CreateProcessInternalW` force-suspend). **Current
status:** the director API (`vfs_inject::run_target_with_preinit`) is
landed and proven; child inject in `vfs-shim` still uses LoadLibrary so it
does not double-patch the four early-owned NT stubs when the full std shim
also installs. Child cutover lands when either (a) the early payload carries
enough redirect/Engine state for children, or (b) the full shim installs only
the remaining detours and leaves open/create/qattr/qfull to the early layer.

---

## 4. Config ABI (sketch)

`#[repr(C)]` blob written into the target. Payload imports nothing.

- `nt_protect` — `NtProtectVirtualMemory`
- per-hook: `*_target` (ntdll stub), `*_tramp` (RWX buffer ≥32 B)
- `install_mask` — bit0 qfull, bit1 qattr, bit2 open, bit3 create
- fixed redirect table: N entries of
  `{ suffix_ptr, suffix_wlen, backing_ptr, backing_wlen, backing_size }`
  (UTF-16, lengths exclude NUL; paths absolute `\??\...` for backing)
- `counters` — optional `*mut u32` diagnostics page

Match rule: object name ends with suffix (case-insensitive), preceded by
`\`/`/` or start of string. Backing files live **outside** any virtual root
so cleanup/self opens never redirect onto the backing.

---

## 5. Success criteria

- `vfs_payload.dll` has an empty import table (`dumpbin /imports`).
- Fixture EXE with a static import of a DLL that is **absent** from its
  app directory still loads that import via redirect to an off-root backing
  and observes the backing export value.
- Stub sentinel proves pre-init code ran before orig RIP.
- No proxy DLL written into the game/app directory.

---

## 6. Non-goals (this slice)

- Full no_std port of vfs-core / snapshot / overlay into the payload.
- x86 / WOW64.
- Reviving instrumentation-callback (Spike B).
- Security-research framing — this is game-modding infrastructure
  (USVFS/MO2 lineage).

---

## 7. Crates

| Crate | Responsibility |
| --- | --- |
| `vfs-payload` | Zero-import early hooks + `shim_install` |
| `vfs-inject` | PE flatten/reloc, PIC stub, SetThreadContext vehicle, director API |
| `vfs-shim` | Child-process force-suspend + same vehicle; full Engine post-init |

*End of design.*

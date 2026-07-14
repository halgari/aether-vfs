# Spike B — Instrumentation-Callback Timing — Design Spec

**Status:** Approved-to-proceed. **Type:** throwaway spike, scratchpad-only, not
a workspace crate.
**Date:** 2026-07-13
**Context:** [[vfs-nostd-payload-recipe]] left two candidate pre-init vehicles
for running `shim_install` before the target's own static PE imports (the
game EXE's d3d/dxgi imports) get snapped by the loader: Task A
(reflective-map + RIP-redirect) and Task B (process instrumentation
callback). This spike tests **Task B only**.

---

## 1. Question this spike answers

When a process instrumentation callback
(`NtSetInformationProcess`, `ProcessInstrumentationCallback` = information
class 40) is armed on a `CREATE_SUSPENDED` target **before** `ResumeThread`,
does its **first invocation fire before `LdrpInitializeProcess` snaps the
target EXE's own IAT**?

- **Yes** → Task B is a viable pre-init vehicle; carry it forward as the
  vehicle for the real shim.
- **No** (IAT already snapped on first fire) → Task B is dead for pre-init
  virtualization of the EXE's own static imports; fall back to Task A.

This is a binary pass/fail spike. No shim logic, no redirect logic, no
production code — it only measures timing.

---

## 2. Observable (per user decision)

**IAT thunk inspection**, not DllMain marker ordering. The callback handler
reads the target EXE's own import directory, locates the `FirstThunk` array
for one imported function, and classifies it:

- **unsnapped**: thunk value looks like a pre-bind RVA/ordinal-ish small
  value (`< image_base`)
- **snapped**: thunk value is a full VA into the imported DLL
  (`>= image_base` of that DLL, in practice a large pointer far above the
  EXE's own base)

This directly answers the question being asked (state of *this* EXE's *own*
IAT), unlike a DllMain-ordering proxy which would only bound against a
dependency DLL's init — a weaker, indirect signal.

---

## 3. Three throwaway pieces (all under `scratchpad/spike-b/`)

### 3.1 Target EXE (`target.exe`)

Minimal Rust `#[no_std]`-optional (std is fine, this side isn't the
zero-import payload) EXE that statically imports **one** function from a
small helper DLL built alongside it (e.g. a trivial `helper.dll` exporting
`fn helper_value() -> u32`). The EXE calls nothing interesting — on normal
run it just calls the import and exits with its return value cast to an
exit code. Its only job is to *have* a real IAT thunk for that import so
there's something to classify. Built with default MSVC toolchain (no
NODEFAULTLIB tricks needed — this isn't the payload).

### 3.2 Callback stub + data page

Injected into the target's address space by the injector via
`VirtualAllocEx` (RWX page, thrown away after — spike doesn't need
perms hygiene).

- **ABI stub** (hand-written x64 bytes, ~30 bytes): on entry, per the
  documented instrumentation-callback contract, `R10` holds the original
  return RIP and `RAX` holds the original return value. Stub must:
  1. Preserve all volatile registers/flags it touches.
  2. Call the handler (below) with a pointer to the data page (fixed
     address, passed at build time / patched into the stub bytes).
  3. Restore `RAX` to its original value.
  4. `jmp r10` to resume the original control flow.
- **Handler** (small routine, called by the stub): makes **NO syscalls** —
  pure memory reads only, to sidestep re-entrancy/disarm hazards (the
  callback fires on every kernel→user transition, reentrantly, until
  disarmed).
  1. Guarded by a one-shot byte flag on the data page — do the real work
     only on first fire; every subsequent fire is a no-op (flag check +
     return).
  2. On first fire: read `gs:[0x60]` (PEB) → `ImageBaseAddress` → walk the
     PE header/import directory of the EXE image → find the one import's
     `FirstThunk` entry → read its current value → classify
     snapped/unsnapped per §2.
  3. Record to the data page: total fire count (incremented unconditionally,
     even on no-op fires, to sanity-check the callback is actually being
     invoked repeatedly), the first-fire `R10` value, the raw thunk value
     observed, and the classification verdict.

### 3.3 Injector (`inject.exe`, ordinary Rust binary, std is fine)

1. `CreateProcessW(target.exe, ..., CREATE_SUSPENDED)`.
2. `VirtualAllocEx` + `WriteProcessMemory` to place the stub + data page in
   the target.
3. `NtSetInformationProcess(hProcess, ProcessInstrumentationCallback (40),
   &info, sizeof(info))` pointing at the stub.
4. `ResumeThread`.
5. `WaitForSingleObject` on the process (bounded timeout).
6. `ReadProcessMemory` the data page back out; print fire count, first-fire
   verdict, and raw thunk value to stdout.

---

## 4. Success / failure criteria

- **Signal collected** if fire count > 0 and a verdict was recorded. If
  fire count is 0, the callback never fired in this configuration — that's
  itself a finding (mechanism doesn't apply as attempted), not a crash to
  chase.
- **Task B confirmed viable** if the recorded verdict is "unsnapped" on
  first fire.
- **Task B falsified for pre-init** if the verdict is "snapped" on first
  fire (callback fires, but too late).
- A target crash (bad stub corrupting `RAX`/`RSP`) is a stub bug, not an
  answer — iterate on the stub, it isn't evidence either way.

---

## 5. Explicitly out of scope

- Task A (RIP-redirect / reflective map) — separate spike if B is
  falsified.
- Any real shim/redirect logic, zero-import build tricks, disarming the
  callback cleanly, handling multiple imports, x86/WOW64.
- Polish, error handling beyond "don't hang forever" (bounded waits only).
  This code is deleted or left in scratchpad after the answer is obtained;
  it does not enter the workspace.

*End of spec.*

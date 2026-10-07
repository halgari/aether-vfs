# vfs-inject Cross-Process End-to-End — Design Spec

**Status:** Approved-to-proceed (standing goal: drive to a working end-to-end
VFS), ready for planning. **All primitives de-risked by passing spikes.**
**Date:** 2026-07-13
**Slice:** Eighth slice / shim sub-slice 5c — the **last mile**: a director
launches a real target process, injects the shim, and the target's own file
opens are redirected to mod backing files. This closes the loop into a working
end-to-end usermode VFS demonstrated by an automated cross-process test.
**Parent docs:** *VFS Design* (C1–C3), *IPC Architecture* (§2, §8 bootstrap).
**Depends on:** `vfs-shim` (`Engine`, `install`, `HookGuard`), `vfs-core`,
`vfs-shared`, `windows-sys` 0.59.
**Validated recipes (memory):** *vfs-ntcreatefile-hook-recipe* (the hook) and
*vfs-dll-injection-recipe* (LoadLibrary injection) — both proven on stable Rust.

---

## 1. Context & positioning

`vfs-shim` already redirects file opens **in-process**. This slice runs the shim
in a **different** process than the one that built the snapshot:

1. A **director** builds a snapshot and writes it (plus the managed root) to a
   small **config file**.
2. The director launches the **target** suspended, injects **`vfs-shim-dll`**
   (a `cdylib`) via the LoadLibrary technique, and waits for the shim to signal
   readiness.
3. On load, the shim reads the config file, builds an `Engine`, and `install()`s
   the `NtCreateFile` hook — all before the target's main thread runs.
4. The director resumes the target. The target opens a **virtual** path that does
   not exist on disk; the injected hook redirects it to the mod backing file, and
   the target reads mod bytes.

The two hard mechanisms (the hook, the injection) are already proven by spikes;
this slice assembles them plus the bootstrap glue and an automated proof.

### Why a config file (not shared memory yet)

The one-time bootstrap read happens **before** the hook is installed, so it
cannot recurse (G11 governs the hot-path transport, not startup). A plain config
file is the lowest-risk way to hand the snapshot across the process boundary for
this MVP. Swapping in `vfs-win`'s `SharedMapping` + the `vfs-shared` seqlock for a
**live, updatable** snapshot is the natural follow-up — the components already
exist; this slice deliberately uses a static snapshot to keep the end-to-end
proof simple.

---

## 2. Scope & crate boundary

- **`vfs-shim`** (extend): a config codec + a bootstrap entry point (no new
  `unsafe`; reuses the existing hook).
- **`vfs-shim-dll`** (new `cdylib`): the injectable DLL; `DllMain` spawns a
  thread that bootstraps the shim.
- **`vfs-inject`** (new): the injection primitives (all `unsafe` confined to one
  module), a `run_target_with_shim` orchestrator, a `vfs-probe` test-target
  binary, and the end-to-end integration test.

### In scope

- `vfs_shim::encode_config(root, snapshot) -> Vec<u8>` /
  `decode_config(&[u8]) -> Option<(String, Vec<u8>)>` — the tiny bootstrap format
  `[u32 LE root_len][root utf8][snapshot bytes]`.
- `vfs_shim::bootstrap_from_config_path(path) -> Result<HookGuard, BootstrapError>`
  — read the file, decode, build `Engine`, `install`.
- `vfs-shim-dll`: `DllMain` (PROCESS_ATTACH → spawn thread) → read env
  `VFS_SHIM_CONFIG` (+ `VFS_SHIM_READY`) → `bootstrap_from_config_path` → on
  success `mem::forget` the guard (hook persists) and write the ready marker.
- `vfs_inject::inject_dll(process, dll_path)` — VirtualAllocEx +
  WriteProcessMemory + CreateRemoteThread(LoadLibraryW) + wait.
- `vfs_inject::run_target_with_shim(RunConfig) -> Result<i32, InjectError>` —
  launch suspended, set the env, inject, wait for the ready marker (bounded),
  resume, wait for exit, return the exit code.
- `vfs-probe` binary: reads `argv[1]` and writes its bytes to `argv[2]`.
- End-to-end test: probe reads a virtual path → output file contains the backing
  bytes.

### Explicitly out of scope (later)

- Live/updatable snapshot over shared memory + seqlock (this slice uses a static
  config-file snapshot; `vfs-win` + `vfs-shared` seqlock wire in later).
- Child-process tree propagation (auto-injecting spawned children), the real
  `vfs-server` process producing the snapshot over the ring, unnamed-section +
  `DuplicateHandle` security hardening, `NtOpenFile`/directory ops, WOW64/x86,
  identity spoofing, writes/materialize.

---

## 3. The bootstrap + injection flow (both halves spike-proven)

**Config format** (`encode_config`): `root_len: u32 LE`, then `root` UTF-8 bytes,
then the snapshot bytes (the remainder). `decode_config` returns `None` on
truncation or invalid UTF-8 in the root.

**`bootstrap_from_config_path(path)`**: `fs::read(path)` → `decode_config` →
`Engine::new(&root, snapshot)` → `install(engine)`. Errors:
`BootstrapError { Io, BadConfig, Engine(EngineError), Install(InstallError) }`.

**`vfs-shim-dll` `DllMain`** (PROCESS_ATTACH == 1): spawn a `std::thread` (loader
lock forbids heavy work in `DllMain`) that:
1. reads `VFS_SHIM_CONFIG` (config path) and `VFS_SHIM_READY` (ready marker path);
2. `bootstrap_from_config_path(config)`; on `Ok(guard)`, `mem::forget(guard)` so
   the hook stays installed for the process lifetime, then `fs::write(ready,
   b"ready")`;
3. on error, writes nothing to the ready path (the director times out and
   surfaces the failure).

**`run_target_with_shim(RunConfig)`** (director, `unsafe` FFI per the injection
recipe):
1. `set_var("VFS_SHIM_CONFIG", config_path)`, `set_var("VFS_SHIM_READY",
   ready_path)`; remove any stale ready file.
2. `CreateProcessW(target, CREATE_SUSPENDED)` (null env ⇒ child inherits the
   director env with the two vars).
3. `inject_dll(pi.hProcess, dll_path)`.
4. Poll for the ready file up to `ready_timeout` (e.g. 10 s); `Timeout` if it
   never appears.
5. `ResumeThread(pi.hThread)`; `WaitForSingleObject(pi.hProcess, INFINITE)`;
   `GetExitCodeProcess`; close handles; return the exit code.

`RunConfig { target_exe: String, args: Vec<String>, dll_path: String,
config_path: String, ready_path: String, ready_timeout: Duration }`.

---

## 4. API

```rust
// vfs-shim (added)
pub fn encode_config(root: &str, snapshot: &[u8]) -> Vec<u8>;
pub fn decode_config(bytes: &[u8]) -> Option<(String, Vec<u8>)>;
#[derive(Debug)]
pub enum BootstrapError { Io, BadConfig, Engine(EngineError), Install(InstallError) }
pub fn bootstrap_from_config_path(path: &str) -> Result<HookGuard, BootstrapError>;

// vfs-inject
pub struct RunConfig { pub target_exe: String, pub args: Vec<String>, pub dll_path: String,
                       pub config_path: String, pub ready_path: String, pub ready_timeout: std::time::Duration }
#[derive(Debug)]
pub enum InjectError { CreateProcess, Alloc, Write, RemoteThread, Timeout, Wait, ExitCode }
pub fn inject_dll(process: windows_sys::Win32::Foundation::HANDLE, dll_path: &str) -> Result<(), InjectError>; // unsafe internally
pub fn run_target_with_shim(cfg: RunConfig) -> Result<i32, InjectError>;
```

---

## 5. Error handling

- `decode_config` returns `None` (never panics) on any malformed input;
  `bootstrap_from_config_path` maps I/O, decode, engine, and install failures to
  `BootstrapError` variants.
- The DLL bootstrap thread never panics; on failure it simply doesn't signal
  ready, and the director reports `Timeout`.
- `run_target_with_shim` returns `InjectError` for every Win32 failure and always
  closes handles it opened. No panics.

## 6. Testing

- **`vfs-shim` unit tests:** `encode_config`/`decode_config` round-trip;
  `decode_config` returns `None` on a truncated buffer;
  `bootstrap_from_config_path` returns `BootstrapError::Io` for a missing file and
  `BootstrapError::BadConfig` for garbage bytes (these paths do NOT install the
  global hook, so they're safe as ordinary unit tests).
- **`vfs-inject` end-to-end test** (`tests/end_to_end.rs`, single test, its own
  process): create a temp root; write a backing file with known content; build a
  snapshot mapping `asset.dat` → the backing file's absolute path; `encode_config`
  it to a temp file; locate the `vfs-probe` binary via
  `env!("CARGO_BIN_EXE_vfs-probe")` and the shim DLL by checking the test exe's
  own dir then its parent (a dev-dep cdylib lands in `target/debug/deps/`, a
  workspace build in `target/debug/` — verified); run
  `run_target_with_shim` with the probe reading the virtual path and writing to an
  output file; assert exit code 0 and that the output equals the backing content.
- The `vfs-inject` crate dev-depends on `vfs-shim-dll` so the DLL is built before
  the test runs, and on `vfs-shim`/`vfs-core`/`vfs-shared` (bridge) for fixtures.

## 7. Dependencies & toolchain

- **Toolchain:** stable (both spikes confirmed no nightly needed).
- **`vfs-shim`:** unchanged deps; new code is `unsafe`-free.
- **`vfs-shim-dll`:** `[lib] crate-type = ["cdylib"]`; deps `vfs-shim`,
  `windows-sys = { version = "0.59", features = ["Win32_Foundation"] }` (for the
  `DllMain` signature types).
- **`vfs-inject`:** `windows-sys = { version = "0.59", features = [
  "Win32_Foundation", "Win32_Security", "Win32_System_Threading",
  "Win32_System_Memory", "Win32_System_Diagnostics_Debug",
  "Win32_System_LibraryLoader"] }`; a `[[bin]] name = "vfs-probe"`; dev-deps
  `vfs-shim`, `vfs-shim-dll`, `vfs-core`, `vfs-shared` (features `["bridge"]`).
- **Unsafe:** `vfs-inject` is `#![deny(unsafe_code)]` with localized
  `#[allow(unsafe_code)]` in its `inject` module only; `vfs-shim-dll`'s `DllMain`
  is `#[allow(unsafe_code)]`/`extern "system"` (minimal). `vfs-shim`'s additions
  are `unsafe`-free.
- **Workspace:** add `crates/vfs-shim-dll` and `crates/vfs-inject` to `members`.

## 8. Out-of-scope reminders

No live shared-memory snapshot, no child-tree injection, no real server process,
no security hardening, no NtOpenFile/dir ops, no writes, no WOW64.

*End of spec.*

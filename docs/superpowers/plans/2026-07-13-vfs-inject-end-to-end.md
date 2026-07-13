# vfs-inject Cross-Process End-to-End Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A director that launches a target process, injects `vfs-shim-dll`, and makes the target's own file opens redirect to mod backing files — proven by an automated cross-process test where a probe reads a virtual path and receives backing bytes.

**Architecture:** `vfs-shim` gains a config codec + a `bootstrap_from_config_path` entry point (no new `unsafe`). `vfs-shim-dll` is a `cdylib` whose `DllMain` spawns a thread that bootstraps the shim from an env-named config file and signals readiness. `vfs-inject` launches the target suspended, injects the DLL via the LoadLibrary technique (all `unsafe` in one module), waits for readiness, resumes, and returns the exit code; it also ships the `vfs-probe` test target and the end-to-end test.

**Tech Stack:** Rust (stable). `windows-sys` 0.59. Both mechanisms are already validated by spikes (memory: *vfs-ntcreatefile-hook-recipe*, *vfs-dll-injection-recipe*).

## Global Constraints

- Stable Rust. `vfs-inject` uses `#![deny(unsafe_code)]` with localized `#[allow(unsafe_code)]` in its `inject` module ONLY. `vfs-shim`'s additions are `unsafe`-free. `vfs-shim-dll`'s `DllMain` is minimal `unsafe`.
- No panics in library code or the DLL bootstrap thread. Errors are typed.
- Derive `Debug` on `BootstrapError` and `InjectError`.
- Config format: `[u32 LE root_len][root utf8 bytes][snapshot bytes]`.
- Env protocol: director sets `VFS_SHIM_CONFIG` (config file path) and `VFS_SHIM_READY` (ready marker path); the DLL reads both via `std::env::var`.
- `HANDLE` is `*mut c_void` in windows-sys 0.59 (`.is_null()` / `null_mut()`). `GetProcAddress` returns `Option`. `WriteProcessMemory` is in `Win32::System::Diagnostics::Debug`. `CreateProcessW`/`CreateRemoteThread` require the `Win32_Security` feature.
- DLL artifact location from the test: check the test exe's OWN dir first (`target/debug/deps/`, where a dev-dep cdylib lands) then its parent (`target/debug/`, a workspace build) — both verified.
- `vfs_core::SourceId` is `From<&str>` only (use `s.as_str().into()` in fixtures).

---

### Task 1: `vfs-shim` config codec

**Files:**
- Create: `crates/vfs-shim/src/bootstrap.rs`
- Modify: `crates/vfs-shim/src/lib.rs`
- Test: `crates/vfs-shim/src/bootstrap.rs` (`#[cfg(test)]`)

**Interfaces:**
- Produces: `encode_config(root: &str, snapshot: &[u8]) -> Vec<u8>`, `decode_config(bytes: &[u8]) -> Option<(String, Vec<u8>)>`.

- [ ] **Step 1: Write the failing tests**

Create `crates/vfs-shim/src/bootstrap.rs`:

```rust
//! Bootstrap glue: a tiny config codec and a config-file entry point used by the
//! injected DLL to build an `Engine` and install the hook. No `unsafe` here.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips() {
        let snapshot = vec![1u8, 2, 3, 4, 5];
        let bytes = encode_config(r"\??\C:\Games\Skyrim", &snapshot);
        let (root, snap) = decode_config(&bytes).unwrap();
        assert_eq!(root, r"\??\C:\Games\Skyrim");
        assert_eq!(snap, snapshot);
    }

    #[test]
    fn decode_rejects_truncated() {
        // Claims root_len = 100 but no bytes follow.
        let mut bytes = 100u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(b"short");
        assert!(decode_config(&bytes).is_none());
    }

    #[test]
    fn decode_rejects_too_short_for_header() {
        assert!(decode_config(&[0u8, 1]).is_none());
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p vfs-shim config_`
Expected: FAIL to compile (`encode_config`/`decode_config` undefined).

- [ ] **Step 3: Implement the codec**

Add ABOVE the test module in `crates/vfs-shim/src/bootstrap.rs`:

```rust
/// Encode `[u32 LE root_len][root utf8][snapshot bytes]`.
pub fn encode_config(root: &str, snapshot: &[u8]) -> Vec<u8> {
    let root = root.as_bytes();
    let mut out = Vec::with_capacity(4 + root.len() + snapshot.len());
    out.extend_from_slice(&(root.len() as u32).to_le_bytes());
    out.extend_from_slice(root);
    out.extend_from_slice(snapshot);
    out
}

/// Decode a buffer produced by [`encode_config`]. Returns `None` on truncation or
/// invalid UTF-8 in the root. Never panics.
pub fn decode_config(bytes: &[u8]) -> Option<(String, Vec<u8>)> {
    if bytes.len() < 4 {
        return None;
    }
    let root_len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let root_end = 4usize.checked_add(root_len)?;
    if bytes.len() < root_end {
        return None;
    }
    let root = std::str::from_utf8(&bytes[4..root_end]).ok()?.to_string();
    let snapshot = bytes[root_end..].to_vec();
    Some((root, snapshot))
}
```

Add `mod bootstrap;` and `pub use bootstrap::{decode_config, encode_config};` to
`crates/vfs-shim/src/lib.rs` (keep the existing `mod`/`pub use` lines).

- [ ] **Step 4: Run to verify passing**

Run: `cargo test -p vfs-shim`
Expected: PASS (existing tests + 3 new codec tests).

- [ ] **Step 5: Commit**

```bash
git add crates/vfs-shim/src/bootstrap.rs crates/vfs-shim/src/lib.rs
git commit -m "vfs-shim: config codec for cross-process bootstrap"
```

---

### Task 2: `vfs-shim` bootstrap entry point

**Files:**
- Modify: `crates/vfs-shim/src/bootstrap.rs`
- Modify: `crates/vfs-shim/src/lib.rs`
- Test: `crates/vfs-shim/src/bootstrap.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: `encode_config`/`decode_config` (Task 1), `crate::engine::Engine`, `crate::engine::EngineError`, `crate::hook::{install, HookGuard, InstallError}`.
- Produces: `bootstrap_from_config_path(path: &str) -> Result<HookGuard, BootstrapError>`, `enum BootstrapError { Io, BadConfig, Engine(EngineError), Install(InstallError) }` (derives `Debug`).

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `bootstrap.rs`. These exercise only
the ERROR paths (they must not install the global hook):

```rust
    #[test]
    fn bootstrap_missing_file_is_io_error() {
        let err = bootstrap_from_config_path(r"C:\nope\does-not-exist.cfg").unwrap_err();
        assert!(matches!(err, BootstrapError::Io));
    }

    #[test]
    fn bootstrap_garbage_config_is_bad_config() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("vfs-shim-badcfg-{}.bin", std::process::id()));
        std::fs::write(&path, [0u8, 1]).unwrap(); // too short for the header
        let err = bootstrap_from_config_path(path.to_str().unwrap()).unwrap_err();
        assert!(matches!(err, BootstrapError::BadConfig));
        let _ = std::fs::remove_file(&path);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p vfs-shim bootstrap_`
Expected: FAIL to compile (`bootstrap_from_config_path`/`BootstrapError` undefined).

- [ ] **Step 3: Implement the entry point**

Add to `bootstrap.rs` (above the tests), and add the imports it needs:

```rust
use crate::engine::{Engine, EngineError};
use crate::hook::{install, HookGuard, InstallError};

/// Errors bootstrapping the shim from a config file.
#[derive(Debug)]
pub enum BootstrapError {
    /// The config file could not be read.
    Io,
    /// The config bytes were malformed.
    BadConfig,
    /// The engine could not be built (bad root or snapshot).
    Engine(EngineError),
    /// The hook could not be installed.
    Install(InstallError),
}

/// Read a config file, build an `Engine`, and install the NtCreateFile hook.
/// Returns the guard keeping the hook alive (the injected DLL leaks it).
pub fn bootstrap_from_config_path(path: &str) -> Result<HookGuard, BootstrapError> {
    let bytes = std::fs::read(path).map_err(|_| BootstrapError::Io)?;
    let (root, snapshot) = decode_config(&bytes).ok_or(BootstrapError::BadConfig)?;
    let engine = Engine::new(&root, snapshot).map_err(BootstrapError::Engine)?;
    install(engine).map_err(BootstrapError::Install)
}
```

Add `pub use bootstrap::{bootstrap_from_config_path, BootstrapError};` to
`crates/vfs-shim/src/lib.rs`.

- [ ] **Step 4: Run to verify passing**

Run: `cargo test -p vfs-shim`
Expected: PASS (all prior + 2 new error-path tests).

- [ ] **Step 5: Commit**

```bash
git add crates/vfs-shim/src/bootstrap.rs crates/vfs-shim/src/lib.rs
git commit -m "vfs-shim: bootstrap_from_config_path entry point"
```

---

### Task 3: `vfs-shim-dll` cdylib

**Files:**
- Create: `crates/vfs-shim-dll/Cargo.toml`
- Create: `crates/vfs-shim-dll/src/lib.rs`
- Modify: `Cargo.toml` (workspace `members`)

**Interfaces:**
- Consumes: `vfs_shim::bootstrap_from_config_path`.
- Produces: a `vfs_shim_dll.dll` whose `DllMain` bootstraps the shim on load.

- [ ] **Step 1: Add the crate to workspace members**

In root `Cargo.toml`, add `"crates/vfs-shim-dll"` to `members` (alphabetical: after `crates/vfs-shim`, before `crates/vfs-win`).

- [ ] **Step 2: Write `crates/vfs-shim-dll/Cargo.toml`**

```toml
[package]
name = "vfs-shim-dll"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
vfs-shim = { path = "../vfs-shim" }
windows-sys = { version = "0.59", features = ["Win32_Foundation"] }
```

- [ ] **Step 3: Write `crates/vfs-shim-dll/src/lib.rs`**

```rust
//! Injectable shim DLL. On load, a background thread bootstraps the shim from the
//! config file named by `VFS_SHIM_CONFIG` and signals readiness via
//! `VFS_SHIM_READY`. Kept minimal — real work happens off the loader lock.
#![allow(unsafe_code)]

use core::ffi::c_void;
use windows_sys::Win32::Foundation::{BOOL, HINSTANCE, TRUE};

const DLL_PROCESS_ATTACH: u32 = 1;

/// Standard DLL entry point. Spawns a thread (loader lock forbids heavy work
/// here) that installs the hook, then returns immediately.
#[no_mangle]
pub extern "system" fn DllMain(_dll: HINSTANCE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        std::thread::spawn(bootstrap);
    }
    TRUE
}

/// Runs on a fresh thread after `DllMain` returns. Bootstraps the shim and, on
/// success, leaks the guard (hook persists for the process lifetime) and writes
/// the ready marker. On failure, signals nothing — the director times out.
fn bootstrap() {
    let config = match std::env::var("VFS_SHIM_CONFIG") {
        Ok(c) => c,
        Err(_) => return,
    };
    if let Ok(guard) = vfs_shim::bootstrap_from_config_path(&config) {
        core::mem::forget(guard);
        if let Ok(ready) = std::env::var("VFS_SHIM_READY") {
            let _ = std::fs::write(&ready, b"ready");
        }
    }
}
```

- [ ] **Step 4: Build the cdylib**

Run: `cargo build -p vfs-shim-dll`
Expected: compiles; produces `vfs_shim_dll.dll` under `target/debug/`.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/vfs-shim-dll/Cargo.toml crates/vfs-shim-dll/src/lib.rs
git commit -m "vfs-shim-dll: injectable cdylib that bootstraps the shim on load"
```

---

### Task 4: `vfs-inject` scaffold, `vfs-probe` target, and public types

**Files:**
- Create: `crates/vfs-inject/Cargo.toml`
- Create: `crates/vfs-inject/src/lib.rs`
- Create: `crates/vfs-inject/src/bin/vfs-probe.rs`
- Modify: `Cargo.toml` (workspace `members`)

**Interfaces:**
- Produces: `RunConfig`, `InjectError` (derives `Debug`); the `vfs-probe` binary; a stub `run_target_with_shim` (implemented in Task 5).

- [ ] **Step 1: Add the crate to workspace members**

In root `Cargo.toml`, add `"crates/vfs-inject"` to `members` (alphabetical: after `crates/vfs-ipc`, before `crates/vfs-redirect`).

- [ ] **Step 2: Write `crates/vfs-inject/Cargo.toml`**

```toml
[package]
name = "vfs-inject"
version = "0.1.0"
edition = "2021"

[dependencies]
windows-sys = { version = "0.59", features = [
  "Win32_Foundation",
  "Win32_Security",
  "Win32_System_Threading",
  "Win32_System_Memory",
  "Win32_System_Diagnostics_Debug",
  "Win32_System_LibraryLoader",
] }

[dev-dependencies]
vfs-shim = { path = "../vfs-shim" }
vfs-shim-dll = { path = "../vfs-shim-dll" }
vfs-core = { path = "../vfs-core" }
vfs-shared = { path = "../vfs-shared", features = ["bridge"] }

[[bin]]
name = "vfs-probe"
path = "src/bin/vfs-probe.rs"
```

- [ ] **Step 3: Write the `vfs-probe` target**

Create `crates/vfs-inject/src/bin/vfs-probe.rs`:

```rust
//! Test target: read the file at argv[1] and write its bytes to argv[2].
//! When injected, argv[1] is a VIRTUAL path the shim redirects to a mod file.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        std::process::exit(2);
    }
    let content = std::fs::read(&args[1]).unwrap_or_default();
    std::fs::write(&args[2], &content).expect("probe write output");
}
```

- [ ] **Step 4: Write `crates/vfs-inject/src/lib.rs` with the types and a stub**

```rust
#![deny(unsafe_code)]

//! Launch a target process, inject the shim DLL, and run it with file-open
//! redirection active.

use std::time::Duration;

mod inject;

/// Parameters for [`run_target_with_shim`].
pub struct RunConfig {
    pub target_exe: String,
    pub args: Vec<String>,
    pub dll_path: String,
    pub config_path: String,
    pub ready_path: String,
    pub ready_timeout: Duration,
}

/// Failure points in launch + inject + run.
#[derive(Debug)]
pub enum InjectError {
    CreateProcess,
    Alloc,
    Write,
    RemoteThread,
    Timeout,
    Wait,
    ExitCode,
}

pub use inject::run_target_with_shim;
```

- [ ] **Step 5: Write a stub `crates/vfs-inject/src/inject.rs` so it compiles**

```rust
//! All Win32 injection FFI (implemented in the next task).
use crate::{InjectError, RunConfig};

pub fn run_target_with_shim(_cfg: RunConfig) -> Result<i32, InjectError> {
    Err(InjectError::CreateProcess)
}
```

- [ ] **Step 6: Build**

Run: `cargo build -p vfs-inject`
Expected: compiles (unused-field warnings on `RunConfig` are acceptable at this stub step).

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml crates/vfs-inject/Cargo.toml crates/vfs-inject/src/lib.rs crates/vfs-inject/src/inject.rs crates/vfs-inject/src/bin/vfs-probe.rs
git commit -m "vfs-inject: crate scaffold, vfs-probe target, public types"
```

---

### Task 5: Injection FFI + end-to-end test

**Files:**
- Modify: `crates/vfs-inject/src/inject.rs`
- Test: `crates/vfs-inject/tests/end_to_end.rs`

**Interfaces:**
- Consumes: `windows-sys`, `RunConfig`, `InjectError`. Test consumes `vfs_shim::encode_config`, `vfs_core`, `vfs_shared::bridge`.
- Produces: a working `run_target_with_shim`.

- [ ] **Step 1: Write the failing end-to-end test**

Create `crates/vfs-inject/tests/end_to_end.rs`:

```rust
//! Single-test binary: launch the probe, inject the shim, and verify the probe's
//! read of a VIRTUAL path was redirected to the backing file.
use std::time::Duration;
use vfs_inject::{run_target_with_shim, RunConfig};

fn locate_dll() -> String {
    // The test exe lives in target/debug/deps/. A dev-dep cdylib lands there;
    // a workspace build lands in target/debug/. Check both.
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().to_path_buf();
    for cand in [dir.join("vfs_shim_dll.dll"), dir.parent().unwrap().join("vfs_shim_dll.dll")] {
        if cand.exists() {
            return cand.to_str().unwrap().to_string();
        }
    }
    panic!("vfs_shim_dll.dll not found near {dir:?}");
}

#[test]
fn injected_shim_redirects_target_file_open() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-e2e-{pid}"));
    let root = base.join("gameroot");
    let backing_dir = base.join("mods");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&backing_dir).unwrap();

    let backing = backing_dir.join("asset.dat");
    std::fs::write(&backing, b"REDIRECTED MOD CONTENT").unwrap();
    let virtual_path = root.join("asset.dat"); // NOT created on disk
    assert!(std::fs::read(&virtual_path).is_err());

    // Snapshot: vpath `asset.dat` -> the backing file's absolute path.
    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let source = backing.to_str().unwrap();
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "asset.dat".into(),
                kind: EntryKind::File,
                source: source.into(), // SourceId: From<&str>
                size: 22,
                mtime: 1,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };

    // Config file mapping the root + snapshot.
    let config_bytes = vfs_shim::encode_config(root.to_str().unwrap(), &snapshot);
    let config_path = base.join("shim.cfg");
    std::fs::write(&config_path, &config_bytes).unwrap();

    let ready_path = base.join("ready.flag");
    let _ = std::fs::remove_file(&ready_path);
    let output_path = base.join("probe-out.bin");
    let _ = std::fs::remove_file(&output_path);

    let probe = env!("CARGO_BIN_EXE_vfs-probe").to_string();
    let dll = locate_dll();

    let exit = run_target_with_shim(RunConfig {
        target_exe: probe,
        args: vec![
            virtual_path.to_str().unwrap().to_string(),
            output_path.to_str().unwrap().to_string(),
        ],
        dll_path: dll,
        config_path: config_path.to_str().unwrap().to_string(),
        ready_path: ready_path.to_str().unwrap().to_string(),
        ready_timeout: Duration::from_secs(10),
    })
    .expect("run_target_with_shim");

    assert_eq!(exit, 0, "probe exit code");
    let got = std::fs::read(&output_path).expect("probe output");
    assert_eq!(got, b"REDIRECTED MOD CONTENT", "redirect did not deliver mod bytes");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p vfs-inject --test end_to_end`
Expected: FAIL — the stub returns `Err(InjectError::CreateProcess)`, so `.expect(...)` panics.

- [ ] **Step 3: Implement `run_target_with_shim` (the injection FFI)**

Replace `crates/vfs-inject/src/inject.rs` with (this is the validated spike code,
generalized and wired to `RunConfig`; ALL `unsafe` is in this file):

```rust
//! All Win32 injection FFI. Validated by the dll-injection spike.
#![allow(unsafe_code)]

use core::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::time::Instant;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Memory::{
    VirtualAllocEx, MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, CreateRemoteThread, GetExitCodeProcess, ResumeThread, WaitForSingleObject,
    CREATE_SUSPENDED, INFINITE, LPTHREAD_START_ROUTINE, PROCESS_INFORMATION, STARTUPINFOW,
};

use crate::{InjectError, RunConfig};

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// Inject `dll_path` into a process via `LoadLibraryW` on a remote thread.
fn inject_dll(process: HANDLE, dll_path: &str) -> Result<(), InjectError> {
    // SAFETY: standard remote LoadLibrary injection; `process` is a live process
    // handle with the needed rights (from CreateProcessW). Validated by spike.
    unsafe {
        let dll_w = wide(dll_path);
        let bytes = dll_w.len() * 2;
        let remote = VirtualAllocEx(process, core::ptr::null(), bytes, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
        if remote.is_null() {
            return Err(InjectError::Alloc);
        }
        let mut written = 0usize;
        let ok = WriteProcessMemory(process, remote, dll_w.as_ptr() as *const c_void, bytes, &mut written);
        if ok == 0 || written != bytes {
            return Err(InjectError::Write);
        }
        let k32 = GetModuleHandleW(wide("kernel32.dll").as_ptr());
        if k32.is_null() {
            return Err(InjectError::RemoteThread);
        }
        let load_library = match GetProcAddress(k32, b"LoadLibraryW\0".as_ptr()) {
            Some(p) => p,
            None => return Err(InjectError::RemoteThread),
        };
        let start: LPTHREAD_START_ROUTINE = Some(core::mem::transmute(load_library));
        let hthread = CreateRemoteThread(process, core::ptr::null(), 0, start, remote, 0, core::ptr::null_mut());
        if hthread.is_null() || hthread == INVALID_HANDLE_VALUE {
            return Err(InjectError::RemoteThread);
        }
        WaitForSingleObject(hthread, INFINITE);
        CloseHandle(hthread);
        Ok(())
    }
}

/// Launch the target suspended, inject the shim, wait for readiness, resume, and
/// return the target's exit code.
pub fn run_target_with_shim(cfg: RunConfig) -> Result<i32, InjectError> {
    // The child inherits our env (null lpEnvironment), so set the shim vars here.
    std::env::set_var("VFS_SHIM_CONFIG", &cfg.config_path);
    std::env::set_var("VFS_SHIM_READY", &cfg.ready_path);
    let _ = std::fs::remove_file(&cfg.ready_path);

    // Build the command line: "exe" "arg1" "arg2" ... (mutable buffer required).
    let mut cmdline = format!("\"{}\"", cfg.target_exe);
    for a in &cfg.args {
        cmdline.push_str(&format!(" \"{a}\""));
    }
    let app_w = wide(&cfg.target_exe);
    let mut cmd_w = wide(&cmdline);

    // SAFETY: standard CreateProcessW + inject + resume; handles are closed on
    // every exit path. Validated by spike.
    unsafe {
        let mut si: STARTUPINFOW = zeroed();
        si.cb = size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = zeroed();
        let ok = CreateProcessW(
            app_w.as_ptr(),
            cmd_w.as_mut_ptr(),
            core::ptr::null(),
            core::ptr::null(),
            0,
            CREATE_SUSPENDED,
            core::ptr::null(),
            core::ptr::null(),
            &si,
            &mut pi,
        );
        if ok == 0 {
            return Err(InjectError::CreateProcess);
        }

        // Inject; on failure, tear the process down.
        if let Err(e) = inject_dll(pi.hProcess, &cfg.dll_path) {
            let _ = ResumeThread(pi.hThread); // let it die naturally
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            return Err(e);
        }

        // Wait for the shim to signal it installed the hook.
        let deadline = Instant::now() + cfg.ready_timeout;
        while !std::path::Path::new(&cfg.ready_path).exists() {
            if Instant::now() >= deadline {
                CloseHandle(pi.hThread);
                CloseHandle(pi.hProcess);
                return Err(InjectError::Timeout);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Resume and wait for exit.
        ResumeThread(pi.hThread);
        if WaitForSingleObject(pi.hProcess, INFINITE) != 0 {
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            return Err(InjectError::Wait);
        }
        let mut code: u32 = 0;
        let got = GetExitCodeProcess(pi.hProcess, &mut code);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
        if got == 0 {
            return Err(InjectError::ExitCode);
        }
        Ok(code as i32)
    }
}
```

- [ ] **Step 4: Run the end-to-end test**

Run: `cargo test -p vfs-inject --test end_to_end`
Expected: PASS — `injected_shim_redirects_target_file_open` reads `"REDIRECTED MOD CONTENT"` from the probe's output.

If it fails, DO NOT modify the test to pass. STOP and report the exact failure. A
compile error on a `windows-sys` signature → adapt only to the compiler. A
runtime failure (timeout, wrong bytes) → report the details: both mechanisms are
spike-proven, so a runtime failure is important signal (e.g. the DLL wasn't built
— confirm `cargo build -p vfs-shim-dll` produced `vfs_shim_dll.dll`, which the
dev-dependency should force).

- [ ] **Step 5: Commit**

```bash
git add crates/vfs-inject/src/inject.rs crates/vfs-inject/tests/end_to_end.rs
git commit -m "vfs-inject: LoadLibrary injection + cross-process redirect end-to-end test"
```

---

### Task 6: Verification sweep

**Files:** none (verification only).

- [ ] **Step 1: Full workspace test**

Run: `cargo test --workspace`
Expected: all crates green, including the `vfs-inject` end-to-end test.

- [ ] **Step 2: Warning check**

Run: `cargo build --workspace 2>&1 | rg -i "warning" || echo "no warnings"`
Expected: `no warnings`.

- [ ] **Step 3: Unsafe audit**

Run: confirm `vfs-inject/src/lib.rs` is `#![deny(unsafe_code)]`, all `unsafe` in
`vfs-inject` is in `inject.rs` (under its `#![allow(unsafe_code)]` with `// SAFETY:`
notes), `vfs-shim`'s `bootstrap.rs` has no `unsafe`, and `vfs-shim-dll` has only
the `DllMain` boundary.
Expected: unsafe confined as described.

- [ ] **Step 4: Commit Cargo.lock**

```bash
git add Cargo.lock
git commit -m "vfs-inject/vfs-shim-dll: update Cargo.lock"
```

---

## Self-Review Notes

- **Spec coverage:** config codec (Task 1), `bootstrap_from_config_path` (Task 2), `vfs-shim-dll` DllMain (Task 3), injection primitive + orchestrator + probe + types (Tasks 4–5), end-to-end proof (Task 5 test), workspace wiring + deps + localized unsafe (Tasks + Global Constraints + Task 6 audit). Readiness coordination (wait for the ready file before resume) is in Task 5's `run_target_with_shim`.
- **Derives:** `BootstrapError` and `InjectError` derive `Debug` (used via `.unwrap_err()`/`matches!`/`.expect()`). `EngineError`/`InstallError` already derive `Debug` (from `vfs-shim`), so `BootstrapError`'s derive holds.
- **Type consistency:** `run_target_with_shim(RunConfig) -> Result<i32, InjectError>` matches the test's call; `encode_config(&str, &[u8]) -> Vec<u8>` matches `decode_config`'s inverse; the DLL reads `VFS_SHIM_CONFIG`/`VFS_SHIM_READY` exactly as the director sets them; `inject_dll` takes `HANDLE` (windows-sys) internally and is not part of the public API (no leaked type).
- **No-panic:** library code and the DLL bootstrap thread use `Result`/`if let`/`match`; the only `expect` is in the `vfs-probe` binary (a test target, where a panic → nonzero exit is acceptable and detected by the exit-code assert).
- **Artifact location:** the test checks the test-exe dir then its parent for `vfs_shim_dll.dll` — both cases verified empirically; the `vfs-shim-dll` dev-dependency forces the DLL to build.

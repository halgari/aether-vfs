# Spike B — Instrumentation-Callback Timing — Implementation Plan

**Status: SHELVED.** Do not continue this plan. Production pre-init injection is
reflective-map + RIP-redirect — see
`docs/superpowers/plans/2026-07-14-preinit-injection.md` and the design
`docs/superpowers/specs/2026-07-14-preinit-injection-design.md`.
`scratchpad/spike-b/` is retained as evidence only.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Answer one binary question — does a process instrumentation callback
armed on a suspended process fire *before* the target's own static-import IAT
is bound by the loader? This determines whether Task B is a viable pre-init
vehicle for the real VFS shim (see [[vfs-nostd-payload-recipe]]).

**Architecture:** Three throwaway Win32 binaries in an isolated Cargo
workspace at `scratchpad/spike-b/` (NOT added to the repo's root workspace):
`helper.dll` (one exported function, gives the target something to import),
`target-exe.exe` (statically imports that function — has a real IAT thunk to
observe), and `injector.exe` (creates the target suspended, writes a
hand-assembled callback stub + data page into it, arms
`NtSetInformationProcess` class 40, resumes, and reads back what the stub
observed).

**Tech Stack:** Rust, stable toolchain, `windows-sys 0.59` (features mirrored
from `crates/vfs-shim`/`crates/vfs-inject`, both already proven in this repo),
`core::arch::global_asm!` (stable) for the callback stub machine code.

## Global Constraints

- This is throwaway spike code per
  `docs/superpowers/specs/2026-07-13-spike-b-instrumentation-callback-timing-design.md`:
  lives entirely under `scratchpad/spike-b/`, its own Cargo workspace, never
  added to the repo root `Cargo.toml` `members` list.
- No shim/redirect logic, no disarming polish, no x86/WOW64 — single Windows
  x64 host, single run, read the printed verdict.
- The callback handler (inside the stub) makes **zero syscalls** — pure memory
  reads only, per the design's re-entrancy rationale.
- Mirror existing proven windows-sys incantations from `crates/vfs-shim/src/hook.rs`
  (ntdll `GetModuleHandleA`/`GetProcAddress` pattern) and
  `crates/vfs-inject/src/inject.rs` (`CreateProcessW`/`VirtualAllocEx`/
  `WriteProcessMemory`/resume/wait pattern) exactly — do not re-derive these
  signatures from scratch.
- Build order matters and is NOT Cargo-managed (no `[build-dependencies]`
  edge): always build `helper` before `target-exe`, per each task's exact
  commands below.

---

### Task 1: Helper DLL + Target EXE

**Files:**
- Create: `scratchpad/spike-b/Cargo.toml`
- Create: `scratchpad/spike-b/helper/Cargo.toml`
- Create: `scratchpad/spike-b/helper/src/lib.rs`
- Create: `scratchpad/spike-b/target-exe/Cargo.toml`
- Create: `scratchpad/spike-b/target-exe/build.rs`
- Create: `scratchpad/spike-b/target-exe/src/main.rs`

**Interfaces:**
- Produces: `helper.dll` exporting `extern "C" fn helper_value() -> u32`
  (returns `42`). `target-exe.exe` statically imports and calls it, exits with
  that value as its process exit code. Both land in
  `scratchpad/spike-b/target/debug/` (shared workspace target dir) — later
  tasks rely on this to locate `target-exe.exe` next to the injector binary.

- [ ] **Step 1: Create the workspace root**

`scratchpad/spike-b/Cargo.toml`:
```toml
[workspace]
resolver = "2"
members = ["helper", "target-exe", "injector"]
```

(The `injector` member doesn't exist yet — that's fine, Cargo only needs it to
exist by the time you build that member; `cargo build -p helper` and
`cargo build -p target-exe` below don't require it to exist yet... actually
Cargo resolves the whole `members` list on any invocation, so create an empty
placeholder now to avoid a "package not found" error:)

Also create `scratchpad/spike-b/injector/Cargo.toml` right away as a minimal
stub so Task 1's builds don't fail workspace resolution:
```toml
[package]
name = "injector"
version = "0.1.0"
edition = "2021"
```
and `scratchpad/spike-b/injector/src/main.rs`:
```rust
fn main() {}
```
(Task 2 replaces this file's contents.)

- [ ] **Step 2: Create the helper crate**

`scratchpad/spike-b/helper/Cargo.toml`:
```toml
[package]
name = "helper"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]
```

`scratchpad/spike-b/helper/src/lib.rs`:
```rust
#[no_mangle]
pub extern "C" fn helper_value() -> u32 {
    42
}
```

- [ ] **Step 3: Create the target-exe crate**

`scratchpad/spike-b/target-exe/Cargo.toml`:
```toml
[package]
name = "target-exe"
version = "0.1.0"
edition = "2021"
build = "build.rs"

[[bin]]
name = "target-exe"
path = "src/main.rs"
```

`scratchpad/spike-b/target-exe/build.rs`:
```rust
use std::env;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let target_dir = format!("{manifest_dir}/../target/debug");
    println!("cargo:rustc-link-search=native={target_dir}");
    // MSVC names the cdylib's import library "helper.dll.lib". Passing
    // "helper.dll" here makes rustc ask the linker for "helper.dll" + ".lib"
    // = "helper.dll.lib", matching that name exactly.
    println!("cargo:rustc-link-lib=dylib=helper.dll");
}
```

`scratchpad/spike-b/target-exe/src/main.rs`:
```rust
extern "C" {
    fn helper_value() -> u32;
}

fn main() {
    let value = unsafe { helper_value() };
    std::process::exit(value as i32);
}
```

- [ ] **Step 4: Build in dependency order and verify**

Run (from `scratchpad/spike-b/`):
```
cargo build -p helper
cargo build -p target-exe
.\target\debug\target-exe.exe
echo $LASTEXITCODE
```
Expected: `42`.

**If the `target-exe` link fails** with something like
`LNK1181: cannot open input file 'helper.dll.lib'`: run
`dir target\debug\helper*` and check the exact generated filename. If it's
`helper.dll.lib`, the `dylib=helper.dll` line above is correct — the failure
means `helper` wasn't built first (re-run `cargo build -p helper`). If the
file is named differently (e.g. plain `helper.lib`), change the build.rs line
to `cargo:rustc-link-lib=dylib=helper` instead and rebuild.

- [ ] **Step 5: Commit**

```bash
cd C:/oss/vfs
git add scratchpad/spike-b/Cargo.toml scratchpad/spike-b/helper scratchpad/spike-b/target-exe scratchpad/spike-b/injector
git -c user.name="Claude" -c user.email="noreply@anthropic.com" commit -m "spike(b): helper DLL + target EXE scaffold"
```

---

### Task 2: Injector process-control skeleton (no injection yet)

**Files:**
- Modify: `scratchpad/spike-b/injector/Cargo.toml`
- Modify: `scratchpad/spike-b/injector/src/main.rs`

**Interfaces:**
- Consumes: nothing from Task 1 except the built `target-exe.exe` binary
  sitting in the same `target/debug/` directory as `injector.exe` (both are
  workspace members, so this is automatic).
- Produces: a working suspend → resume → wait → exit-code pipeline, proven
  BEFORE adding the risky stub-injection logic in Task 3. Later tasks extend
  `main()` in place.

This task deliberately proves the plumbing (`CreateProcessW` with
`CREATE_SUSPENDED`, `ResumeThread`, `WaitForSingleObject`,
`GetExitCodeProcess`) works before any hand-written assembly enters the
picture — if something's wrong here, it's cheap to diagnose; if it's wrong
after Task 3 adds the stub, it's much harder to tell whether the bug is in the
process control or the assembly.

- [ ] **Step 1: Add windows-sys dependency**

`scratchpad/spike-b/injector/Cargo.toml`:
```toml
[package]
name = "injector"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "injector"
path = "src/main.rs"

[dependencies]
windows-sys = { version = "0.59", features = [
  "Win32_Foundation",
  "Win32_Security",
  "Win32_System_Threading",
  "Win32_System_Memory",
  "Win32_System_Diagnostics_Debug",
  "Win32_System_LibraryLoader",
] }
```

- [ ] **Step 2: Write the skeleton**

`scratchpad/spike-b/injector/src/main.rs`:
```rust
use std::env;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, ResumeThread, WaitForSingleObject, CREATE_SUSPENDED,
    INFINITE, PROCESS_INFORMATION, STARTUPINFOW,
};

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// `target-exe.exe` lives next to this binary (same workspace target dir).
fn target_exe_path() -> String {
    let mut path = env::current_exe().expect("current_exe");
    path.set_file_name("target-exe.exe");
    path.to_string_lossy().into_owned()
}

fn main() {
    let target = target_exe_path();
    let app_w = wide(&target);
    let mut cmd_w = wide(&format!("\"{target}\""));

    // SAFETY: standard CreateProcessW + suspend/resume/wait; every handle
    // opened here is closed before the function returns.
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
        assert!(ok != 0, "CreateProcessW failed: {}", std::io::Error::last_os_error());
        println!("target created suspended, pid={}", pi.dwProcessId);

        let resumed = ResumeThread(pi.hThread);
        assert!(resumed != u32::MAX, "ResumeThread failed: {}", std::io::Error::last_os_error());

        let wait = WaitForSingleObject(pi.hProcess, INFINITE);
        assert_eq!(wait, 0, "WaitForSingleObject unexpected result: {wait}");

        let mut exit_code: u32 = 0;
        let got = GetExitCodeProcess(pi.hProcess, &mut exit_code);
        assert!(got != 0, "GetExitCodeProcess failed");
        println!("target exited with code {exit_code}");

        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    }
}
```

- [ ] **Step 3: Build and run**

```
cargo build -p injector
.\target\debug\injector.exe
```
Expected output:
```
target created suspended, pid=<some number>
target exited with code 42
```

- [ ] **Step 4: Commit**

```bash
cd C:/oss/vfs
git add scratchpad/spike-b/injector
git -c user.name="Claude" -c user.email="noreply@anthropic.com" commit -m "spike(b): injector suspend/resume/wait skeleton (no callback yet)"
```

---

### Task 3: Callback stub bytes + cross-process write + verify

**Files:**
- Create: `scratchpad/spike-b/injector/src/stub.rs`
- Create: `scratchpad/spike-b/injector/src/datapage.rs`
- Modify: `scratchpad/spike-b/injector/src/main.rs`

**Interfaces:**
- Produces from `stub.rs`: `pub const DATA_PAGE_MAGIC: u64`,
  `pub fn stub_bytes() -> Vec<u8>` (extracts the compiled stub's own machine
  code out of the injector's own loaded image — see Step 1), and
  `pub fn patch_data_page_address(stub: &mut [u8], addr: u64)` (finds the
  8-byte little-endian `DATA_PAGE_MAGIC` sequence inside `stub` and overwrites
  it with `addr`'s little-endian bytes).
- Produces from `datapage.rs`: byte offset constants
  (`OFFSET_FLAG=0, OFFSET_THUNK_VALUE=8, OFFSET_VERDICT=16,
  OFFSET_FIRST_R10=24, OFFSET_FIRE_COUNT=32`) and `pub struct Decoded` /
  `pub fn decode(buf: &[u8]) -> Decoded` — Task 4 uses these to interpret the
  data page read back from the target.
- This task does NOT call `NtSetInformationProcess` yet — it only proves the
  stub bytes can be extracted, patched, and written into the suspended
  target's memory correctly (verified by reading them back and comparing).
  Task 4 arms the callback and lets the target run.

**What the stub does (full assembly, explained):** on entry, per the
documented instrumentation-callback ABI, `R10` holds the interrupted return
address and `RAX` holds the interrupted return value. The stub:
1. Saves every general-purpose register + flags (so the body below can use
   any of them as scratch — they're restored in exact reverse order before
   returning, including `R10` and `RAX`).
2. Loads the data-page address (patched into a placeholder 64-bit immediate
   `0xDEADBEEFCAFEBABE`) into `r15`.
3. Checks a one-shot flag byte at `[r15+0]`. If already set, skips straight to
   the unconditional fire-count increment (every fire is still counted, only
   the classification work runs once).
4. On first fire: reads the PEB via the well-known `gs:[0x60]` offset, then
   `ImageBaseAddress` at `PEB+0x10`, then walks the PE headers
   (`e_lfanew` at `image_base+0x3C` → `IMAGE_NT_HEADERS64` → import
   directory RVA at `nt_headers+24+120` per the documented `IMAGE_OPTIONAL_HEADER64`
   layout → first `IMAGE_IMPORT_DESCRIPTOR`'s `FirstThunk` field at offset
   `+16`) to find the IAT slot for the target's one import, reads its current
   value, and classifies it: `< image_base` ⇒ unsnapped (byte `1`),
   otherwise ⇒ snapped (byte `2`). Records the raw thunk value, verdict byte,
   and the original `R10` to the data page.
5. Restores every register in reverse order (this is what puts `R10`/`RAX`
   back to their original values even though the body used them as scratch)
   and `jmp r10` to resume exactly where the callback interrupted.

**No calls, no syscalls, no external symbol references** inside the asm
block — the only "external" data is the one patched 8-byte immediate, so the
extracted bytes are fully position-independent and safe to copy into another
process's address space verbatim.

- [ ] **Step 1: Write the stub module**

`scratchpad/spike-b/injector/src/stub.rs`:
```rust
use std::arch::global_asm;

global_asm!(
    ".global callback_stub_start",
    ".global callback_stub_end",
    "callback_stub_start:",
    "pushfq",
    "push rax",
    "push rbx",
    "push rcx",
    "push rdx",
    "push rsi",
    "push rdi",
    "push rbp",
    "push r8",
    "push r9",
    "push r10",
    "push r11",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "movabs r15, 0xDEADBEEFCAFEBABE",
    "cmp byte ptr [r15], 0",
    "jne callback_stub_skip_work",
    "mov byte ptr [r15], 1",
    "mov rax, gs:[0x60]",
    "mov rax, [rax+0x10]",
    "mov rbx, rax",
    "mov ecx, dword ptr [rax+0x3C]",
    "add rax, rcx",
    "mov ecx, dword ptr [rax+144]",
    "add rcx, rbx",
    "mov edx, dword ptr [rcx+16]",
    "add rdx, rbx",
    "mov rax, [rdx]",
    "mov [r15+8], rax",
    "cmp rax, rbx",
    "jb callback_stub_unsnapped",
    "mov byte ptr [r15+16], 2",
    "jmp callback_stub_store_r10",
    "callback_stub_unsnapped:",
    "mov byte ptr [r15+16], 1",
    "callback_stub_store_r10:",
    "mov [r15+24], r10",
    "callback_stub_skip_work:",
    "add dword ptr [r15+32], 1",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop r11",
    "pop r10",
    "pop r9",
    "pop r8",
    "pop rbp",
    "pop rdi",
    "pop rsi",
    "pop rdx",
    "pop rcx",
    "pop rbx",
    "pop rax",
    "popfq",
    "jmp r10",
    "callback_stub_end:",
);

extern "C" {
    fn callback_stub_start();
    fn callback_stub_end();
}

pub const DATA_PAGE_MAGIC: u64 = 0xDEAD_BEEF_CAFE_BABE;

/// Copies the compiled stub's own machine code out of THIS process's loaded
/// image. Position-independent aside from the one patched magic constant, so
/// the returned bytes are safe to write verbatim into another process.
pub fn stub_bytes() -> Vec<u8> {
    let start = callback_stub_start as usize;
    let end = callback_stub_end as usize;
    assert!(end > start, "bad stub symbol range: start={start:#x} end={end:#x}");
    let len = end - start;
    unsafe { std::slice::from_raw_parts(start as *const u8, len) }.to_vec()
}

/// Finds the 8-byte little-endian `DATA_PAGE_MAGIC` sequence in `stub` and
/// overwrites it with `addr`'s little-endian bytes.
pub fn patch_data_page_address(stub: &mut [u8], addr: u64) {
    let needle = DATA_PAGE_MAGIC.to_le_bytes();
    let pos = stub
        .windows(8)
        .position(|w| w == needle)
        .expect("magic constant not found in stub bytes");
    stub[pos..pos + 8].copy_from_slice(&addr.to_le_bytes());
}
```

- [ ] **Step 2: Write the data-page decode module**

`scratchpad/spike-b/injector/src/datapage.rs`:
```rust
pub const DATA_PAGE_SIZE: usize = 4096;

pub const OFFSET_FLAG: usize = 0;
pub const OFFSET_THUNK_VALUE: usize = 8;
pub const OFFSET_VERDICT: usize = 16;
pub const OFFSET_FIRST_R10: usize = 24;
pub const OFFSET_FIRE_COUNT: usize = 32;

pub struct Decoded {
    pub flag: u8,
    pub thunk_value: u64,
    pub verdict: u8,
    pub first_r10: u64,
    pub fire_count: u32,
}

pub fn decode(buf: &[u8]) -> Decoded {
    Decoded {
        flag: buf[OFFSET_FLAG],
        thunk_value: u64::from_le_bytes(
            buf[OFFSET_THUNK_VALUE..OFFSET_THUNK_VALUE + 8].try_into().unwrap(),
        ),
        verdict: buf[OFFSET_VERDICT],
        first_r10: u64::from_le_bytes(
            buf[OFFSET_FIRST_R10..OFFSET_FIRST_R10 + 8].try_into().unwrap(),
        ),
        fire_count: u32::from_le_bytes(
            buf[OFFSET_FIRE_COUNT..OFFSET_FIRE_COUNT + 4].try_into().unwrap(),
        ),
    }
}

impl Decoded {
    pub fn verdict_str(&self) -> &'static str {
        match self.verdict {
            0 => "NEVER RECORDED (callback did not fire before the process ran to completion, \
                  or fired but the classification branch never executed)",
            1 => "UNSNAPPED (Task B fires BEFORE the IAT is bound -> viable pre-init vehicle)",
            2 => "SNAPPED (Task B fires AFTER the IAT is bound -> too late for pre-init)",
            _ => "UNKNOWN (unexpected verdict byte - likely a stub bug)",
        }
    }
}
```

- [ ] **Step 3: Wire allocation + patch + write + read-back verify into main.rs**

Replace the body of `main()` in
`scratchpad/spike-b/injector/src/main.rs` — keep the `CreateProcessW` block
from Task 2 exactly as-is, but insert the following BEFORE the
`ResumeThread` call, and add the new imports/modules at the top of the file:

```rust
mod datapage;
mod stub;

use core::ffi::c_void;
use std::env;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Diagnostics::Debug::{
    FlushInstructionCache, ReadProcessMemory, WriteProcessMemory,
};
use windows_sys::Win32::System::Memory::{
    VirtualAllocEx, MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE, PAGE_READWRITE,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, ResumeThread, WaitForSingleObject, CREATE_SUSPENDED,
    INFINITE, PROCESS_INFORMATION, STARTUPINFOW,
};
```

(remove the earlier narrower `use windows_sys::Win32::System::Diagnostics::Debug` /
`Memory` imports from Task 2 if present — Task 2's skeleton didn't import
those modules yet, so this is additive.)

Insert this block into `main()`'s `unsafe {}` region, right after
`println!("target created suspended, pid={}", pi.dwProcessId);` and before the
`ResumeThread` call:

```rust
        // --- Task 3: write the stub + data page into the suspended target ---
        let mut local_stub = stub::stub_bytes();
        let stub_len = local_stub.len();
        println!("extracted stub: {stub_len} bytes");

        let data_remote = VirtualAllocEx(
            pi.hProcess,
            core::ptr::null(),
            datapage::DATA_PAGE_SIZE,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        );
        assert!(!data_remote.is_null(), "VirtualAllocEx(data page) failed");

        stub::patch_data_page_address(&mut local_stub, data_remote as u64);

        let stub_remote = VirtualAllocEx(
            pi.hProcess,
            core::ptr::null(),
            stub_len,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_EXECUTE_READWRITE,
        );
        assert!(!stub_remote.is_null(), "VirtualAllocEx(stub page) failed");

        let mut written = 0usize;
        let ok = WriteProcessMemory(
            pi.hProcess,
            stub_remote,
            local_stub.as_ptr() as *const c_void,
            stub_len,
            &mut written,
        );
        assert!(ok != 0 && written == stub_len, "WriteProcessMemory(stub) failed");

        // Required after writing fresh code into an executable page: the CPU
        // isn't guaranteed to see it without an explicit cache flush.
        FlushInstructionCache(pi.hProcess, stub_remote, stub_len);

        // Verify: read the stub back and diff against the local (patched) copy.
        let mut readback = vec![0u8; stub_len];
        let mut read_n = 0usize;
        let ok = ReadProcessMemory(
            pi.hProcess,
            stub_remote,
            readback.as_mut_ptr() as *mut c_void,
            stub_len,
            &mut read_n,
        );
        assert!(ok != 0 && read_n == stub_len, "ReadProcessMemory(stub verify) failed");
        assert_eq!(readback, local_stub, "stub bytes in target do not match what was written");
        println!(
            "stub written and verified at remote addr {:#x}, data page at {:#x}",
            stub_remote as usize, data_remote as usize
        );
```

Leave the rest of `main()` (the `ResumeThread`/`WaitForSingleObject`/
`GetExitCodeProcess` block) unchanged for this task — the callback isn't
armed yet, so the target just runs and exits normally as in Task 2.

- [ ] **Step 4: Build and run**

```
cargo build -p injector
.\target\debug\injector.exe
```
Expected output (exact addresses will vary):
```
target created suspended, pid=<n>
extracted stub: <N> bytes
stub written and verified at remote addr 0x..., data page at 0x...
target exited with code 42
```
The stub-byte length `N` should be a small number (tens of bytes, not
hundreds) — if it's 0 or absurdly large, the `.global`/label placement in
`stub.rs` is wrong (check that `callback_stub_start`/`callback_stub_end` sit
exactly where expected in the `global_asm!` block).

- [ ] **Step 5: Commit**

```bash
cd C:/oss/vfs
git add scratchpad/spike-b/injector
git -c user.name="Claude" -c user.email="noreply@anthropic.com" commit -m "spike(b): callback stub bytes + cross-process write/verify"
```

---

### Task 4: Arm NtSetInformationProcess, run, decode the verdict

**Files:**
- Modify: `scratchpad/spike-b/injector/src/main.rs`

**Interfaces:**
- Consumes: `stub::stub_bytes`/`patch_data_page_address` and
  `datapage::decode`/`Decoded` from Task 3; the `stub_remote` address and
  `data_remote` address already computed in Task 3's block.
- Produces: the actual spike answer, printed to stdout.

This mirrors the `GetModuleHandleA`/`GetProcAddress` resolution pattern
already proven in `crates/vfs-shim/src/hook.rs` (`make_detour`/`install`) —
same two calls, just resolving `NtSetInformationProcess` instead of an
`NtCreateFile`-family export.

- [ ] **Step 1: Add the ntdll resolution + instrumentation-callback types**

Add to the top of `scratchpad/spike-b/injector/src/main.rs` (alongside the
other imports):
```rust
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
```

Add this function (module-level, outside `main`):
```rust
type NtSetInformationProcessFn = unsafe extern "system" fn(
    process_handle: HANDLE,
    process_information_class: u32,
    process_information: *mut c_void,
    process_information_length: u32,
) -> i32;

#[repr(C)]
struct ProcessInstrumentationCallbackInfo {
    version: u32,
    reserved: u32,
    callback: *mut c_void,
}

const PROCESS_INSTRUMENTATION_CALLBACK: u32 = 40;

/// Resolves `ntdll!NtSetInformationProcess`. Mirrors the
/// GetModuleHandleA/GetProcAddress pattern proven in
/// crates/vfs-shim/src/hook.rs for NtCreateFile-family exports.
unsafe fn resolve_nt_set_information_process() -> NtSetInformationProcessFn {
    let ntdll = GetModuleHandleA(b"ntdll.dll\0".as_ptr());
    assert!(!ntdll.is_null(), "GetModuleHandleA(ntdll.dll) failed");
    let proc = GetProcAddress(ntdll, b"NtSetInformationProcess\0".as_ptr())
        .expect("GetProcAddress(NtSetInformationProcess) failed");
    core::mem::transmute::<_, NtSetInformationProcessFn>(proc)
}
```

- [ ] **Step 2: Arm the callback before resume, decode the data page after**

In `main()`'s `unsafe {}` block, insert this immediately after Task 3's
"stub written and verified" `println!` and BEFORE the existing
`ResumeThread` call:
```rust
        let nt_set_information_process = resolve_nt_set_information_process();
        let mut info = ProcessInstrumentationCallbackInfo {
            version: 1,
            reserved: 0,
            callback: stub_remote,
        };
        let status = nt_set_information_process(
            pi.hProcess,
            PROCESS_INSTRUMENTATION_CALLBACK,
            &mut info as *mut _ as *mut c_void,
            size_of::<ProcessInstrumentationCallbackInfo>() as u32,
        );
        println!("NtSetInformationProcess status = {status:#x}");
```

Then, replace the existing `GetExitCodeProcess`/`println!("target exited...")`
block (still inside the same `unsafe {}`) with this — same exit-code logic,
plus reading and decoding the data page:
```rust
        let mut exit_code: u32 = 0;
        let got = GetExitCodeProcess(pi.hProcess, &mut exit_code);
        assert!(got != 0, "GetExitCodeProcess failed");
        println!("target exited with code {exit_code}");

        let mut data_buf = vec![0u8; datapage::DATA_PAGE_SIZE];
        let mut read_n = 0usize;
        let ok = ReadProcessMemory(
            pi.hProcess,
            data_remote,
            data_buf.as_mut_ptr() as *mut c_void,
            data_buf.len(),
            &mut read_n,
        );
        assert!(ok != 0 && read_n == data_buf.len(), "ReadProcessMemory(data page) failed");
        let decoded = datapage::decode(&data_buf);
        println!(
            "fire_count={} first_r10={:#x} thunk_value={:#x}",
            decoded.fire_count, decoded.first_r10, decoded.thunk_value
        );
        println!("VERDICT: {}", decoded.verdict_str());
        if decoded.fire_count == 0 {
            println!(
                "NOTE: callback never fired. Check the NtSetInformationProcess status \
                 above first (nonzero = install rejected) before concluding anything \
                 about timing."
            );
        }
```

- [ ] **Step 3: Build and run**

```
cargo build -p injector
.\target\debug\injector.exe
```
Expected output shape:
```
target created suspended, pid=<n>
extracted stub: <N> bytes
stub written and verified at remote addr 0x..., data page at 0x...
NtSetInformationProcess status = 0x0
target exited with code 42
fire_count=<k> first_r10=0x... thunk_value=0x...
VERDICT: <UNSNAPPED ...|SNAPPED ...|NEVER RECORDED ...>
```

**Interpreting the result** (this is the actual spike conclusion — record it
in the `vfs-nostd-payload-recipe` memory once observed):
- `NtSetInformationProcess status` nonzero: the struct layout or class number
  is wrong for this Windows build — this is undocumented/research-derived
  (not an official MSDN struct), so if this fails, that's the first thing to
  re-derive, not a target/stub bug.
- `fire_count == 0` with status `0x0`: the callback was accepted but never
  invoked in this configuration before the process exited — also a real
  finding (mechanism doesn't fire the way attempted), not a crash to chase.
- `fire_count > 0` and verdict `UNSNAPPED`: **Task B confirmed viable** —
  fires early enough to beat the IAT snap.
- `fire_count > 0` and verdict `SNAPPED`: **Task B falsified for pre-init** —
  fires, but after binding already happened. Fall back to Task A.
- If `target-exe.exe` crashes instead of exiting with 42 (no output after
  "stub written and verified", or a Windows error dialog / nonzero unexpected
  exit code): the stub's register save/restore is unbalanced or one of the
  hand-computed PE offsets is wrong — this is a stub bug, re-check the
  `global_asm!` block in `stub.rs` against the walkthrough in Task 3's
  description, it is not evidence for or against Task B's viability.

- [ ] **Step 4: Record the finding in memory**

After a successful run (whichever verdict), update the memory note
`vfs-nostd-payload-recipe.md` (at
`C:\Users\tbaldrid\.claude\projects\C--oss-vfs\memory\vfs-nostd-payload-recipe.md`)
replacing the "Next: reflective-map + RIP-redirect (Task A) vs
instrumentation-callback (Task B)..." line with the observed outcome (fire
count, verdict, and the resulting recommendation — proceed with Task B, or
fall back to Task A). Keep the file's frontmatter and existing content
otherwise unchanged; append the new fact as its own paragraph.

- [ ] **Step 5: Commit**

```bash
cd C:/oss/vfs
git add scratchpad/spike-b/injector
git -c user.name="Claude" -c user.email="noreply@anthropic.com" commit -m "spike(b): arm NtSetInformationProcess, decode verdict"
```

(The memory-file edit in Step 4 is outside the git repo and is not part of
this commit.)

*End of plan.*

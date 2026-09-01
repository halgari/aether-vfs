# Closing the NtQueryObject identity leak — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A redirected handle must report its **virtual** path through
`NtQueryObject(ObjectNameInformation)`, not the backing file it was redirected
to.

**Architecture:** One new detour on `NtQueryObject`, answering only
`ObjectNameInformation` for handles the shim already tracks, and passing
everything else straight through. The virtual path comes from `PATH_TABLE`,
which already maps handle → the NT path the caller opened.

**Spec:** `docs/superpowers/specs/2026-09-01-wine-hosted-shim-design.md` §6.

## Why this is not a Wine bug

`GetFinalPathNameByHandleW` resolves a handle differently on the two hosts.
Measured 2026-09-01 with a purpose-built probe:

| host | `GetFinalPathNameByHandleW` calls | `NtQueryObject` returns |
|---|---|---|
| Windows | `NtQueryInformationFile(FileNormalizedNameInformation)` — **hooked** | `\Device\HarddiskVolume3\vfstmp\objprobe\target.txt` |
| Wine | `NtQueryObject(ObjectNameInformation)` — **not hooked** | `\??\C:\probe\objprobe\target.txt` |

So this is a **pre-existing identity leak on Windows** that Wine merely exposed.
On Windows the leak hides because `GetFinalPathNameByHandleW` happens to take a
hooked route, but any program calling `NtQueryObject` directly on a redirected
handle gets the backing path today, silently, with nothing asserting otherwise.
That is the same shape as the `NtLockFile` gap that broke all of Skyrim's INI
loading undetected: an unhooked handle-taking NT API does not error, it answers
wrong.

**The fix is therefore NOT `cfg`-gated to Linux.** It changes Windows behaviour
deliberately, which makes it unlike every other change in this port so far.

## The measurement that determines the design

A hook cannot emit one fixed format. Also measured, and this rules out the
obvious approach:

| host | `NtQueryObject` prefix | `QueryDosDeviceW("C:")` | agree? |
|---|---|---|---|
| Windows | `\Device\HarddiskVolume3` | `\Device\HarddiskVolume3` | yes |
| Wine | `\??\C:` | `\Device\HarddiskVolume1` | **no** |

Building a device path from `QueryDosDeviceW` would emit, on Wine, a form Wine
never produces natively. So the hook **calls the trampoline first and adopts the
prefix convention the host actually used**, rather than assuming one.

## Global Constraints

- **`NtQueryObject` is general-purpose.** It answers about events, mutexes,
  sections, registry keys, threads — not just files. Every handle the shim does
  not recognise, and every info class other than `ObjectNameInformation`, MUST
  reach the trampoline unmodified. Getting this wrong breaks unrelated Windows
  APIs that a file-focused suite will not catch.
- **Never invent a name for a handle we do not track.** If `PATH_TABLE` has no
  entry, pass through. A wrong name is worse than the real one.
- **This is a deliberate Windows behaviour change.** Do not claim "no behaviour
  change on Windows" anywhere; claim the suite is green, which is different.
- `cargo clippy --all-targets -- -D warnings` must pass.
- Every `unsafe` block needs `// SAFETY:` and `#[allow(unsafe_code)]`, matching
  the surrounding style in `hook.rs`.
- **Verify at workspace scope**, not crate scope: `cargo test --no-fail-fast`
  for the whole workspace. A crate-scoped run missed a workspace lint one
  increment ago.
- **Never read `$?` after a pipeline** and never judge a test result through
  `tail`. Both have produced wrong answers in this project.
- Linux/Wine verification uses the Arch WSL box; pipe scripts via **stdin** to
  `bash -s` with `MSYS_NO_PATHCONV=1`, and remember the clone sees only
  **committed** work.

---

### Task 1: Hook `NtQueryObject`

**Files:**
- Modify: `rust/crates/vfs-shim/src/hook.rs`
- Modify: `rust/crates/vfs-redirect/src/lib.rs` (only if the `NtQueryObjectFn`
  signature belongs beside the other NT function types — check where
  `NtQueryInformationFileFn` is declared and follow it)
- Test: `rust/crates/vfs-shim/tests/identity_objectname.rs` (new)

**Interfaces:**
- Consumes: `PATH_TABLE` (handle → virtual NT path, e.g. `\??\C:\root\mod.esp`),
  `TRAMP_*` conventions, `make_detour`, `note_skipped_detour`.
- Produces: `TRAMP_QOBJ`, `qobj_hook`, and a new entry in the detour list.

- [ ] **Step 1: Establish the too-small-buffer contract by measurement**

Do NOT guess what `NtQueryObject` returns when the buffer is too small; the hook
must reproduce it exactly or callers that size-probe will loop or fail. A probe
already exists at
`C:\Users\tbaldrid\AppData\Local\Temp\claude\C--oss-aether-vfs\aa080c90-3978-4a4d-a523-e12a31af9e04\scratchpad\objname\`.
Extend it to call `NtQueryObject` with a deliberately tiny buffer (say 8 bytes,
and again with exactly 16), and run it **on Windows and under Wine**. Record for
each: the `NTSTATUS`, and whether `ret_len` was set to the required size.

Wine invocation:
```
R=/root/aether/runtimes/GE-Proton11-6-x86_64
export WINEPREFIX=/root/aether/probe-prefix WINEDLLOVERRIDES="mscoree=d;mshtml=d" WINEDEBUG=-all
$R/files/bin/wine objname.exe 'C:\probe\objprobe\target.txt'
```

Write the measured statuses into your report AND into a comment above the hook.
If the two hosts disagree, the hook mirrors whatever the trampoline just did —
see Step 3.

- [ ] **Step 2: Write the failing test**

`rust/crates/vfs-shim/tests/identity_objectname.rs`. This runs on **Windows**,
because that is where the leak is reachable today and untested:

```rust
//! A redirected handle must not leak its backing path through NtQueryObject.
//!
//! `GetFinalPathNameByHandleW` takes different routes on different hosts —
//! `NtQueryInformationFile` on Windows, `NtQueryObject` on Wine — so a shim that
//! only spoofs the first answers correctly on one host and leaks on the other.
#![cfg(windows)]

use vfs_shim::{install, Engine};
use windows_sys::Win32::Foundation::HANDLE;

#[link(name = "ntdll")]
extern "system" {
    fn NtQueryObject(h: isize, class: i32, info: *mut u8, len: u32, ret: *mut u32) -> i32;
}
const OBJECT_NAME_INFORMATION: i32 = 1;

fn object_name(h: HANDLE) -> String {
    let mut buf = vec![0u8; 4096];
    let mut ret = 0u32;
    let st = unsafe {
        NtQueryObject(h as isize, OBJECT_NAME_INFORMATION, buf.as_mut_ptr(), buf.len() as u32, &mut ret)
    };
    assert_eq!(st, 0, "NtQueryObject failed: 0x{st:08x}");
    let len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
    let chars: Vec<u16> = buf[16..16 + len]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&chars)
}

#[test]
fn a_redirected_handle_reports_its_virtual_name_not_the_backing_one() {
    let pid = std::process::id();
    let root = std::env::temp_dir().join(format!("vfs-objname-{pid}"));
    let backing_dir = std::env::temp_dir().join(format!("vfs-objname-backing-{pid}"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&backing_dir).unwrap();
    let backing = backing_dir.join("backing_blob.dat");
    std::fs::write(&backing, b"the-real-bytes").unwrap();
    let vfile = root.join("mod.esp");

    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "mod.esp".into(),
                kind: EntryKind::File,
                source: backing.to_str().unwrap().into(),
                size: 14,
                mtime: 0,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };
    let engine = Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let _guard = install(engine).expect("install");

    use std::os::windows::io::AsRawHandle;
    let f = std::fs::File::open(&vfile).expect("open redirected virtual file");
    let name = object_name(f.as_raw_handle() as HANDLE).to_lowercase();

    assert!(name.contains("mod.esp"), "must report the VIRTUAL name: {name}");
    assert!(
        !name.contains("backing_blob"),
        "must NOT leak the backing name: {name}"
    );
}

#[test]
fn an_untracked_handle_is_untouched() {
    // NtQueryObject answers about events, mutexes and keys too. A handle the
    // shim knows nothing about must pass through with the host's own answer, or
    // the hook breaks unrelated Windows APIs.
    let pid = std::process::id();
    let plain_dir = std::env::temp_dir().join(format!("vfs-objname-plain-{pid}"));
    std::fs::create_dir_all(&plain_dir).unwrap();
    let plain = plain_dir.join("ordinary.txt");
    std::fs::write(&plain, b"x").unwrap();

    // Capture the answer with NO shim installed.
    use std::os::windows::io::AsRawHandle;
    let before = {
        let f = std::fs::File::open(&plain).unwrap();
        object_name(f.as_raw_handle() as HANDLE)
    };

    let root = std::env::temp_dir().join(format!("vfs-objname-plain-root-{pid}"));
    std::fs::create_dir_all(&root).unwrap();
    let snapshot = {
        use vfs_core::{build, Layer, LayerId};
        let tree = build(vec![Layer { id: LayerId(0), entries: vec![] }]).unwrap();
        vfs_shared::bridge::flatten(&tree)
    };
    let engine = Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let _guard = install(engine).expect("install");

    let after = {
        let f = std::fs::File::open(&plain).unwrap();
        object_name(f.as_raw_handle() as HANDLE)
    };
    assert_eq!(before, after, "an untracked handle must be answered unchanged");
}
```

Note: the two tests each call `install`, which patches process-global
trampolines. Check how `tests/hook_coverage.rs` handles this — if `install` is
one-shot per process, these must be **separate test binaries**, not two `#[test]`
functions in one file. Split them if so, and say in your report which you did.

- [ ] **Step 3: Run to verify failure, then implement**

Expected failure: the first test reports `backing_blob.dat`.

The hook:

```rust
/// `NtQueryObject` hook. Answers `ObjectNameInformation` (class 1) for a handle
/// the shim redirected -> the VIRTUAL path, in the prefix convention this host
/// actually uses. Every other class, and every handle we do not track, passes
/// through untouched: this API answers about events, mutexes, sections and
/// registry keys too, and inventing a name for one of those would break
/// unrelated Windows APIs.
///
/// Why the convention is discovered rather than assumed: measured 2026-09-01,
/// Windows returns `\Device\HarddiskVolume3\...` while Wine returns `\??\C:\...`,
/// and `QueryDosDeviceW("C:")` reports `\Device\HarddiskVolume1` on Wine — it
/// disagrees with Wine's own `NtQueryObject`. So building a device path from it
/// would emit a form Wine never produces. Instead the trampoline runs first and
/// its answer's prefix is reused.
```

Sequence:

1. If class is not `ObjectNameInformation`, tail-call the trampoline.
2. Look up `PATH_TABLE[handle]`. If absent, tail-call the trampoline. Do this
   **before** any allocation or scratch call — an untracked handle must cost
   nothing but a map lookup.
3. Call the trampoline into a scratch buffer to obtain the host's real answer.
   If it fails, return its status unchanged — do not substitute success.
4. Derive the emitted name:
   - real answer starts with `\??\` → emit `\??\` + the DOS portion of the
     virtual path;
   - real answer starts with `\Device\` → resolve the **virtual** path's drive
     letter with `QueryDosDeviceW` and emit `<device>\<rest>`. If that lookup
     fails, pass the trampoline's answer through unchanged rather than guessing.
   - anything else → pass through unchanged.
5. Write `OBJECT_NAME_INFORMATION` into the caller's buffer: a `UNICODE_STRING`
   whose `Length` is the byte length excluding NUL, `MaximumLength` includes it,
   and `Buffer` points **16 bytes into the caller's own buffer** (both hosts do
   this — measured). Then the UTF-16 name, NUL-terminated.
6. Set `ret_len` if non-null. If the caller's buffer is too small, reproduce the
   status Step 1 measured, and still set `ret_len` to the required size.

Register the detour beside the others in `install_all_detours`, add
`TRAMP_QOBJ`, push it into the keep-alive `detours` vector, and — since a host
may lack the export — make it optional in the same style as
`NtQueryDirectoryFileEx`, calling `note_skipped_detour("NtQueryObject")` when it
does not install. `NtQueryObject` is present on both hosts (verified: the probe
resolved it under Wine), so `skipped_detours()` should stay empty and
`tests/hook_coverage.rs` must still pass.

- [ ] **Step 4: Verify**

Workspace scope: `cargo test --no-fail-fast` (with `TMP=C:\vfstmp` and
`TEMP=C:\vfstmp`) and `cargo clippy --all-targets -- -D warnings`. Capture to a
file; read the tally from it.

`tests/hook_coverage.rs` asserting `skipped_detours()` is empty is now a
**stronger** check — confirm it still passes rather than adjusting it.

- [ ] **Step 5: Commit**

```bash
git add rust/crates/vfs-shim
git commit -m "fix(shim): close the NtQueryObject identity leak on both hosts"
```

---

### Task 2: Prove it under Wine

**Files:** none — this is verification.

- [ ] **Step 1: Rebuild and re-run the Wine identity probe**

`tests/hook_identity.rs` failed under Wine at its final assertion, reporting the
backing name through `GetFinalPathNameByHandleW`. It should now pass unchanged.

Build it on Windows, copy into the prefix, run under GE-Proton:

```bash
cargo test -p vfs-shim --test hook_identity --no-run     # note the exe path it prints
```

then, in the Arch box, with the exe copied to `$WINEPREFIX/drive_c/probe`:

```
R=/root/aether/runtimes/GE-Proton11-6-x86_64
export WINEPREFIX=/root/aether/probe-prefix WINEDLLOVERRIDES="mscoree=d;mshtml=d" WINEDEBUG=-all
timeout 300 $R/files/bin/wine hook_identity.exe --nocapture --test-threads=1
```

Filter Wine's noise with
`grep -viE 'freetype|equal to 2.0.5|www.freetype|fixme:|err:winediag|wineserver:'`.

Expected: `test redirected_file_reports_virtual_path ... ok`, and the run's exit
code 0 — captured **without** reading `$?` after a pipe.

- [ ] **Step 2: Report**

Record the literal output. If it still fails, that is a real finding and the
diagnosis matters more than a fix: say exactly which assertion failed and what
the reported path was.

---

## Self-Review

**Spec coverage.** §6's identity gap is Tasks 1-2. The spec says the work is to
"trace which call Wine actually makes and cover it" — the trace is done
(`NtQueryObject`, class 1) and this plan covers it.

**Type consistency.** `TRAMP_QOBJ` follows the `TRAMP_*` naming of every other
trampoline; `qobj_hook` follows `qif_hook`/`qdirex_hook`. `PATH_TABLE` is the
existing handle → NT path map, not a new one.

**Known soft spots.** Step 3's hook body is prose plus an exact 6-step sequence,
not complete code, because the surrounding macro-generated hook declarations in
`hook.rs` must be matched by reading them. Step 1 is a genuine unknown that the
implementer must measure rather than a step I could pre-answer. And the
`install`-is-process-global question in Step 2 could force splitting one test
file into two binaries; the step says to check and report which.

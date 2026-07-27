# Dual-Layer Inject Handoff — Implementation Plan

**Status: IMPLEMENTED (2026-07-14).** Production path = spin-gate dual-layer
(see design §5). All vfs-inject integration tests green.

> **For agentic workers:** Use subagent-driven-development or executing-plans.
> Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Unify director injection so every target gets pre-init early hooks
(static EXE imports) **and** the full Engine without double-patching the four
ntdll path/attr stubs.

**Design:** `docs/superpowers/specs/2026-07-14-dual-layer-inject-handoff-design.md`

**Tech stack:** existing `vfs-payload`, `vfs-inject`, `vfs-shim`, `vfs-shim-dll`,
windows-sys 0.59, stable Rust.

---

## Global constraints

- Early payload remains zero-import / `no_std`; secondary pointers only, no
  Engine inside the payload.
- Full shim must **never** `make_detour` the four early stubs when
  `install_late` is used.
- In-process `install(engine)` keeps detouring all stubs (tests).
- Do not use `CreateRemoteThread(LoadLibrary)` as the *first* runnable thread
  in the target for the unified path.
- Backing files for the early redirect table stay outside the managed root.
- Framing: game modding, not security research.

---

### Task 1: Payload secondary dispatch

**Files:**
- Modify: `crates/vfs-payload/src/lib.rs`
- Modify: `crates/vfs-inject/src/payload_cfg.rs` (host mirror must match)

**Steps:**

- [ ] **1.1** Add to `Config` / `PayloadConfig` (all `usize`, default 0):

```text
secondary_open, secondary_create, secondary_qattr, secondary_qfull
```

Place after existing fields (or document exact layout once and update both
sides in the same commit).

- [ ] **1.2** In each of the four hooks, after redirect-table match fails:

```rust
if c.secondary_open != 0 {
    let f: NtOpenFileFn = transmute(c.secondary_open);
    return f(...);
}
// else trampoline
```

(Analogous for create/qattr/qfull.)

- [ ] **1.3** `cargo build -p vfs-payload` succeeds; static_import test still
  passes with secondaries left zero.

- [ ] **1.4** Commit: `vfs-payload: secondary dispatch slots for full-shim handoff`

---

### Task 2: Full shim `install_late` + sync bootstrap export

**Files:**
- Modify: `crates/vfs-shim/src/hook.rs`
- Modify: `crates/vfs-shim/src/bootstrap.rs`
- Modify: `crates/vfs-shim/src/lib.rs`
- Modify: `crates/vfs-shim-dll/src/lib.rs`
- Possibly small `payload_cfg` duplicate or shared constants in shim for Config
  layout offsets when writing secondaries (prefer a tiny `#[repr(C)]` mirror
  in shim or a `vfs-payload-abi` header module — avoid depending on the cdylib
  as an rlib if panic/no_std conflicts; a duplicated `#[repr(C)]` struct in
  shim is acceptable if tested for `size_of`/`offset_of` match).

**Steps:**

- [ ] **2.1** Extract detour install into helpers, e.g. `install_early_owned`
  (the four) vs `install_remainder` (qdirex, close, qif, setinfo, cpiw).

- [ ] **2.2** `install(engine)` = set ENGINE + early-owned + remainder
  (current behavior).

- [ ] **2.3** `install_late(engine, cfg: *mut PayloadConfigMirror)` =
  set ENGINE + write four hook fn pointers into `cfg` + `install_remainder`
  only. Do **not** call `make_detour` on the four.

- [ ] **2.4** `bootstrap_from_config_path` gains optional cfg pointer parameter
  or reads dual-layer from a thread-local/arg set by the DLL export.

- [ ] **2.5** DLL export:

```rust
#[no_mangle]
pub unsafe extern "system" fn vfs_shim_sync_bootstrap(payload_cfg: *mut c_void) -> u32
```

Reads `VFS_SHIM_CONFIG`, builds Engine, calls `install_late` if
`payload_cfg` non-null else `install`, writes ready marker, returns 0/err.

- [ ] **2.6** `DllMain`: if env `VFS_DUAL_LAYER` is set, **do not** spawn
  bootstrap thread (late-entry will call sync export). Else keep spawn
  (classic path / safety net).

- [ ] **2.7** `cargo test -p vfs-shim` — all hook_* tests pass via full
  `install`.

- [ ] **2.8** Commit: `vfs-shim: install_late + sync bootstrap for dual-layer`

---

### Task 3: Late-entry OEP gate in vfs-inject

**Files:**
- Create: `crates/vfs-inject/src/oep_gate.rs`
- Modify: `crates/vfs-inject/src/inject.rs`
- Modify: `crates/vfs-inject/src/lib.rs`
- Modify: `crates/vfs-inject/Cargo.toml` (features already cover CONTEXT /
  memory / library loader)

**Steps:**

- [ ] **3.1** Implement remote PEB read → ImageBase (NtQueryInformationProcess
  class ProcessBasicInformation, or documented PEB offset via
  `NtQueryInformationProcess` + `ReadProcessMemory`). Prefer a proven
  windows-sys path; document the PEB offset used (x64 `0x10` ImageBase is
  under PEB, address of PEB from PROCESS_BASIC_INFORMATION).

- [ ] **3.2** Local or remote PE parse: `AddressOfEntryPoint` → `real_oep`.

- [ ] **3.3** `build_late_stub(data)` PIC/absolute stub:

  1. `LoadLibraryW(dll_path)`  
  2. `GetProcAddress(h, "vfs_shim_sync_bootstrap")`  
  3. `call bootstrap(cfg_remote)`  
  4. Restore original OEP bytes  
  5. `jmp real_oep`

  Pass LoadLibraryW / GetProcAddress / dll path / cfg_remote / real_oep /
  saved bytes via a remote data page (injector-resolved k32 addresses).

- [ ] **3.4** `arm_oep_gate(process, dll_path, cfg_remote) -> Result<(), InjectError>`

- [ ] **3.5** Unit-test pure PE helpers with a fixture binary if cheap;
  otherwise cover via integration test in Task 5.

- [ ] **3.6** Commit: `vfs-inject: OEP late-entry gate for post-init full shim`

---

### Task 4: Unify `run_target_with_shim`

**Files:**
- Modify: `crates/vfs-inject/src/lib.rs` (`RunConfig`)
- Modify: `crates/vfs-inject/src/inject.rs`
- Modify: `crates/vfs-inject/tests/end_to_end.rs`
- Modify: `crates/vfs-inject/tests/acceptance.rs` (and any bin helpers)

**Steps:**

- [ ] **4.1** Extend `RunConfig`:

```rust
pub struct RunConfig {
    // existing fields…
    pub payload_path: String,              // vfs_payload.dll
    pub preinit_redirects: Vec<PreinitRedirect>, // may be empty
}
```

- [ ] **4.2** New flow for `run_target_with_shim`:

```text
set env VFS_SHIM_CONFIG, VFS_SHIM_READY, VFS_DUAL_LAYER=1
CreateProcess SUSPENDED
arm_preinit_payload(...) -> cfg_remote
arm_oep_gate(..., dll_path, cfg_remote)
ResumeThread
// optional: poll ready marker after resume (sync bootstrap should set it)
WaitForSingleObject process
return exit code
```

Remove the pre-resume `CreateRemoteThread(LoadLibrary)` + ready-wait from this
path (that was the loader-waking sequence).

- [ ] **4.3** Update e2e/acceptance to pass `payload_path` (locate
  `vfs_payload.dll` like other artifacts) and empty redirects (unless testing
  static imports).

- [ ] **4.4** Keep `run_target_with_preinit` for payload-only static_import test.

- [ ] **4.5** `cargo test -p vfs-inject` — e2e + acceptance + static_import green.

- [ ] **4.6** Commit: `vfs-inject: dual-layer run_target_with_shim`

---

### Task 5: Dual-layer acceptance test (static + virtual)

**Files:**
- Create: `crates/vfs-inject/tests/dual_layer.rs`
- Reuse fixtures: `vfs-staticimp`, `vproxy.dll`, `vfs_payload.dll`, probe or
  staticimp + a virtual file read if staticimp is extended.

**Preferred fixture approach:**

- Launch `vfs-staticimp` from an isolated app dir (no vproxy on disk) with
  early redirect for `vproxy.dll`.
- Also pass a full shim config whose snapshot redirects a virtual data file;
  extend staticimp **or** use a small new bin that (1) calls `vproxy_value()`
  and (2) reads a virtual path — cleanest proof in one process.

**Steps:**

- [ ] **5.1** Add bin or extend fixture to exercise both static import and
  `std::fs::read` of a virtual path.

- [ ] **5.2** Test uses unified `run_target_with_shim` with non-empty
  `preinit_redirects` + normal config snapshot.

- [ ] **5.3** Asserts: exit 0, `vproxy_value=4242`, virtual file bytes correct,
  no `vproxy.dll` in app dir.

- [ ] **5.4** Commit: `test(vfs-inject): dual-layer static import + virtual file`

---

### Task 6: Docs + child-path note

**Files:**
- Modify: `docs/superpowers/specs/2026-07-14-preinit-injection-design.md`
  (pointer to dual-layer as the unified director path)
- Modify: `crates/vfs-shim/src/inject.rs` comment (child still LoadLibrary;
  dual-layer is next)
- Memory: `vfs-nostd-payload-recipe.md` — dual-layer landed when green

**Steps:**

- [ ] **6.1** Cross-link design docs; mark unified path as default director.
- [ ] **6.2** Explicit child follow-up: same handoff, blocked only on wiring
  payload path into CPIW inject.
- [ ] **6.3** Final `cargo test -p vfs-inject -p vfs-shim` (plus build
  fixtures).

---

## Verification checklist (end of plan)

```
cargo build -p vfs-payload -p vfs-fixture-vproxy -p vfs-fixture-staticimp
cargo test -p vfs-shim
cargo test -p vfs-inject
```

Expect:

- [ ] All vfs-shim hook tests pass (full `install`)
- [ ] static_import pass
- [ ] end_to_end + acceptance pass on dual-layer `run_target_with_shim`
- [ ] dual_layer test pass (static + virtual)
- [ ] No double-patch: secondary path used when cfg present (optional counter)

---

## Implementation order rationale

1. Payload secondary first — safe, tests still work with zeros.  
2. Shim install_late + sync export — in-process tests protect remainder detours.  
3. OEP gate — pure inject machinery.  
4. Unify director — wires 1–3.  
5. Dual-layer test — proves the design criterion.  
6. Docs — record the default path.

## Risk notes

- **PEB/OEP parsing** is the fiddliest Win32 piece; isolate and assert image
  base ≠ 0 before writing.
- **DllMain vs sync bootstrap:** dual-layer must not run both.
- **Config layout drift** between payload and host/shim mirrors — add a
  `const _: () = assert!(size_of::<PayloadConfig>() == …)` test on the inject
  side once sizes stabilize.
- **Workspace `panic = "abort"`** already required by payload; leave as-is.

*End of plan.*

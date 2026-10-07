# aether-vfs cleanup audit: Windows / in-process side

Scope: `crates/vfs-shim` (src + tests), `vfs-shim-dll`, `vfs-payload`, `vfs-inject`, `vfs-pe`, `vfs-redirect`.
Repo: `/home/tbaldrid/oss/haskill/external/aether-vfs/rust`, master `fcdf874`. Read-only audit; nothing was edited.

## How this was checked

- I read `docs/architecture.md` and all of `hook.rs`. I outlined and spot-read the other modules.
- `cargo xwin check --target x86_64-pc-windows-msvc -p vfs-shim -p vfs-shim-dll -p vfs-inject -p vfs-pe -p vfs-redirect --all-targets` is clean: **0 rustc warnings**, so there is no compiler-visible dead code. With a warm cache it takes about 1.5 s.
  - It needs `CFLAGS_x86_64-pc-windows-msvc=-Wno-error=implicit-function-declaration` in the environment. Without it, `libudis86-sys` fails to build under clang-cl. The README does not mention this.
- `cargo xwin clippy` on the same target was run with `-W clippy::undocumented_unsafe_blocks -W unreachable_pub -W unsafe_op_in_unsafe_fn`. Its counts are quoted below.
- `cargo +stable test -p vfs-redirect --no-run` on Linux **fails to compile** (see S5).
- `cargo fmt --check` reports drift (see S22).

Value, effort and risk scales: Value is H/M/L. Effort is S (< ½ day), M (1–3 days) or L (> 3 days). Risk is Low/Med/High, with the reason given.

---

## Tier 1: high value

### S1. Split `hook.rs` (7,724 lines) by concern
**Category:** oversized module

**Evidence.** One file holds all of the following:
- the panic and entry machinery (`hook.rs:9-655`, `930-1012`);
- 60 `static mut` trampolines and the install code (`733-1361`, `1368-1761`);
- 37 registry hook bodies (`1763-2590`);
- path decoding (`2592-2790`);
- the handle tables (`2844-2955`);
- open/create (`3029-4069`);
- stat-by-path (`4071-4372`);
- close (`4374-4465`);
- delete/setinfo (`4467-4975`);
- the query-info family (`4977-5370`, `5545-5916`);
- lock/flush (`5372-5543`);
- read/write (`5918-6130`);
- sections (`6132-6528`);
- process creation (`6530-6592`);
- directory enumeration (`6594-6965`);
- 760 lines of tests (`6967-7724`).

**Proposed layout** (`src/hook/`):

| module | contents (current lines) | approx. size |
|---|---|---|
| `mod.rs` | `ENGINE`, re-exports, `allow_disk_fallthrough`, `child_cwd_root` (657-677, 742) | 100 |
| `entry.rs` | `ShimIoGuard`, `in_hook_reenter`, `STATUS_HOOK_PANICKED`, `contain_panic`, `hook_entry_points!` and its invocation, `install_panic_hook` (1-655, 930-1012) | 700, or about 350 once S11 is done |
| `install.rs` | trampoline storage, `make_detour`, `optional_detour`, `detour_if_present`, `install`, `install_late`, `install_all_detours`, `install_registry_detours`, `SKIPPED_DETOURS`, `REG_DETOURS_INSTALLED` (733-1361, 1368-1761) | 1,000 now, about 200 after S2 |
| `path.rs` | `object_name_str`, `oa_name_only`, `cwd_from_peb`, `parent_dir_of_handle`, `DecodedPath`, `path_of_tracked`, `path_of`, `decision_for`, `path_is_ours`, `parse_rename_target`, `to_nt_path`, `fuse_root_directory` | 400 |
| `handles.rs` | `DIR_TABLE`, `IDENTITY_TABLE`, `PATH_TABLE`, `HANDLE_PATHS`, `tag_under_root`, `record_identity`, `record_path`, `path_of_handle`, `synth_path`, `open_synth`, and close-time reclamation | 250 |
| `close.rs` | `close_hook_body` (shared by file, section and registry) | 100 |
| `file_open.rs` | create/open hooks, `try_fuse_create`, `try_fuse_mkdir`, `tramp_*_abs`, `note_*_outcome`, `drm_exe_trace`, `director_open_trace` (the disposition predicates go to S5) | 900 |
| `file_attr.rs` | qattr, qfull, qibn, `fuse_path_attr`, `fill_by_name` | 300, or 120 after S6 |
| `file_info.rs` | qif, `fuse_query_information`, qvol, qobj, `emit_object_name`, `spoofed_object_name`, `device_for_drive`, `host_name_convention`, `host_is_wine`, `synth_final_path`, `synth_file_id`, `path_file_id`, `SYNTH_VOLUME_SERIAL` | 900 |
| `file_io.rs` | read, write, lock, unlock, flush | 450 |
| `file_mutate.rs` | `delete_hook`, `setinfo_hook`, `delete_status_for`, `tramp_delete_abs`, `setinfo_source_path`, `is_delete_request` | 550 |
| `dirquery.rs` | qdir, qdirex, `serve_dir_query`, `wildcard_of`, `EnumState`, `DirTracked` | 400, or 250 after S11 |
| `section.rs` | create_section, map_view, unmap_view, `real_image_section`, `fuse_create_section` | 400 |
| `process.rs` | `cpiw_hook_body`, `CreateProcessInternalWFn`, `SELF_DLL`, `CHILD_READY_TIMEOUT_MS` | 100 |
| `registry.rs` | `reg_real`, `reg_bypass`, every registry hook body, `out_of_scope_body!`, `served_*`, `refusal` | 830, or about 400 after S13 |

Tests move next to the module they test.

**Value:** H. **Effort:** L (mostly mechanical). **Risk:** Med, for three reasons:
- `no_extern_hook_bypasses_the_panic_containment_macro` (`hook.rs:7605-7723`) asserts against `include_str!("hook.rs")` and requires the macro to be "the only place in `hook.rs`" that defines an `extern "system"` fn. That check must be rewritten to scan `src/hook/**`.
- The `TRAMP_*` statics and tables need `pub(super)` visibility.
- Do it as move-only commits, one module per commit, with no behaviour change, after S2 and S4. Otherwise the duplicated code moves twice.

### S2. Drive the 60 trampolines, the install code and hookstats from one detour table
**Category:** unsafe hygiene and duplication

**Evidence:**
- 60 `static mut TRAMP_*: Option<Fn>` (`hook.rs:744-816`).
- The file install code repeats this four-line sequence 20 times: `make_detour` + `transmute` + store + enable (`1074-1266`).
- The "optional export" sequence is hand-inlined three times (`1119-1133` QDIREX, `1203-1217` QIBN, `1226-1240` QOBJ), even though `optional_detour` (`1311`) exists for exactly that job.
- The registry install code repeats each name twice, `c"NtQueryKey", "NtQueryKey"`, 15 times (`1388-1588`), then 22 `if_present!` calls (`1606-1759`).
- The same NT names also live in:
  - `hook_entry_points!` (`192-655`, the `as "NtCreateFile"` strings);
  - `hookstats::Hook` and the parallel `NAMES` array (`hookstats.rs:22-142`, `N = 57`), which already disagree with the hook list (unmap_view and cpiw have no `Hook` variant);
  - each body's `Timed::new(Hook::X)`.
- clippy `unsafe_op_in_unsafe_fn` counts 95 "use of mutable static" sites in `hook.rs`.

**Proposed change.**
- Add a `Tramp<F: Copy>` type: an `AtomicUsize` with `set(Option<F>)` and `get() -> Option<F>`, using `transmute_copy`. It is sound for fn pointers, avoids `static mut` and `static_mut_refs`, and keeps the "store before enable, clear on enable failure" rule in one place (see the comment at `1116-1118`).
- Add a `detours!` table with rows like `NtCreateFile => create_hook / create_hook_body (args...) -> NTSTATUS, tramp: TRAMP_CREATE, kind: Required | Optional | IfPresent, group: File | Registry, stat: Hook::Create`.
- Generate from that table: the extern wrappers (replacing `hook_entry_points!`), the `Tramp` statics, the install loop, the `Hook` enum, `NAMES` and `N`.
- `reg_real()` (`1772`) then becomes a cheap read of those statics.

**Value:** H. It removes about 900 lines and every `static mut`, and makes name drift between the four lists impossible.
**Effort:** M.
**Risk:** Med, for three reasons:
- the install order and the `install_late` early-owned four (`1039-1059`) must be preserved;
- the containment test's source scan must accept the new macro;
- this touches every hook prologue, so the full Wine test run is required.

### S3. One registry handle resolver, plus one `with_real`/`gone`/`check`
**Category:** duplication

**Evidence.** The step "synthetic? → `regkeys::synthetic`; else `resolve` / `resolve_for_read`; then map deleted and unresolvable" is written six times:
- `regquery::classify` (`regquery.rs:98-153`, which also does `regclient::lookup`);
- `regwrite::target` (`regwrite.rs:87-117`);
- `regnotify::notify` (`regnotify.rs:194-205`);
- `regkeys::serves_handle` (`regkeys.rs:1581-1592`);
- `regkeys::set_security` (`regkeys.rs:1505-1528`);
- `regkeys::query_security` (`1463`, synthetic only).

The access check `check` exists twice, identically (`regquery.rs:155`, `regwrite.rs:119`). So do:
- `gone` (`regquery.rs:178` as a closure, `regwrite.rs:177`);
- `with_real` (`regquery.rs:169-206`, `regwrite.rs:186-220`). These differ only in where `rights` comes from: hard-coded `KEY_QUERY_VALUE | KEY_ENUMERATE_SUB_KEYS` for reads, a parameter for writes.

`DELETE` is defined in both `regkeys.rs:87` and `regwrite.rs:53`.

**Proposed change.**
- In `regkeys`, add:
  - `enum KeyHandle { Synthetic(SynthKey), Pass(KeyRec), NotOurs, Invalid, Unresolvable }`;
  - `unsafe fn key_handle(real, h, Mode::Read | Mode::Write) -> KeyHandle`. It puts the read-fallback counter and the write-refused counter in one place.
- Add a shared `KeyRef { handle, path, access, real, wow64, deleted, synthetic }` with `check(right)` and `with_real(real, rights, f)`.
- `regquery::Ctx` and `regwrite::Target` collapse into it.
- `regquery::classify` keeps only its lookup and merge decision.

**Value:** H. The spec's fail-closed rules for writes and fall-back rules for reads would then be enforced in exactly one place.
**Effort:** M.
**Risk:** Med. The read and write modes count different things, and `classify` checks `right` only on the synthetic Err arm (`regquery.rs:108`). Port the existing `tests/reg*.rs` unchanged and keep them green.

### S4. Shared raw-buffer helpers for `UNICODE_STRING`, `OBJECT_ATTRIBUTES` and `IO_STATUS_BLOCK`
**Category:** unsafe hygiene and duplication

**Evidence.**
- **Six `UNICODE_STRING` decoders, with subtly different rules** (NULL buffer with length 0, odd length):
  - `hook.rs:2592` `object_name_str`;
  - `hook.rs:6596` `wildcard_of`;
  - `regquery.rs:484` `read_us`;
  - `regwrite.rs:162` `units`, which returns `STATUS_ACCESS_VIOLATION` for a NULL buffer with nonzero length;
  - `regkeys.rs:855` (`open_or_create`);
  - `regkeys.rs:1612` (`serves_target`).

  `hook.rs:2724` `oa_name_only` is a redundant wrapper around `object_name_str`; both null-check.
- **Six hand-built absolute `OBJECT_ATTRIBUTES`:**
  - `hook.rs:3788` (create redirect);
  - `4023` (open redirect);
  - `3878` (`tramp_create_abs`);
  - `3920` (`tramp_open_abs`);
  - `4599` (`tramp_delete_abs`);
  - `regkeys.rs:466` (`AbsName`, already a boxed owned version).

  They are also inconsistent: the two redirect arms set `maximum_length == length` with no NUL, while the `_abs` versions NUL-terminate.
- **About 20 `IO_STATUS_BLOCK` writes:**
  - 15 inline `write_unaligned(p.add(8) as *mut usize, …)` sites: `hook.rs:3389, 3653, 3680, 4187, 4216, 5244, 5607, 5980, 6006, 6066, 6100, 6119, 6962`;
  - plus `synth_iosb_ok` (`4978`), plus `setinfo_ok_iosb` (`4696`, which equals `synth_iosb_ok(iosb, 0)`);
  - plus `regnotify::write_iosb` (`regnotify.rs:580`, which uses `write_volatile` where the others use `write_unaligned`).

**Proposed change.** Add a `ntbuf` module in vfs-shim with:
- `unsafe fn us_units<'a>(*const UnicodeString) -> Result<Option<&'a [u16]>, NTSTATUS>`, with one documented rule;
- `us_string`;
- `struct OwnedOa` (from `AbsName`), with `fn absolute(template: &ObjectAttributes, nt: &str) -> Box<OwnedOa>` and `as_ptr()`;
- `unsafe fn iosb_set(iosb: *mut c_void, status, info)`.

Replace every site listed above. The pure halves (UTF-16 to String) go to S5's crate.

**Value:** H. It removes about 250 lines and the decoder inconsistency, and gives one place for SAFETY notes.
**Effort:** S–M.
**Risk:** Low–Med. Pick one rule for an odd byte length and a NULL buffer, and test it under Wine. Changing the redirect arms to NUL-terminate is a small behaviour change; check `drm_overlay_recursion_gone` and `hook_write`.

### S5. Move the pure NT layout and decision code into a host-testable crate, and make `vfs-redirect` build on Linux
**Category:** crate boundaries

**Evidence.**
- `vfs-redirect` is documented as the pure, unit-tested layer (`architecture.md:233-235`, `637-639`). But `cargo +stable test -p vfs-redirect` **does not compile on Linux**: `volumes.rs:273` uses `std::os::windows::fs::FileTypeExt` without `#[cfg(windows)]`. So none of its roughly 1,200 lines of tests run on a Linux host.
- Pure byte-layout logic is trapped in `unsafe fn`s in `hook.rs`, testable only under Wine:
  - `fill_by_name` (`4097-4147`, already unit-tested through a raw-pointer shim at `7086`);
  - the per-class writers in `fuse_query_information` (`5100-5325`: FILE_ALL prefix, FILE_STAT, FILE_ID, ATTRIBUTE_TAG, NAME);
  - `emit_object_name` (`5871`);
  - `spoofed_object_name` (`5655`, already pure);
  - `parse_rename_target`'s byte parsing (`2992-3003`);
  - `is_write_open`, `is_append_only`, `open_create_flags`, `dir_open_downgrades`, `disposition_needs_existence_probe` and `disposition_information` (`3045-3204`).
- Those disposition helpers match on literals `0 | 2 | 3 | 4 | 5`, although `vfs_redirect::FILE_SUPERSEDE … FILE_OVERWRITE_IF` exist (`vfs-redirect/src/lib.rs:887-892`).
- `vfs-registry` already plays this role for registry layouts ("NT query layouts", crate map). Files have no equivalent.

**Proposed change.**
1. Fix the Linux build: `#[cfg(windows)]` on the reparse pre-filter, and a non-Windows stub that returns no aliases.
2. Create `vfs-ntlayout` (`#![forbid(unsafe_code)]`, no dependencies), or a `vfs-redirect::nt` module, with `&mut [u8]` writers for: `FILE_INFORMATION_CLASS` by-name and by-handle answers, `FILE_NAME_INFORMATION` (move `write_file_name_info` and `write_dir_info` there from `vfs-redirect`), `OBJECT_NAME_INFORMATION`, the `FILE_RENAME_INFORMATION` parser, the disposition and access predicates (shared with `vfs_redirect::classify_open`, `lib.rs:906`), and the NT status and constant vocabulary.
3. Hooks keep only the `slice::from_raw_parts_mut` and the IOSB write.

**Value:** H. It turns Wine-only ABI checks into `cargo test` on Linux, and shrinks `file_info.rs` by about half.
**Effort:** M.
**Risk:** Low. Pure moves with existing tests. The one real change is `fill_by_name` and `fuse_query_information` sharing one writer (S6).

### S6. Stat-by-path and info-class answers are written twice or more
**Category:** duplication

**Evidence.**
- `qattr_hook_body` (`4229-4299`), `qfull_hook_body` (`4301-4372`) and `qibn_hook_body` (`4149-4227`) each run the same sequence: `path_of`, then `fuse_path_attr`, then `note_stat`, then the NOT_FOUND seal or `allow_disk_fallthrough`, then the `engine.overlay_state` fallback.
- The four-`SYNTH_FILETIME` fill plus attributes is pasted five times: `4259`, `4284`, `4330`, `4355`, `5121`, and again with a different style in `5291`.
- The FileBasic, FileStandard, FileNetworkOpen and FileStat layouts are implemented twice: by offset in `fill_by_name` (`4120-4145`), and by struct or offset in `fuse_query_information` (`5116-5302`).

**Proposed change.**
- Add `fn stat_by_path(path) -> Option<Result<PathStat, NTSTATUS>>`. It returns `None` for "outside every root" and covers both the director answer and the overlay fallback.
- Add one `write_info_class(class, &PathStat, file_id, pos, buf)` in S5's crate.
- The three hooks then become about 15 lines each.

**Value:** H. A fix to one stat path currently has to be made three times, and missing one is the "stat APIs disagree" bug class that `hook_stat_agreement` exists to catch.
**Effort:** S.
**Risk:** Low–Med. The `note_stat` labels differ per hook (`"found"` versus `byname{class}-ok`); keep them as parameters.

### S7. `create_hook` and `open_hook` duplicate the whole routing tail
**Category:** duplication

**Evidence.**
- `hook.rs:3721-3842` and `3972-4068` are line-for-line the same: decode once, `UncachedScope`, `note_passthrough` or `note_undecodable`, `try_fuse_create`, the Routed stats block, `decision_for`, the Redirect arm with OA rebuild, Deny, PassThrough with the `fuse_root_directory` rebuild, then `tag_under_root` and `record_path`.
- Only the trampoline arity and `disp` differ.
- The comments explaining `outcome_recorded` are also duplicated (`3749-3753`, `3983-3986`).

**Proposed change.** `unsafe fn route_open(call: OpenCall, tramp: &dyn Fn(*const ObjectAttributes) -> NTSTATUS) -> NTSTATUS`, where `OpenCall` holds the handle out-pointer, access, oa, iosb, disposition, create options and a create-or-open flag. Each hook supplies a closure that calls its own trampoline with a substituted OA. `tramp_create_abs` and `tramp_open_abs` disappear into S4's `OwnedOa`.

**Value:** M–H. This is the hottest path, and the two copies have drifted before.
**Effort:** M.
**Risk:** Med. Hot path: `note_open_sync` exists only in create (`3776`), and the `mark_rooted` timing must be preserved. Cover with `hook_relative_paths`, `hook_write*`, `write_seal*` and `hook_dir_write_open`.

### S8. The payload `Config` ABI is defined three times, and offsets are pinned in only one
**Category:** crate boundaries and duplication

**Evidence.**
- `vfs-payload/src/lib.rs:36-70` (`Config`) has `offset_of!` asserts (`756+`).
- `vfs-inject/src/payload_cfg.rs:6-40` (`PayloadConfig`) has no asserts.
- `vfs-shim/src/payload_abi.rs:10-39` (the "MUST match … field-for-field" comment) has no asserts.
- The shim copy is byte-identical to the vfs-inject copy (verified with `diff`), and vfs-shim already depends on vfs-inject.
- `install_late` writes `secondary_*` into this struct inside a live process (`hook.rs:1040-1056`). A silent field-order drift corrupts the early payload's dispatch.

**Proposed change.**
- Delete `payload_abi.rs` and re-export `vfs_inject::PayloadConfig` from `lib.rs:100`.
- Better: add a tiny `#![no_std]` `vfs-payload-abi` crate. A path dependency works across the separate payload workspace, and vfs-payload, vfs-inject and vfs-shim would all use it.
- At minimum, copy the `offset_of!` test into vfs-inject.

**Value:** H (silent ABI drift). **Effort:** S. **Risk:** Low.

### S9. Unsafe hygiene: unsafe ops without blocks in unsafe-fn bodies, and a stale "all unsafe is here" claim
**Category:** unsafe hygiene

**Evidence.**
- Edition 2021 lets `unsafe fn` bodies perform unsafe operations with no block and no SAFETY comment.
- clippy `-W unsafe_op_in_unsafe_fn` counts:

  | file | sites |
  |---|---|
  | `hook.rs` | 658 |
  | `regkeys.rs` | 84 |
  | `regquery.rs` | 70 |
  | `regwrite.rs` | 32 |
  | `regnotify.rs` | 21 |
  | `vfs-inject/inject.rs` | 20 |
  | `lazy_section.rs` | 14 |

  The `hook.rs` sites include 83 raw derefs, 65 `write_unaligned` and 39 `transmute`.
- `hook.rs:1` says "ALL `unsafe` in the crate lives here". It is false: `bootstrap.rs`, `inject.rs`, `lazy_section.rs`, `regkeys.rs`, `regquery.rs`, `regwrite.rs`, `regnotify.rs`, `fuse_client.rs` and `breadcrumb.rs` all allow `unsafe_code`.
- Redundant inner `#[allow(unsafe_code)]` inside an already-allowing module: `hook.rs:5042`, `5085`, `5624`, `5899`.
- `undocumented_unsafe_blocks` counts 12 in `hook.rs`, 10 in `lazy_section.rs`, and about 200 across tests.

**Proposed change.**
- Add `#![deny(unsafe_op_in_unsafe_fn)]` to each module as S1 creates it. That is the edition-2024 default, so it also prepares the edition bump.
- Wrap operations in small blocks with SAFETY lines. Most collapse into S4's helpers, so the count drops sharply first.
- Fix the `hook.rs:1` doc and drop the redundant allows now.

**Value:** M. **Effort:** M, done incrementally alongside S1. **Risk:** Low (no codegen change).

---

## Tier 2: worthwhile

### S10. Stale and misplaced comments
**Category:** doc quality. **Value:** M. **Effort:** S. **Risk:** Low.

- **Orphaned doc blocks:**
  - `hook.rs:2608-2617`: the docs for "Fully-qualified NT/Win32 path for an open" and "The ObjectAttributes name field alone" have lost their functions and are stacked on `cwd_from_peb`.
  - `hook.rs:4704-4726`: `setinfo_hook_body`'s doc is attached to the `FILE_COMPLETION_INFORMATION` const.
  - `hook.rs:6971-6977`: a doc about "Offsets and sizes of the metadata classes" sits on the `spoofed_object_name` test.
- **"JVM" references to the old director:** `hook.rs:3642`, `3663`, `4705`, `5919`; `fuse_client.rs:670`, `699`.
- **Stale counts:**
  - "all twenty ntdll entry points" (`hook.rs:81`);
  - "18 of the 20 functions" (`1100`);
  - "all twenty ntdll detours" and "20 detours + 2 test hooks" (`7702`, `7707`).

  There are now 60.
- **Other stale text:**
  - `hookstats.rs:19` says "Anything not listed lands in `Other`", but there is no `Other` variant.
  - `try_fuse_create`'s poisoning argument (`hook.rs:3284-3308`) gives reason 2: "rustc's forced abort … tears the process down". `contain_panic` now catches at that boundary, so reason 2 is false.
  - `vfs-inject/src/map.rs:40` links `crate::ghostly::preload_remote_import_dlls`, a module that no longer exists.
  - `read_hook_body`'s doc narrates its own history (`6027-6029`).

### S11. Move incident narratives from code into docs
**Category:** doc quality

**Evidence.**
- `hook.rs` is 27% comments. It has about 60 "gate N / Task M" references, 25 "used to", and 9 dated measurements.
- Comment blocks over 20 lines:

  | location | lines | content |
  |---|---|---|
  | `close_hook` | `4417-4439` | the try_lock hang trace |
  | `serve_dir_query` | `6729-6815` | 87 lines, including an admission that the arm is dead code |
  | `serve_dir_query` | `6859-6880`, `6903-6918` | |
  | `try_fuse_create` | `3236-3308`, `3408-3500` | |
  | `setinfo_hook` | `4905-4965` | |
  | `install_panic_hook` | `930-970` | |
  | `ShimIoGuard` | `27-39` | |
  | `lock_hook` | `5398-5454` | |
  | `qif_hook` / `qobj_hook` | `5550-5574`, `5692-5724` | |

- `engine.rs` (42% comments) and `overlay.rs` (41%) are similar.

**Proposed change.**
- Create `docs/shim-invariants.md`, or one ADR per incident: close-path locking, enumeration containment, write-seal statuses, panic containment, name-query consistency, lock semantics.
- In code, leave the invariant in 2–5 lines plus a link.
- Keep the measured tables (for example the `qobj` buffer contract) in rustdoc, because they are API contract.
- Do this during S1, so each new module starts lean.

**Value:** M. Reviewability of the hot paths roughly doubles. **Effort:** S–M. **Risk:** Low, but be careful not to drop an invariant while trimming its history.

### S12. Close-path locking has two policies; consolidate the handle tables
**Category:** duplication and consistency

**Evidence.**
- The file side, `close_hook_body` (`hook.rs:4441-4457`), uses a single `try_lock` per table and gives up silently.
- The registry side, `regkeys::lock_for_close` (`regkeys.rs:155-165`), spins up to 10,000 `try_lock`s and counts each give-up in `hookstats`. It is used by `regkeys`, `regquery.rs:726` and `regnotify.rs:501`.
- Four handle-keyed maps (`DIR_TABLE`, `IDENTITY_TABLE`, `PATH_TABLE`, `HANDLE_PATHS`; `hook.rs:865`, `868`, `872`, `2905`) are each locked separately on every close. `PATH_TABLE` is a subset of `HANDLE_PATHS` (under-root only), and `try_fuse_create` inserts into both (`3392-3395`).

**Proposed change.**
- Add one `sync::lock_for_close` helper with one documented policy, and use it on both sides.
- Fold the four maps into one `HandleTable { path, under_root: bool, identity: Option<String>, dir: Option<DirTracked> }`. That means one lock per close and one place for the `HANDLE_PATHS_MAX` bound.

**Value:** M. **Effort:** S for the helper, M for the table merge. **Risk:** Med. Close is where the 2026-09-02 hangs were. Keep `try_lock` semantics, and keep `serve_dir_query`'s rule that it never holds the lock while touching the caller's buffer (`6901-6918`).

### S13. Registry hook body boilerplate, and a redundant guard
**Category:** duplication

**Evidence.**
- Ten read-shaped bodies repeat `Timed`, then `let Some(tramp)`, then `reg_bypass`, then `ShimIoGuard::enter() else tramp(...)`. They are at `1793`, `1822`, `1946`, `1967`, `1997`, `2027`, `2057`, `2207`, `2228` and `2282`.
- Five write-shaped bodies repeat `enabled`, then `reg_write_guard`, then `WriteScope`, then the `Write::Done`/`Pass` match. They are at `2099`, `2125`, `2144`, `2163` and `2182`.
- `reg_write_guard` (`2089-2094`) is `if in_hook_reenter() { None } else { ShimIoGuard::enter() }`, which is exactly what `ShimIoGuard::enter()` already does (`43-52`).
- Likewise, `reg_bypass`'s `in_hook_reenter()` check is repeated by the `enter()` that always follows it.

**Proposed change.** Add `reg_read_body!` and `reg_write_body!` macros in the style of the existing `out_of_scope_body!` (`2419`), or fold them into S2's table as a `kind:` column. Delete `reg_write_guard`.

**Value:** M (about 400 lines). **Effort:** S. **Risk:** Low.

### S14. Decide the fate of the Engine and shim-local overlay path
**Category:** legacy code, design decision

**Evidence.**
- Standalone mode is retired: bootstrap aborts if the ring cannot attach (`hook.rs:6772-6776`). Even so, every file hook still has a second routing branch through `Engine` and `Overlay`:
  - the Redirect arms (`3782`, `4017`);
  - the overlay fallbacks in qattr, qfull and qibn (`4209`, `4280`, `4351`);
  - `delete_hook`'s `engine.whiteout` (`4548`);
  - `setinfo_hook`'s engine branch (`4859-4972`);
  - `serve_dir_query`'s `ContainedNoDirector` arm (`6883-6895`), which its own comment calls dead code (`6785-6796`).
- `engine.rs` is 1,399 lines and `overlay.rs` 463.
- Several hook tests (`hook_enum_parity`, `hook_relative_paths`, `hook_write`) run with no ring and depend on the overlay Redirect path, so it is still a *test* mode.
- A known divergence is documented between the two delete routes, where `setinfo` uses the engine only (`6869-6880`).

**Proposed change.** Make an explicit decision.
- **Either** declare the shim-local overlay a supported no-director mode, and document it in `architecture.md`;
- **or** port those tests to `fakedirector` and delete the Engine's Redirect, overlay and whiteout paths. Keep `RootMap` for `path_is_ours`.

**Value:** H if removed: roughly 2,000 lines and half the branches in every file hook. **Effort:** L. **Risk:** High: behaviour change and test rewrites. It also interacts with copy-up (`cow_seed_*` tests).

### S15. Dead compatibility re-exports and PE helper duplication in vfs-inject
**Category:** dead code and crate boundaries

**Evidence.**
- `vfs-inject/src/lib.rs:21-28` re-exports `is_system_import_dll` and `import_dll_names_of_pe`, documented as having "no remaining in-workspace caller".
- `pe.rs:36-43` does the same.
- `map.rs:19` re-exports five `vfs_pe` functions so that internal callers keep their old spelling (`map.rs:6-18` explains).
- `rd_u32` and `rd_u64` are duplicated in `map.rs:27-32`, `pe.rs:25` and `vfs-pe/src/lib.rs:3-11`.
- `pe_layout` (`pe.rs:28`) re-derives what `vfs_pe::build_image` returns.
- The shim calls `vfs_inject::pe_looks_like_image` (`hook.rs:6250`) although it depends on `vfs-pe` directly.

**Proposed change.**
- Delete the unused re-exports and call `vfs_pe::` directly.
- Have `vfs-pe` export `pub(crate)`-style readers through a small `le` module, or accept the duplication but delete the long justification comments.

**Value:** L–M. **Effort:** S. **Risk:** Low. Check that `vfs-embed` and `vfs-director` do not use the re-exports: grep found no callers outside vfs-inject.

### S16. Module naming, and scattered handle tags
**Category:** naming and organisation

**Evidence.**
- `zipserve.rs` admits it is misnamed: "keeps its old name only because renaming it would churn every call site" (`zipserve.rs:3-18`). It is now a synthetic *section* table.
- `fuse_synth.rs` is the synthetic *file* handle table.
- `fuse_client.rs` is the director ring client. "FUSE" is the director's kernel, not what the shim talks to.
- The shim's `inject.rs` (child injection from `cpiw`) shares a name with the `vfs-inject` crate.
- The handle-tag space is spread over three files, with the cross-references kept in comments:
  - `FUSE_TAG` 2^47 (`fuse_synth.rs:9`);
  - `SYNTH_SECTION_TAG` 2^45 (`zipserve.rs:27`);
  - `REG_TAG` `0x6000_0000` (`regkeys.rs:69`).
- `fuse_synth::lookup` returns an anonymous 5-tuple `(u64, u64, bool, u64, bool)` (`fuse_synth.rs:118`), destructured as `(fh, size, is_dir, pos, append_only)` at `hook.rs:4767`, `5107`, `5950` and `6230`, although a `ReadView` struct already exists (`107`).

**Proposed change.**
- Renames:
  - `synth::{file, section, key}` (with `regkeys` keeping the key records), or `synth_file.rs` and `synth_section.rs`;
  - `fuse_client` to `director.rs` (or `ring_client.rs`);
  - the shim's `inject.rs` to `child.rs`.
- Add a `handle_tags.rs` with all three tags and the non-overlap test (now `regkeys.rs:1720`).
- Return a struct from `lookup`.

**Value:** L–M. **Effort:** S (mechanical; do it with S1). **Risk:** Low. `fuse_client` is `pub` (`lib.rs:9`) and used by tests (`fakedirector::second_client`), so keep a deprecated alias for one release.

### S17. Consolidate the test binaries and shared fixtures
**Category:** test layout

**Evidence.**
- 34 integration binaries; 25 of them contain a single `#[test]`. 107 tests in total.
- Each Windows test exe is about 5.7 MB (`target/x86_64-pc-windows-msvc/debug/deps`), so the link step dominates incremental builds.
- "One install per process" (`architecture.md:640-643`) is the reason given.
- The registry tests each build their own `Fixture`, with copied helpers:
  - `user_sid` ×4;
  - `reg_create` ×4;
  - `wide` ×5;
  - `open_abs` ×3;
  - `with_oa` ×3;
  - `reg_checker` ×3;
  - `object_name` ×3.
- `fakedirector/mod.rs:51` and `ntapi/mod.rs:7` need `#![allow(dead_code)]` because they are compiled into every binary.

**Proposed change.**
1. Move the registry helpers into `tests/common/reg.rs`.
2. Merge scenarios into about 4 binaries (`file_hooks`, `seal`, `registry`, `diagnostics`), with each test running in its own process. Either:
   - adopt `cargo nextest`, which is process-per-test by design (add `.config/nextest.toml`); or
   - write a `harness = false` main that re-executes itself with `--exact <name>` per scenario. The registry tests already self-exec a `reg_checker`.
3. Put per-scenario environment setup in a `scenario!` helper that runs before `install`.

**Value:** M (build and link time; dedup). **Effort:** M. **Risk:** Med. Environment variables and `OnceLock` statics leak between tests if anything runs in-process. Verify with Wine that every scenario still gets a fresh process.

### S18. Small duplications worth one sweep
**Category:** duplication. **Value:** L–M. **Effort:** S. **Risk:** Low.

- **The root vpath as `"."`** is spelled out 7 times (`hook.rs:3234`, `3636`, `4077`, `4539`, `4790`, `4818`, `6822`). Fix with `FuseClient::route(path) -> Option<(RootId, VPath)>`.
- **`to_nt_path`** (`hook.rs:3855`) duplicates `vfs_redirect::to_nt` (`lib.rs:930`) plus a strip.
- **ByteOffset parsing** is duplicated in read and write (`hook.rs:5940-5949`, `6048-6057`).
- **The write-access mask is defined five times:**
  - `hook.rs:3046`;
  - `hook.rs:3072-3074`;
  - `engine.rs:1112`;
  - `engine.rs:1274`;
  - `vfs_redirect::classify_open` (`lib.rs:908`), which also counts `GENERIC_ALL`, unlike `is_write_open`.

  Unify them, and decide whether `GENERIC_ALL` counts.
- **Duplicate NTSTATUS constants:**
  - `STATUS_ACCESS_VIOLATION` ×3 (`lazy_section.rs:43`, `ntdef.rs:599`, `regkeys.rs:75`);
  - five more duplicated between `ntdef.rs` and `vfs-registry/src/layout.rs`.
- **Trace logging:** `drm_exe_trace` and `director_open_trace` duplicate the append-line-to-log code (`3529-3606`). `director_open_trace` also re-reads `VFS_DIRECTOR_OPEN_LOG` from the environment on **every successful director open** (`3567`), while its sibling caches the value in a `OnceLock` (`3530`). That is a small hot-path cost.

### S19. Split `hookstats.rs` (2,548 lines)
**Category:** oversized module

**Evidence.** One file holds:
- timing (`22-226`);
- the snapshot and renderer (`228-500`);
- async and fill counters (`504-575`);
- name and registry counters (`580-800`);
- read-cache render (`805-905`);
- setinfo and lock counters (`941-1050`);
- path, trace and stat maps (`1056-1225`);
- readdir (`1225-1380`);
- open outcomes (`1386-1640`);
- copy-up and overlay failures (`1642-1930`);
- the reporter thread (`1932-2030`);
- about 500 lines of tests.

The bounded `Mutex<Option<HashMap<String, u64>>>` with a `*_MAX` pattern is repeated 10 times (`PATHS`, `UNDECODABLE`, `STATS`, `SYNTH_LOCKS`, `COPYUPS`, `OVERLAY_FAILS`, `OUTCOME_PATHS`, `SETINFO_NOOP`, …).

**Proposed change.**
- Split into `hookstats/{mod.rs (enabled, Timed, Hook; Hook generated by S2), counters.rs (generic BoundedTally<K>), open.rs, registry.rs, overlay.rs, io.rs, report.rs}`.
- Add one `BoundedTally` type to replace the 10 hand-rolled maps.

**Value:** M. **Effort:** M. **Risk:** Low. The report text format is asserted by tests and by Haskill's parser, so keep labels byte-identical.

### S20. Split `vfs-redirect/src/lib.rs` (2,170 lines), and drop test-only public API
**Category:** oversized module and dead code

**Evidence.**
- `lib.rs` holds the `UncachedScope` and path cache (`20-225`), `RootMap` (`226-660`), the dir-info writers (`686-880`), the NT disposition constants and `classify_open` (`887-912`), whiteout helpers (`916-928`), path helpers (`930-978`), and tests (`979-2170`).
- `utf16_to_string` and `string_to_utf16` (`674-683`) are `pub` but used only by this crate's own tests.

**Proposed change.**
- Split into `rootmap.rs`, `cache.rs`, `dirinfo.rs` (or move it to S5's crate), `nt.rs` and `whiteout.rs`.
- Delete the two UTF-16 helpers, or make them the S4/S5 decoders.

**Value:** M. **Effort:** S. **Risk:** Low.

---

## Tier 3: nitpicks

### S21. Over-wide `pub` in private modules
**Category:** naming. **Value:** L. **Effort:** S. **Risk:** Low.

clippy `unreachable_pub` counts:

| file | count |
|---|---|
| `ntdef.rs` | 145 |
| `regkeys.rs` | 54 |
| `hookstats.rs` | 47 |
| `tests/ntapi` | 31 |
| `tests/fakedirector` | 26 |
| `fuse_synth.rs` | 15 |
| `overlay.rs` | 12 |
| `regwrite.rs` | 9 |
| `inject.rs` | 9 |

Change these to `pub(crate)`. That lets `dead_code` see unused items. The current zero-warning result is partly because `pub` hides them.

### S22. rustfmt drift
**Category:** hygiene. **Value:** L, but it is a prerequisite for reviewable splits. **Effort:** S. **Risk:** Low.

`cargo fmt --check` hunk counts:

| file | hunks |
|---|---|
| `tests/ntapi/mod.rs` | 153 |
| `tests/fakedirector/mod.rs` | 108 |
| `hook.rs` | 68 |
| `vfs-redirect/lib.rs` | 49 |
| `engine.rs` | 29 |
| `hookstats.rs` | 26 |

`hook.rs` also has a stray blank line inside the `use` list (`694`) and a double blank (`6569-6570`). Do one fmt-only commit before anything else.

### S23. Unused-value silencers
**Category:** noise. **Value:** L. **Effort:** S. **Risk:** Low.

- `let _ = kb;` (`hook.rs:1301`)
- `let _ = fh;` (`5111`)
- `let _page_prot = page_prot;` (`6229`)
- `let _ = (apc, apc_ctx, key);` (`6011`)
- `let _ = (process, zero_bits, …);` (`6488`)
- `let _ = process;` (`6524`)

Use `_`-prefixed parameters or a destructure instead.

### S24. Instrumentation gaps
**Category:** consistency. **Value:** L. **Effort:** S. **Risk:** Low.

`unmap_view_hook_body` (`hook.rs:6512`) and `cpiw_hook_body` (`6535`) have no `Timed` and no `Hook` variant. S2's table makes this uniform for free.

### S25. Document the xwin CFLAGS workaround
**Category:** docs. **Value:** L. **Effort:** S. **Risk:** Low.

`libudis86-sys` needs `CFLAGS_x86_64-pc-windows-msvc=-Wno-error=implicit-function-declaration` under clang-cl (seen in a previous successful build's `output`). Put it in the README's cargo-xwin section, or in `.cargo/config.toml` `[env]`.

---

## Suggested order of work

1. **Groundwork. No behaviour change; each is a small PR.**
   - S22 rustfmt-only commit.
   - S10 stale and orphaned comments, and S23 silencers.
   - S8 payload ABI: one definition plus `offset_of!` tests.
   - S5 step 1: `#[cfg(windows)]` in `vfs-redirect/volumes.rs`, so its tests run on Linux.
   - S25 README note.
2. **Shared helpers in place, before anything moves.** S4 (`ntbuf`: UNICODE_STRING, OwnedOa, IOSB) and S18. This shrinks the code that S1 has to move.
3. **S2: table-driven detours and the `Tramp<F>` type.** This removes `static mut` and the install boilerplate and generates `hookstats::Hook`. Run the full Wine suite.
4. **S1: split `hook.rs`.** Move-only commits, one module at a time. Rewrite the containment test to scan `src/hook/`. Turn on `unsafe_op_in_unsafe_fn` per new module (S9), and trim narratives into `docs/shim-invariants.md` as each module lands (S11). Do the S16 renames in the same pass.
5. **Registry dedup.** S3 (`KeyHandle` / `KeyRef`, one `with_real`), then S13 (body macros, drop `reg_write_guard`).
6. **File-hook dedup.** S6 (stat-by-path), then S7 (`route_open`), then S12 (close-lock policy and the merged handle table).
7. **Pure crate.** S5 step 2 (`vfs-ntlayout`, or `vfs-redirect::nt`), and S20 (split `vfs-redirect`).
8. **The rest.** S19 (`hookstats` split with `BoundedTally`), S15 (vfs-inject re-exports), S21 (`pub(crate)`; re-run clippy to find newly visible dead code).
9. **Tests.** S17 (common registry fixture, then consolidated binaries with process-per-test).
10. **Design decision, last.** S14: keep or remove the Engine and shim-local overlay. Do this once the code is small enough to see the full surface; it is the largest single deletion available.

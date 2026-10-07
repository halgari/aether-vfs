# Pre-merge review — injected shim NT hooks, engine, director/ring side

Branch `feat/stage4-embed`. `cargo test -p vfs-shim` passes (exit 0, all suites green).

**Verdict:** the two gates and the lock trio are real and well built, but the governing
invariant does not hold — three NT object-manager spellings of an under-root path reach the
real file, verified by execution — and `docs`' "no exceptions of any kind" claim is false.

---

## Note on the diff supplied

The diff I was given (`shim.diff`) is the **panic-containment + vfs-embed extraction** work
(`9c487de`…`2e768f2`), not gates 4/5. `cow_seed`/`SEED_CHUNK`/the deleted DRM exceptions all
landed earlier on this branch. I reviewed the area against the stated priorities rather than
only the diff, and verified every gate-4/5 claim against present-day code.

---

## Critical

### C1 — `\GLOBAL??` spellings reach the real file under a managed root — **VERIFIED**

`rust/crates/vfs-redirect/src/canon.rs:166`

I ran `RootMap::contains` against a root of `C:\Games\Skyrim`:

```
CONTAINS false  <- \??\GLOBALROOT\GLOBAL??\C:\Games\Skyrim\Data\a.esp
CONTAINS false  <- \??\GLOBALROOT\??\C:\Games\Skyrim\Data\a.esp
CONTAINS false  <- \GLOBAL??\C:\Games\Skyrim\Data\a.esp
CONTAINS true   <- \??\C:\Games\Skyrim\Data\a.esp
```

All four name the same file. `false` means `Engine::decide` → `Decision::PassThrough` and
`create_hook_body` (hook.rs:2083-2084) trampolines the **original, untouched OA**; the kernel
resolves `\??\GLOBALROOT` → `\`, then `\GLOBAL??\C:` → the volume device, and opens the real
file under the managed root. Nothing counts it (`note_passthrough_outcome` only fires when
`path_is_ours`, which is exactly what is false here), so it is invisible in every report.

Two independent causes, both verified:

1. **`strip_stream_suffix` truncates at the drive colon** (canon.rs:166-171). It assumes a
   drive colon can only sit immediately after the NT prefix, so when the prefix is followed
   by any other token it treats the drive colon as a stream separator:
   `split_stream_suffix(r"\??\GLOBALROOT\GLOBAL??\C:\Games\Skyrim\Data\a.esp")` returns
   `("\\??\\GLOBALROOT\\GLOBAL??\\C", Some(":\\Games\\Skyrim\\Data\\a.esp"))` — measured.
   This alone breaks the `GLOBALROOT\??\C:` form, which `strip_globalroot_wrapper` would
   otherwise have handled correctly.
2. **`\GLOBAL??` is unknown to canonicalisation** — absent from `NT_PREFIXES` (canon.rs:22),
   and `strip_globalroot_wrapper` matches only the literal token `GLOBALROOT` (canon.rs:212).

Why the escape matrix passes: `docs/escape-matrix.md`'s vector 3 tests only
`\??\GLOBALROOT\Device\HarddiskVolumeN\...`, which contains no colon and so never reaches the
truncating branch (confirmed: that spelling round-trips intact).

### C2 — "the invariant holds without exception" is false — **VERIFIED**

`rust/docs/bypass-baseline.md:1294` (and its §"Gate 5: the invariant holds without exception",
"This session is the first in which that is true with **no exceptions of any kind**")

C1 alone falsifies it. Two more under-root operations are answered by something other than the
director, both by code I read directly:

- `qibn_hook_body`: when the director says the path **exists** but `fill_by_name` declines the
  info class (anything outside `{4,5,34,68,77}`, hook.rs:2354-2361), control falls past the
  overlay probe to `tramp` at hook.rs:2465 — the real file's metadata is returned for an
  under-root path. `NtQueryInformationByName` accepts several classes not in that set
  (`FileAttributeTagInformation`, `FileEaInformation`, `FileIdInformation`,
  `FileStatLxInformation`, `FileCaseSensitiveInformation`). The comment at hook.rs:2342-2343
  admits the fall-through ("no worse than before this hook existed") — which is a statement
  the top-level invariant claim contradicts.
- `serve_dir_query` returns `passthrough()` for an unknown directory info class **before any
  root or handle check** (hook.rs:4152-4156). See C4.

The measured run tables in that document are sound; it is the unqualified "no exceptions"
framing that is not.

### C3 — this branch's own `contain_panic` falsifies the comment that licenses a known hole — **VERIFIED**

`rust/crates/vfs-shim/src/hook.rs:1566`

hook.rs:1544-1550 records a real hole: `open_fuse_at_ex(...)?` (hook.rs:1621) gives up its
handle when `fuse_synth`'s mutex is poisoned, returning `None` from `try_fuse_create` *after*
the director already opened the file — leaking the `fh` and sending the open to
`decision_for`. Reason 2 for why it "is still not a live route" reads:

> every production path into this code arrives through an `unsafe extern "system"` hook, and
> rustc's forced abort-on-unwind … tears the process down while that unwind is still in
> flight. The guard's drop would set the poison flag on the way out, but **no later call would
> be alive to observe it.**

`contain_panic` (added in this diff) catches at that boundary, so "no later call would be
alive" is now false — and `contain_panic`'s own doc says the opposite in as many words
(hook.rs:131-135: "a panic taken while one of those tables is locked poisons it for the rest
of the process"). Grepped: this paragraph is not in the diff, i.e. it was not revisited. Reason
1 (nothing under those locks can unwind) still holds, so the hole stays latent — but the
recorded justification is now half wrong, which is exactly the failure mode the branch's own
prose keeps warning about.

### C4 — an unknown info class hands a synthetic handle to the real kernel, uncounted — **VERIFIED in code, reachability SUSPECTED**

`rust/crates/vfs-shim/src/hook.rs:4152`

`DirInfoClass::from_u32` covers only `1,2,3,12,37,38` (vfs-redirect/src/lib.rs:719-729).
`serve_dir_query`'s first act on any other class is `return passthrough()` — before the
`DIR_TABLE` lookup, before any root test, and with no counter on that arm. Consequences:
`FileIdExtdDirectoryInformation` (60), `FileIdExtdBothDirectoryInformation` (63) and
`FileIdGlobalTxDirectoryInformation` (50) on a director-served directory hand a bit-47 handle
to real ntdll and come back `STATUS_INVALID_HANDLE`; on a real under-root directory handle they
enumerate the real directory. This is structurally the **same failure the branch exists to
fix** — an under-root op reaching the kernel with a synthetic handle, silently — and unlike
`setinfo_hook`'s catch-all (which has `note_setinfo_noop`) it records nothing at all.

### C5 — unknown query classes on a synthetic handle return SUCCESS with the caller's buffer unwritten — **VERIFIED in code, reachability SUSPECTED**

`rust/crates/vfs-shim/src/hook.rs:3251`

`fuse_query_information`'s catch-all is `_ => { synth_iosb_ok(iosb, 0); STATUS_SUCCESS }`.
The caller's `info` buffer is never touched, so it parses whatever was on its own stack as a
valid answer, and no counter fires. `STATUS_HOOK_PANICKED`'s own doc rules this out as a
design principle three thousand lines earlier (hook.rs:63-69: "answering `STATUS_SUCCESS`
would hand the game … a buffer of stack garbage to parse, which is materially worse than the
abort this replaces"). Concretely: `qif_hook` routes synthetic handles here *before* its
class-48 branch (hook.rs:3483-3485), so `FileNormalizedNameInformation` on a director-served
handle lands in the catch-all and `GetFinalPathNameByHandleW` reads uninitialised memory as a
path.

---

## Important

- `hook.rs:3018` — `setinfo_hook`'s under-root sealing is gated on `is_delete || is_rename`
  only, so on a **non-synthetic** under-root handle (inherited, pre-injection, duplicated, or
  `allow_disk_fallthrough`) `FileEndOfFileInformation` truncates the real file,
  `FileLinkInformation` hardlinks it, and `FileBasicInformation`/`FileAllocationInformation`
  mutate it — the `path_is_ours` backstop at hook.rs:3092 is never reached. That is the exact
  handle shape `handle_ops_out_of_root_sealed.rs` proves is sealed *for delete and rename*.
- `hook.rs:2411`, `2480`, `2552` — `qibn`/`qattr`/`qfull` decode with the provenance-blind
  `path_of` and hold **no `UncachedScope`**, then feed the result into `vpath_under_root` and
  `overlay_state`, both `RootMap`-cached on the raw string. That violates the contract stated
  at hook.rs:1010-1015 and 1139-1141, which `create`/`open`/`delete`/`setinfo` all honour; it
  is not listed as a known gap anywhere.
- `hook.rs:1313` — `is_write_open`'s `WRITE_ACCESS` omits `GENERIC_ALL` (`0x1000_0000`), which
  `classify_open`'s `WRITE_MASK` includes (vfs-redirect/src/lib.rs:864). Two write predicates
  in one flow disagree: a `GENERIC_ALL` open under a root is routed as a **read**, gets a
  read-only director handle, never copies up, and every subsequent write fails.
  `MAXIMUM_ALLOWED` (`0x0200_0000`) is missing from both.
- `hook.rs` (absent) — `FILE_DELETE_ON_CLOSE` (`0x0000_1000` in `CreateOptions`) appears
  nowhere in the shim: `close_hook` just releases the `fh`, so on a director-served path the
  delete silently never happens and nothing counts it.
- `engine.rs:368` — `!ov.has_file(...)` → `copy_up` is TOCTOU across threads and `ShimIoGuard`
  is thread-**local**, so two game threads seed the same `dest` concurrently with two
  `File::create` handles at offset 0; a loser reaching `std::fs::remove_file(dest)`
  (engine.rs:560) deletes the winner's completed seed while the winner's caller already holds a
  `Redirect` to it.
- `engine.rs:560` — that `let _ = std::fs::remove_file(dest)` is the one discarded `Result`
  left in the file, and it is the only thing standing between a failed seed and a truncated
  overlay file the director then serves as authoritative (the shim's overlay dir *is* the
  director's write layer). It sits directly under the claim that "every outcome is now counted
  and named" (engine.rs:429-434).
- `engine.rs:500` — "A partially written `dest` is removed, so `false` means … the caller's
  write starts from an empty overlay file" is only true for creating dispositions. For
  `FILE_OPEN`/`FILE_OVERWRITE` with write access, `decide_open` still returns `Redirect` to a
  path `cow_seed` just deleted, so the game's open fails `NOT_FOUND` — a write open reporting
  a file missing that a read open serves.
- `hook.rs:1258` — `parse_rename_target`'s doc says "Only absolute targets
  (RootDirectory == NULL) are handled; otherwise `None`". The body resolves handle-relative
  targets via `parent_dir_of_handle` thirty lines down (hook.rs:1288). False in-code doc on a
  function whose exact contract decides a rename seal.
- `hook.rs:4161`/`4359`, `4314`, `1621` — a contained panic now makes poisoning observable,
  and each poisoned table degrades differently and mostly uncounted: `DIR_TABLE` →
  every under-root enumeration `passthrough()`s (real listing for a real handle);
  `FuseClient::ring_lock` → `readdir` errors become `Vec::new()` (hook.rs:4314), i.e. silently
  **empty directories** — the empty-load-order shape — reported as `ReadDirSource::Director`;
  `fuse_synth::TABLE` → C3's leak. Only `note_hook_panic` records that anything happened.
- `hook.rs:4037` — `cpiw_hook` resumes the child even when `inject_child` fails or times out
  ("unvirtualized rather than hung"). The invariant is stated over "any process in the
  session"; this fails it open, silently, at the process boundary.
- `vfs-inject/src/inject.rs:533` — production always runs **dual-layer**
  (`run_target_with_shim` sets `DUAL_LAYER=1` and `PAYLOAD_CFG_FILE` unconditionally;
  `Session::launch` → `vfs_inject::run_target_with_shim`, vfs-embed/src/session.rs:1050), so
  `vfs-payload`'s four hooks are the outermost frames for `NtCreateFile`/`NtOpenFile`/
  `NtQueryAttributesFile`/`NtQueryFullAttributesFile` and vfs-shim is reached via
  `cfg.secondary_*`. **Every** test in `vfs-shim/tests/` uses single-layer `install`, so that
  dispatch is exercised by nothing. Additionally, a `match_redirect` hit in the payload calls
  real ntdll directly (vfs-payload/src/lib.rs:259-270), bypassing vfs-shim entirely — inert
  only because `Session::launch` uses `encode_config_with_overlay` and supplies no static
  imports.
- **Unhooked handle-taking NT APIs** (each silently `STATUS_INVALID_HANDLE` on a synthetic
  handle — the demonstrated `NtLockFile` failure mode), checked against the 21-entry
  `hook_entry_points!` list at hook.rs:192-393: `NtDuplicateObject`,
  `NtDeviceIoControlFile`/`NtFsControlFile`, `NtNotifyChangeDirectoryFile(Ex)`,
  `NtQuerySecurityObject`/`NtSetSecurityObject`, `NtQueryEaFile`/`NtSetEaFile`,
  `NtReadFileScatter`/`NtWriteFileGather`, `NtCreateSectionEx`/`NtMapViewOfSectionEx`,
  `NtFlushBuffersFileEx`, `NtCancelIoFile(Ex)`, `NtQueryObject`, `NtWaitForSingleObject`,
  `NtSetVolumeInformationFile`. `NtDuplicateObject` is the worst of these because it is also a
  *tracking* hole: `DUPLICATE_CLOSE_SOURCE` closes a handle without reaching `close_hook`
  (hook.rs:2639-2647), leaving stale `HANDLE_PATHS`/`PATH_TABLE` entries, so a recycled handle
  value resolves a later relative open against the **wrong parent**.
- Test defects (each verified by reading the test):
  - `tests/write_seal.rs:178` — `assert!(!served.exists())`, presented as spec §8 criterion 4.
    Only `root/data` is ever created (write_seal.rs:49); `root/write` does not exist, so the
    assertion is true regardless of what the shim does. `write_seal_no_overlay.rs:26-29` gets
    this right in the same suite by creating the directory on purpose.
  - `tests/cow_seed_reporting.rs:179` — `contains("seeded")` is already implied by the section
    header (`hookstats.rs:1402`), and label/path are asserted as independent substrings, so
    the attribution could be exactly **swapped** (`served.esp` FAILED, `missing.esp` seeded)
    and all four assertions still pass.
  - `tests/cow_seed_reads_through_director.rs:302` — asserts only `!dest.exists()` after a
    director error; passes unchanged if copy-up never ran at all (no `Decision`, no
    `tally.opens`).
  - `tests/hook_relative_paths.rs:251` — three `assert!(st < 0)` checks pass with the
    attribute hooks **removed** (the redirect target has no such file either), and
    `tests/ntapi/mod.rs:339`/`349`/`360` return `(-1, 0)` when `GetProcAddress` fails, so a
    typo'd export name satisfies `st < 0` too.
- `hook.rs:4934` — `no_extern_hook_bypasses_the_panic_containment_macro`, four gaps:
  hook.rs:4978-4981 claims brace-mismatch is "conservative … makes the body longer" — for a
  *presence* assertion longer is the unsound direction, so an unbalanced `{` in a string
  lets an uncontained entry point pass on a later function's `contain_panic`; the marker
  requires exactly one space, so `extern "system"\n    fn` never matches and two spaces hits
  the `end == 0 { continue }` **silent skip** (hook.rs:5020); only `extern "system"` is
  scanned, not `extern "C"`/`"stdcall"`/`"win64"` (`vfs-payload` already exports five
  `extern "C"` symbols); and only 2 of the 9 crates linked into the DLL are walked, so a
  callback added to `vfs-win`/`vfs-ipc` would be an uncontained entry point with nothing to
  say so — the `veh_handler` story again.

---

## Minor

- `hook.rs:3600`, `3699` — `read`/`write` answer a tagged-but-unresolvable handle with
  `STATUS_UNSUCCESSFUL` while `lock`/`unlock`/`flush` answer `STATUS_INVALID_HANDLE`
  (hook.rs:3396); `INVALID_HANDLE_VALUE` (`-1`) passes the bit-47 test, so the two disagree
  about the same input.
- `hook.rs:3510` — `write_hook`'s doc still says the buffer goes "to the JVM overlay"; so does
  setinfo's at hook.rs:2887. Stale.
- `hook.rs:1163` — `HANDLE_PATHS` silently stops inserting at `HANDLE_PATHS_MAX`, pushing every
  later relative decode onto `parent_dir_of_handle`'s case-4 OS consult — the cost the comment
  at hook.rs:1155-1161 says the unconditional insert exists to avoid.
- `hook.rs:3195` — `FileInternalInformation` reports `handle as i64` as the FileId, so two
  synthetic handles to one file report different ids, and `FILE_OPEN_BY_FILE_ID` (`0x2000`,
  unrecognised anywhere — the `ObjectName` is a binary FileId that `object_name_str`
  hook.rs:878 decodes as UTF-16) cannot work.
- `canon.rs:319` — `x.esp::$DATA`, the explicit spelling of the *default* stream, becomes vpath
  `…::$data` and is sealed not-found rather than served as `x.esp`.
- `canon.rs:248` — trailing dots/spaces are stripped even behind the `\\?\` verbatim prefix,
  where NTFS treats them as part of the name; unifies two genuinely different files (seals
  rather than leaks, so it is only a compatibility note).
- `hook.rs:2911`, `4071`, `4103`, and the handle hooks — no `in_hook_reenter` fast path, while
  `create`/`open`/`qattr`/`qfull`/`qibn`/`delete` all have one. Asymmetry worth a comment.
- Environment, not code: `vfs-fixture-escape` (PID 2856) was present at review start and is
  **unkillable** — `taskkill` reports "no running instance" while `tasklist` still lists it.
  That is a live instance of the documented `DllMain` shutdown stall, and it holds the shim
  DLL, so relinks will fail confusingly for the next person.

## On the known-failing directord e2e test

Not fixed, as instructed. One lead I saw while reading rather than a conclusion:
`serve_dir_query` has two `passthrough()` exits that record **nothing** — the unknown-info-class
exit at hook.rs:4152-4156 and the poisoned-`DIR_TABLE` exits at 4163/4361 — and the untracked-handle
exit at 4165-4181 records only when `hookstats::enabled()`; an instrument that "records nothing"
is consistent with the listing leaving on one of those, not with the director branch running.

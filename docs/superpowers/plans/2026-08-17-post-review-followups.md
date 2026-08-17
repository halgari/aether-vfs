# Post-Review Follow-Ups — feat/stage4-embed

**Source:** the five-area pre-merge review recorded verbatim in
`docs/superpowers/reviews/2026-08-17-stage4-branch/`. Read the area report before
picking up any item here; this file is a triaged index, not the evidence.

**What was fixed before merge** (so do not re-open these): the three NT-spelling
bypasses, `vfs-cache`'s disk-tier stale re-serve, `set_root_mounts`' missing
`SeqRead` gate, `MemoryProvider`'s silent directory no-ops, `ProviderWorker.close()`'s
hang, `releaseProvider`'s missing integer guard, and the false/stale claims in the
specs and benchmark docs (commit `71a3091`).

**Everything below is deliberately deferred.** Each item was verified or reasoned
by a reviewer with a file:line citation in the area report.

---

## Theme 1 — Guards that pass by not looking

This was the dominant defect class in the review: six separate mechanisms
reported success because they never examined the thing they were named for. Fix
these first. A guard that cannot fail is worse than no guard, because it also
suppresses the suspicion that would have found the bug.

- [ ] **`napi_entry_points_contain_panics.rs` silently skips what it cannot
  parse.** A blank line after the attribute, a doc comment after it,
  `pub(super) fn`, `pub(crate) async fn` — all dropped without a word. All 53
  current entry points are fine, so the *test* is the defect. Make an
  unrecognised item a failure, not a skip.
- [ ] **`no_extern_hook_bypasses_the_panic_containment_macro` has four holes.**
  Its brace-matcher's "conservative" claim is inverted for a presence assertion;
  the marker needs exactly one space, so `extern "system"\n    fn` is missed and
  two spaces is *silently skipped*; only `extern "system"` is scanned, not
  `vfs-payload`'s five `extern "C"` exports; and only 2 of the 9 crates linked
  into the DLL are walked — the `veh_handler` failure it was rewritten to prevent.
- [ ] **The seam-crossing guard misses a host.** `embed_api.rs`'s "no host in
  this workspace reaches past the seam" scans two crates for `.kernel()`, while
  `vfs-directord/src/bin/skyrim-live.rs` calls it 13 times, in the gap between
  this guard's `hosts` list and the daemon guard's `src/bin/` exemption.
- [ ] **Three zip corpus tests report green with zero coverage on CI.**
  `implicit_zip_directories_resolve_like_a_real_install` skips unless a
  native-extract directory exists, then does `let _ = native;` and derives
  everything from the archive. Two siblings are the same.
- [ ] **`write_seal.rs:178`'s `assert!(!served.exists())`**, billed as spec §8
  criterion 4, is true regardless of shim behaviour because `root/write` is never
  created. `write_seal_no_overlay.rs:26` does it correctly in the same suite.
- [ ] **`cow_seed_reporting.rs:179`** checks label and path as independent
  substrings, so the two outcomes could be exactly swapped and all four
  assertions still pass.
- [ ] **`cow_seed_reads_through_director.rs:302`** asserts only `!dest.exists()`
  after a director error, so it passes unchanged if copy-up never runs at all.
- [ ] **`hook_relative_paths.rs:251`** — three `assert!(st < 0)` checks pass with
  the attribute hooks removed entirely, and `tests/ntapi/mod.rs:339` returns `-1`
  when `GetProcAddress` fails, so a typo'd export name satisfies them too.

---

## Theme 2 — Unhooked NT APIs (silent failure on synthetic handles)

The demonstrated failure mode of this project: an unhooked handle-taking API
returns `STATUS_INVALID_HANDLE` on a synthetic handle and the app misbehaves with
no diagnostic anywhere. `NtLockFile` cost days precisely this way.

- [ ] **Triage the list the shim reviewer enumerated:** `NtDuplicateObject`,
  `NtDeviceIoControlFile`/`NtFsControlFile`,
  `NtNotifyChangeDirectoryFile(Ex)`, `NtQuerySecurityObject`/`NtSetSecurityObject`,
  `NtQueryEaFile`/`NtSetEaFile`, `NtReadFileScatter`/`NtWriteFileGather`,
  `NtCreateSectionEx`/`NtMapViewOfSectionEx`, `NtFlushBuffersFileEx`,
  `NtCancelIoFile(Ex)`, `NtQueryObject`, `NtWaitForSingleObject`.
  **`NtDuplicateObject` is the priority:** `DUPLICATE_CLOSE_SOURCE` bypasses
  `close_hook` and leaves stale `HANDLE_PATHS` entries that resolve a recycled
  handle to the wrong parent.
- [ ] **`FILE_DELETE_ON_CLOSE` (`0x1000`) appears nowhere in the shim.** On a
  director-served path the delete silently never happens, uncounted.

---

## Theme 3 — Uncounted fall-throughs (these break reconciliation)

`shim routed == director opens_ok + opens_err` cannot hold while these exist, and
they are the live lead on the master-red enumeration test.

- [ ] **`serve_dir_query` returns `passthrough()` for any info class outside
  `{1,2,3,12,37,38}` before the `DIR_TABLE` or root check, with no counter.**
  Classes 50/60/63 hand a synthetic directory handle to real ntdll. **This is the
  standing lead on `directory_enumeration_under_a_managed_root_hides_an_unserved_real_file`**
  (red on master): "the instrument records nothing" is consistent with the
  listing leaving via this exit rather than the director branch. Start here.
- [ ] **`fuse_query_information`'s catch-all returns `STATUS_SUCCESS` with
  `Information=0`** and the caller's buffer never written, uncounted, for every
  unhandled class on a synthetic handle — the outcome `STATUS_HOOK_PANICKED`'s own
  doc rejects as "materially worse".
- [ ] **`qibn_hook` falls through to `tramp`** for an existing under-root path
  whose info class `fill_by_name` declines.

---

## Theme 4 — Shim correctness

- [ ] **`GENERIC_ALL` under a root routes as a read.** `is_write_open` omits it
  while `classify_open` includes it, so the open gets a read-only director
  handle, skips copy-up, and every write fails.
- [ ] **`setinfo_hook`'s under-root sealing is gated on `is_delete || is_rename`,**
  so on a non-synthetic under-root handle `FileEndOfFileInformation` truncates the
  real file and `FileLinkInformation` hardlinks it, with the `path_is_ours`
  backstop never reached.
- [ ] **`copy_up` is TOCTOU.** `!ov.has_file(...)` → `copy_up` races across
  threads and `ShimIoGuard` is thread-local, so two threads seed the same `dest`
  and a loser's `remove_file` can delete the winner's completed seed after the
  winner returned a `Redirect` to it.
- [ ] **`engine.rs:560`'s `let _ = std::fs::remove_file(dest)`** is the only
  guard against a truncated overlay file the director then serves as
  authoritative, and the only discarded `Result` left in the file.
- [ ] **`decide_open` returns a `Redirect` to the path `cow_seed` just deleted**
  for `FILE_OPEN`/`FILE_OVERWRITE`, so a write open reports missing a file a read
  open serves. The comment claiming "the caller's write starts from an empty
  overlay file" is false for those dispositions.
- [ ] **`qibn`/`qattr`/`qfull` decode with provenance-blind `path_of`** and hold
  no `UncachedScope` before hitting the cached `RootMap`, violating the contract
  `create`/`open`/`delete`/`setinfo` all honour.
- [ ] **`cpiw_hook` resumes a child whose injection failed or timed out,**
  failing the "any process in the session" half of the invariant, silently.
- [ ] **Poisoning degrades differently per table and mostly uncounted.**
  `ring_lock` poisoning turns every `readdir` error into `Vec::new()` — silently
  empty directories reported as `ReadDirSource::Director`, the empty-load-order
  shape. `DIR_TABLE` poisoning passes every under-root enumeration through.
- [ ] **Dual-layer dispatch is tested by nothing.** Production always runs
  dual-layer, so `vfs-payload`'s four hooks are the outermost frames for the four
  hottest APIs, while every `vfs-shim/tests/` case uses single-layer `install`. A
  payload `match_redirect` hit calls real ntdll directly, inert only because
  `Session::launch` supplies no static imports.
- [ ] **Sibling spellings** not covered by the NT-prefix fix, per that fixer's
  report: 8.3 short names, `x.esp::$DATA` (sealed rather than served), trailing
  dots/spaces behind `\\?\` (seals rather than leaks), `FILE_OPEN_BY_FILE_ID`
  unrecognised while its binary `ObjectName` is decoded as UTF-16.

---

## Theme 5 — vfs-embed / vfs-compose

- [ ] **`Session` has no write path,** so the crate's own tests reach
  `kernel().open/write/close` 17 times, including `memory_provider_round_trip.rs`
  performing half its flagship round trip past the seam. This is the item that
  most undermines §4's encapsulation claim.
- [ ] **`recompose` drops the roots lock** between reading the composition and
  installing it, so concurrent `mount_at`/`set_write_layer_at` on one root can
  install a stale graph while `composed_roots()` records the lost mount.
- [ ] **`launch(wait: true)` holds `LAUNCH_ENV_LOCK` for the child's entire
  lifetime,** so another session's `serve()` blocks for a whole game run. The
  lock's doc describes it as serialising env writes only.
- [ ] **`read_file_at` does `vec![0u8; size as usize]` from a provider-reported
  size with no bound** — and it is the only read the seam and Node's `readFile`
  offer.
- [ ] **"Never wrap the upper in a `CachingProvider`"** is documented as
  mandatory, enforced nowhere, untested, and undetectable
  (`Capabilities::cached()` passes `access` through unchanged).
- [ ] **`Capabilities::validate()` is called only by the conformance suite and
  the Node path.** A Rust provider declaring `ReadWrite + immutable` mounts fine;
  the spec says such a combination is "rejected at construction".
- [ ] **`casefold` is still missing** — the highest-value gap in §6's catalog.
  The shim folds path components while `MemoryProvider` is case-sensitive, so a
  child's write lands beside a host's seeded file under a different case with no
  diagnostic from anything. Reproduced through the Rust seam, so it is not a Node
  artifact. There is still no Rust-side test recording it, only the Node
  `test.fails` pin and the spec §6b note.
- [ ] **`RouterProvider::readdir` is single-dispatch,** not the union §6
  specifies, and both conformance runs pass empty route lists. Spec now says so;
  the implementation is still owed.
- [ ] **`SeekableProvider::reopen`'s failure path leaves a dangling handle** and
  `REOPEN_MASK` is untested.
- [ ] **§6's mount-time flag table** remains unimplemented apart from the
  `SeqRead` hard error and a `slow`-without-cache warning — and that warning
  lives in the Node binding only, which is the exact anti-pattern §6b names for
  `readonly`/`seekable`. Move it below the binding.

---

## Theme 6 — vfs-cache residue

- [ ] **Sharding makes 12–14% of `ram_budget` unusable** — the default config
  keeps 56 of 64 nominal blocks for a working set that fit exactly before.
  `MIN_BLOCKS_PER_SHARD`'s doc bounds it but nothing prevents, documents, or
  tests it.
- [ ] **`hit_scaling_cost`'s concurrency gate is thin:** measured 0.438–1.458
  over 8 replications on an idle 16-core box against a 0.40 gate, and CI runs it
  in debug on a 4-vCPU runner where the `cores < THREADS` skip does not fire.
  Its comment claiming thresholds "sit an order of magnitude away from both
  outcomes" is false for this gate — defect 0.24, gate 0.40, fixed 0.943 — while
  line 216 of the same file states the accurate 2x figure.
- [ ] **Make CI run the cost tests in `--release`,** or restate what the debug
  run actually gates. (`hit_copy_cost` is allocation-counted and
  build-independent; `hit_scaling_cost` is not.) Docs corrected in `71a3091`;
  the CI change is still owed.
- [ ] **Re-measure the block-size sweep and restore a defensible headline.**
  `block-cache-hit-cost.md`'s `after MiB/s` column is marked `(?)` and its "110x"
  headline withdrawn; only the p50 drop is corroborated.
- [ ] **`stats()` is not a consistent snapshot** and silently omits poisoned
  shards, under-reporting `ram_bytes` in exactly the direction that makes the
  suite's budget assertions pass for the wrong reason.
- [ ] **A poisoned `RwLock` becomes a permanent 1/N cache hole with no signal.**
- [ ] **`bytes_from_source` counts duplicate inserts and blocks never cached** —
  12.5 MiB attributed for 3 refused puts of one 4 MiB block.
- [ ] Coverage: no test exercises a cache/provider block-size mismatch beyond 8x;
  none asserts usable capacity against `ram_budget`.

---

## Theme 7 — Node binding

- [ ] **The panic-containment doc claims in `src/lib.rs` overreach.** `napi`
  2.16.17 ships no `catch_unwind`; `ConformanceTask::compute`/`resolve`, the
  `create_threadsafe_function` closure, and `Drop for Session` are uncontained
  `extern "C"` frames that are panic-*free* today rather than panic-*guarded*.
  And "no `unwrap`, no `expect` on a reachable path" is false —
  `registerProvider(obj, { stallWarnMs: Infinity })` reaches
  `Duration::from_secs_f64` and panics. Add the range check; fix the claim.
- [ ] **`released` means "`releaseProvider` was called", not "the loop is
  alive".** After `worker.terminate()` it reports `false` while every call fails
  `Closing`.
- [ ] **The deadlock guard is checked before the released-tsfn check,** so a
  released main-loop provider reports "would deadlock" instead of "the loop is
  gone".
- [ ] **`__exportStar` getters.** `index.cts` forwards the addon with
  `export * from './native.cjs'`, which `tsc` emits as `__exportStar`, installing
  a **getter** per addon export where the hand-written `index.cjs` had plain own
  properties. A per-access cost the migration introduced that nothing measures.
  If this is worth a perf gate, the project's precedent is tiered:
  deterministic counters asserted tightly, ratios loosely, absolute µs/op
  recorded but asserted only with large headroom — and as a separate `pnpm bench`
  script rather than inside `pnpm test`.
- [ ] **`openTotals()` is always a 2-tuple typed `number[]`,** so
  `[succeeded, failed]` lives only in prose.
- [ ] **`Session` is missing the Node-version-floor prose** that could not
  survive declaration emit (advisory only; all three load-bearing rules did move
  onto their fields).

---

## Theme 8 — Docs and CI residue

- [ ] **README's Rust embedding snippet cannot compile** — `mount(...)?` is
  `Err(i32)` while `serve()?`/`launch()?` are `Err(String)`, and there is no
  enclosing fn. The `vfs-embed/src/lib.rs` doctest it was transcribed from
  carries both missing pieces.
- [ ] **README's Node snippet cannot run** — top-level `await` / `await using` in
  a CommonJS snippet. Every in-repo example wraps it in `async function main()`.
- [ ] **`ci.yml`'s build list is out of step with `ensure_inject_artifacts`'s
  `needed` array,** which its own comment says must match: `needed` includes
  `vfs-fixture-prefs.exe`, which the step does not build.
- [ ] **§8 still asks for an embedded build hash for DLL identity;** none exists,
  and `vfs-node/src/lib.rs` substitutes size+mtime.
- [ ] **§4's crate table lists `vfs-python` as new with no `vfs-node` row.**
  `vfs-python` does not exist; `vfs-node` is the built host.
- [ ] Stale `vfs-director/src/session.rs` citations in `bypass-baseline.md` and
  `escape-matrix.md` (substance re-verified intact at the new location).
- [ ] `[lib] test = false`'s comment overstates; `check-types.cts` vs `.mts`
  naming; `vfs-payload` is unlinted by the clippy invocation whose comment
  implies otherwise.

---

## Theme 9 — Environment

- [ ] **The `DllMain` shutdown stall is live and reproducible.**
  `vfs-fixture-escape` PID 2856 was unkillable during the review — `taskkill`
  reported no such instance while `tasklist` still listed it — holding
  `vfs_shim_dll.dll` mapped and poised to break the next relink. The fix
  direction was previously disproved; the next step is resolving the hung frames
  against the PDB. A trigger now exists, which is what was missing before.

# aether-vfs cleanup pass: follow-up backlog

Date: 2026-10-07. Status: open backlog.

This backlog collects everything the 2026-10-07 cleanup pass deferred or flagged, deduplicated and grouped by importance. The ledger and the per-task reports have the details. The cleanup itself is complete: see `2026-10-07-cleanup.md` and the design spec.

## 1. Correctness and safety

1. **Reads of a director-served file can be silently truncated.** This happens when the file is shorter than the size `OPEN` advertised. A test pins the current behaviour (C8).
2. **`is_write_open` ignores `GENERIC_ALL` and `MAXIMUM_ALLOWED`.** A tool that opens a file under the root this way gets a read-classified synthetic handle. Nothing escapes as a result, but its writes may fail (`vfs-ntlayout/src/disposition.rs`).
3. **Delete-on-close is not honoured on a directory handle created by `try_fuse_mkdir`** (C8).
4. **Delete-on-close fires at the close of the flagged handle, not at the last close.** NT deletes at the last close (C8).
5. **Child-refusal visibility.**
   - Haskill's `playing()` loop can miss a refusal note when the loader exits within one refresh (`haskill/crates/haskill/src/commands/run.rs`).
   - The Windows embed launch adds refusals to the message only on `Err` (`vfs-embed/src/session/windows.rs`).
   - Check whether skse64_loader shows a modal dialog when its child is refused.
6. ~~**`CREATE_SUSPENDED` children.** The child is released from the spin gate and only then re-suspended, so it runs briefly before the caller resumes it.~~ Fixed 2026-10-07: the early payload and its spin gate are gone, and injection never resumes the child's primary thread.
7. **Roots env encoding.** `encode_roots` and the shim's parser have no shared `parse_roots`, and a `;` in a path breaks the encoding (M1).
8. **Registry encode/decode recursion.** In debug builds it overflows a 2 MiB stack at about 600 levels. Give the director threads a larger stack, or make the code iterative (A1).
9. **Production code still uses the system temp dir.** This covers the directord session base, `vfs_embed::Session::new` defaults and the discovery fallback (W4).

Not yet exercised in game: in-game file deletes, which test the delete-on-close fix. The 2026-10-07 final launch stayed at the main menu and deleted nothing. The Wine tests cover the fix.

## 2. Tests and CI

1. **Known Wine failures and flakes:**
   - ~~`static_import*` fail with 0xC0000135.~~ Removed 2026-10-07 with the early payload they tested.
   - `hook_coverage` fails because Wine lacks `NtQueryDirectoryFileEx` and `NtQueryInformationByName`.
   - The `lazy_section` test is flaky.
   - The registry test binary hit a spawn flake (OS error 731) under load.
2. ~~**The payload `Config` ABI offset test never runs.**~~ Moot 2026-10-07: vfs-payload was removed.
3. **Deferred lints:**
   - `unsafe_op_in_unsafe_fn` (about 100 sites);
   - `undocumented_unsafe_blocks` (about 400 sites);
   - per-site SAFETY text to replace the 4 generic texts.
   The registry, `lazy_section` and vfs-inject modules still run without the per-module deny.
4. **rustdoc has about 35 broken or private intra-doc links.** Fix them, then make the CI doc step `-D warnings`.
5. **The /tmp scan has gaps.**
   - Each new directord test binary needs the `TMPDIR` redirect, and nothing checks for it.
   - vfs-redirect and vfs-win are exempt as whole crates.
   - `target/tmp` scratch accumulates.
6. **E23 lint:** forbid `"VFS_…"` string literals outside tests. The read and steam fixtures still use literals.
7. **Missing tests:** a child-exited child-injection path, a connect-level remote `SeqRead` test, and a 32-bit child.
8. **Windows-only runtime paths are only compile-checked from Linux:** the embed `session/windows.rs` and the Windows arm of `stage.rs`.
9. **CI YAML has not yet run on GitHub.** Check the first run.

## 3. Structure and duplication

1. Fold the ring client's `_reporting` submit variants (D20).
2. Spelling rules differ. `MountGraph` keeps the later mount's spelling, while Layered and Overlay keep the bottom's. Decide on one rule, and check the README's "later mounts win".
3. Path normalisation is duplicated in `vfs_compose::path` and vfs-core.
4. The payload cap is hard-coded in the section start (E18).
5. Finish removing SeqRead. `serve.rs` and vfs-embed's `reject_sequential` remain.
6. Optional splits and tidies:
   - `vfs-embed` `proton/mod.rs` (1,400 lines);
   - the escape fixture's `main.rs` (E20);
   - a `delegate_provider!` macro;
   - audit who still uses the vfs-core tree model;
   - the vestigial `root` field in the shim config;
   - the block-store `lifecycle.rs` private snapshot copy.

## 4. Docs and strings

1. **Stale strings:**
   - `vfs-provider/src/model.rs:3` names vfs-shared;
   - `fuse_client::vpath_under_root` appears in `vfs-fixture-escape/src/windows.rs` and in `vfs-directord/tests/escape_matrix.rs`.
2. **`docs/shim-invariants.md`** quotes some renamed module names.
3. **Haskill:** the launch-spike-host comment names the old skyrim-live path.

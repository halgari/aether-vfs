# aether-vfs: code cleanup pass

Date: 2026-10-07
Status: approved by the owner ("spec this work out, implement with subagents"). Defaults for the
four open decisions are set by the rulings in section 4.

## 1. Purpose

After the registry overlay landed, a four-part audit of the whole workspace (132k lines, 39
crates) found 92 cleanup items: dead crates, the same thing defined in several places, files
that are too big to work with, test and CI gaps, stale docs, and six real bugs. This pass fixes
the bugs and carries out the cleanup without changing what the product does.

The audit reports are the detailed evidence (file:line, proposed change, value, effort, risk)
for every item named here:

- `2026-10-07-cleanup-audit/shim.md`: S1–S25 (vfs-shim, vfs-shim-dll, vfs-payload, vfs-inject,
  vfs-pe, vfs-redirect);
- `2026-10-07-cleanup-audit/director.md`: D1–D20 (director, daemon, IPC, protocol, harness);
- `2026-10-07-cleanup-audit/storage.md`: T1–T22 (storage, block store, compose, provider, zip,
  core, registry data);
- `2026-10-07-cleanup-audit/embed.md`: E1–E25 (vfs-embed, vfs-proton, vfs-env, fixtures,
  repo hygiene).

## 2. Goals and non-goals

**Goals**

- Fix the bugs the audit found:
  - **T3:** registry depth cap enforced only on load. A deep tree saves, and then the next load
    sets the whole profile layer aside.
  - **E4:** prefix lock race. The fake-runtime test fails about 3 in 11 when run in parallel,
    and the same race can hit Haskill.
  - **E3:** `VFS_REGISTRY` is not a reserved handshake name.
  - **E9:** the ring-length check never fires in production.
  - **T6:** the crash hook does not really crash.
  - **D1 and S5:** vfs-launch and vfs-redirect do not build on Linux.
- Delete what nothing uses (D1, D8, D10, the seqlock part of D3, dead items).
- Define each repeated thing once:
  - detour table (S2);
  - opcode and status table (D7);
  - env handshake (E3);
  - registry handle resolver (S3);
  - raw NT buffer helpers (S4);
  - payload ABI (S8);
  - overlay markers (T4);
  - codec cursor (D6);
  - path, status and handle-table helpers (T11–T13);
  - Proton test support (E5).
- Split oversized files into modules by concern: S1, S19, S20, E1, E15, T8–T10, T15, D11, D12,
  D14, D15.
- Make tests and CI trustworthy:
  - one Proton test harness and skip policy (E5–E7);
  - CI lints every crate and runs every end-to-end test it can (E8, T6);
  - shim test binaries consolidated (S17).
- Make the docs describe the code (D5, E11, E12, E17, T1, T5, S10, S11).

**Non-goals**

- No behaviour change except the bug fixes listed above and the performance fix T2
  (`stored_name` forwarding).
- No wire or on-disk format change:
  - the ring layout and opcodes keep their numbers;
  - `overlay.reg` keeps format version 1 (the checksum change in T15 is out);
  - the storage format is untouched.
- D16 (folding vfs-control, vfs-source and vfs-directord together) is out. Its TOML and proto
  surface is user-facing.
- No new features.

## 3. Architecture of the work

### 3.1 Streams

The work is split into streams, each owning a disjoint set of crates and files, so streams run in
parallel without editing the same files.

| Stream | Owns | Findings |
|---|---|---|
| **A: storage and registry data** | vfs-storage, vfs-block-store, vfs-compose, vfs-provider, vfs-zip, vfs-core, vfs-registry | T2–T22 (T1 code docs only) |
| **B: director, IPC, protocol** | vfs-director, vfs-directord, vfs-source, vfs-control, vfs-ipc, vfs-protocol, vfs-server, vfs-launch, vfs-ring-harness, vfs-unix, vfs-win, vfs-shared, xtask-descriptor, `resources/`, `bin/regen-protocol` | D1, D2, D3 (seqlock only), D4, D6–D15, D17–D20 |
| **C: shim and injection** | vfs-shim, vfs-shim-dll, vfs-payload, vfs-inject, vfs-pe, vfs-redirect | S1–S25 |
| **M: embed, Proton, env, fixtures** | vfs-embed, vfs-proton, vfs-env, vfs-fixture-*, `bin/build-windows` | E1–E7, E9, E15–E20, E21 (script), E23 |
| **W: docs, CI, workspace** (after A, B, C, M merge) | `rust/docs/`, READMEs, `docs/superpowers/` index and archive, `.github/workflows`, the workspace `Cargo.toml` lints and profiles | D5, E8, E11–E14, E22, T1 (architecture parts), S25 |
| **H: Haskill lockstep** (after M merges) | the Haskill repo, plus the aether API pieces it needs | E10, E21 (shared artefact list), E24, E25, T6 (Haskill crash test), T19, submodule bump |
| **I: integration** (last) | crosses streams | D3 (snapshot made optional, then removed: shim, vfs-shared, vfs-embed), removal of compatibility re-exports kept during the parallel streams, final whole-workspace review, in-game check |

### 3.2 Cross-stream rules

- **Shared files:**
  - Only stream W edits the workspace `rust/Cargo.toml` beyond adding or removing members.
  - Adding or removing members, and the resulting `Cargo.lock` change, is allowed in any stream.
    Conflicts are resolved at merge time by regenerating the lock.
  - A stream that deletes, renames or moves a crate, test or binary also fixes the lines that
    name it in `.github/workflows`, READMEs and `bin/` scripts, so nothing points at a missing
    target. Everything else in those files belongs to stream W.
- **Compatibility re-exports:**
  - A stream that moves or renames an item another stream imports keeps a re-export at the old
    path, marked `#[doc(hidden)]` with a comment naming stream I. For example, B's D7 keeps
    vfs-protocol's `ops` modules for the shim.
  - Stream I removes these re-exports.
- **E3 env handshake:**
  - The env handshake table lives in vfs-env (stream M).
  - Stream M may make the matching edit in vfs-director `ipc.rs` `apply_env_roots`. B does not
    touch that function.
- **Haskill-facing API:**
  - Changes to the API Haskill uses (vfs-embed, vfs-proton, vfs-provider, vfs-storage) are
    additive until stream H.
  - Streams A and M must not break Haskill's build at any merged commit.

### 3.3 Merge and verification

- Each stream works on its own branch in its own git worktree of aether-vfs. Each task ends
  with its crates' tests and clippy passing.
- A stream's branch merges into master once its tasks are done and reviewed, rebased on the
  current master. The full Linux workspace check runs after every merge:
  `cargo test --workspace --exclude <windows-only>` plus clippy.
- Stream C also passes the shim's Wine suite.
- Stream M also passes the Proton end-to-end tests (`proton_launch`, `proton_registry`,
  `proton_fake_runtime`, …) in release mode after `bin/build-windows --release`.
- At the end, stream I runs:
  - the full Linux suite;
  - the Windows cross-build;
  - the Wine suite;
  - the Proton suite;
  - one in-game Journals of Jyggalag launch through Haskill, which must reach the main menu
    with registry key handles bounded and no new prefix registry changes from the game.

## 4. Rulings on the open decisions

1. **S14, the shim-local Engine and overlay path: remove it.**
   - Standalone mode is retired.
   - It is the last task of stream C, after the split, so its full surface is visible.
   - Tests that rely on it are converted to the fake director. A test is dropped only if the
     same behaviour is covered elsewhere, and each one is named in the task report.
   - The Wine suite must stay green.
2. **D3, the snapshot path:**
   - The seqlock goes in stream B.
   - Stream C removed the shim's Engine, so the shim only decoded and validated the config's
     snapshot (and its `overlay` field) and then ignored them. Stream I removes both fields from
     the shim config instead of making them optional: vfs-embed stops sending them, and
     `vfs-shared`, the redirect snapshot decision code and the other snapshot-only code are deleted.
   - The config is now versioned (`"VFSC"` + a `u32` version, in `vfs_protocol::shimcfg`). A config
     with no version header or another version fails shim bootstrap with a `BootstrapError::Config`
     that names both versions. Changing the layout again bumps the version.
   - This is stream I, after B, C and M have merged.
3. **Haskill-facing API changes are in scope** (E10, E24, E25, E21, T19). They land in aether
   first, then in Haskill in the same session (stream H).
4. **Old design docs are archived, not deleted.**
   - Historical specs and plans move with `git mv` to `docs/superpowers/archive/`.
   - `docs/superpowers/README.md` indexes current and archived docs, with one line each.
5. **Low-value items are included only where the audit rates them effort S.** Excluded: D16, the
   T15 checksum v2, and the E20 fixture merges (tidy only).
6. **T17 `SeqRead`:** delete it if nothing outside tests uses it. Otherwise fix the latent bug
   the audit names.

## 5. Process

- The plan is `docs/superpowers/plans/2026-10-07-cleanup.md`. It is executed with
  subagent-driven development: a Sonnet implementer per task and a Sonnet task review.
- Opus is used for:
  - the reviews of S1, S2, S14, D6/D7 and D3, which are behaviour-sensitive or concurrency-
    and ABI-adjacent;
  - each stream's whole-branch review before merge.
- Streams A, B, C and M run at the same time. W and H start once their prerequisites have merged, and I
  runs last.
- Commits are small and either move-only or change-only, never both in one commit, so reviews
  can tell refactor from behaviour.

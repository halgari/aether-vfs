# M4 Part 2 — Full Write Set Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Complete the write set beyond create+write (Part 1): `OP_DELETE` (whiteout), `OP_RENAME`, `OP_MKDIR`, `OP_SETATTR` (truncate) — pure-ring, JVM `overlay` Provider authoritative — and prove each through real injection.

**Architecture:** Mirrors Part 1 exactly. Wire codecs (subagent) + JVM dispatch against aether's `Writable` wrappers (subagent) + shim `setinfo_hook`/create-dir routing to the ring (controller, native) + e2e proofs (controller). The mechanism (`NtWriteFile` hook, `fuse_client`, `try_fuse_create` write flag, virtual write handles) is proven; Part 2 adds the remaining ops on the same rails.

**Tech Stack:** Rust (`vfs-protocol`, `vfs-shim`, `xtask-descriptor`), Clojure + FFM, GitHub Actions.

## Global Constraints
- Pure-ring; JVM `overlay` Provider authoritative. New wire messages golden-pinned + byte-for-byte Clojure conformance. `resources/*.edn` stay `eol=lf`.
- **Temp paths in tests MUST use `java.io.File`/`io/file` join, never `(str tmpdir …)`** — Linux `java.io.tmpdir` has no trailing separator (this bit Part 1).
- Wire formats (LE, mirror Rust↔Clojure): `OP_DELETE(9)` req = `path_utf8` (reuse `encode_path_req`), resp empty. `OP_MKDIR(10)` req = `mode:u32 | path_utf8`, resp empty. `OP_RENAME(8)` req = `from_len:u32 | from_utf8 | to_utf8`, resp empty. `OP_SETATTR(7)` (truncate) req = `fh:u64 | size:u64`, resp empty.
- Commit bodies end with `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`. ≥1 commit/task.

---

## Task 1: wire codecs for mkdir / rename / setattr (Rust + golden + Clojure)

TDD, cross-platform. `OP_DELETE` reuses `encode_path_req` (no new codec). Add codecs for the three that need structure. Mirror the Part-1 `OP_WRITE` codec pattern (`encode_read_req`/`encode_write_req` are the templates).

**Files:** Modify `rust/crates/vfs-protocol/src/lib.rs`, `rust/crates/xtask-descriptor/src/lib.rs`, `resources/protocol-golden.edn`, `src/aether/vfs/wire.clj`, `test/aether/vfs/wire_conformance_test.clj`.

**Interfaces (Rust + Clojure mirror):**
- `MkdirReq { mode:u32 }` + `encode_mkdir_req(mode:u32, path:&str)` = `mode:u32 | path`; `decode_mkdir_req(&[u8]) -> Option<(u32, String)>`.
- `encode_rename_req(from:&str, to:&str)` = `from_len:u32 | from | to`; `decode_rename_req(&[u8]) -> Option<(String, String)>`.
- `SetattrReq { fh:u64, size:u64 }` + `encode_setattr_req(&SetattrReq)` = `fh:u64 | size:u64`; `decode_setattr_req(&[u8]) -> Option<SetattrReq>`.
- Clojure: `wire/encode-mkdir-req [mode path]`/`decode-mkdir-req`, `encode-rename-req [from to]`/`decode-rename-req`, `encode-setattr-req [{:fh :size}]`/`decode-setattr-req`.

- [ ] **Step 1: Rust codecs + roundtrip tests** — mirror `encode_write_req`/`decode_write_req` in `vfs-protocol/src/lib.rs`; add a `#[cfg(test)]` roundtrip for each. Exact layouts per Global Constraints.
- [ ] **Step 2: golden vectors** — in `xtask-descriptor` `golden_vectors()`: `("mkdir-req-mode493-dir", P::encode_mkdir_req(493, "sub/dir"))`, `("rename-req-a-b", P::encode_rename_req("old.txt", "new.txt"))`, `("setattr-req-fh5-size100", P::encode_setattr_req(&vfs_protocol::SetattrReq{fh:5,size:100}))`.
- [ ] **Step 3: regenerate + Rust tests** — `bin/regen-protocol && (cd rust && cargo test -p vfs-protocol -p xtask-descriptor)`; expect PASS, regen idempotent.
- [ ] **Step 4: Clojure codecs + conformance test** — add the six Clojure fns to `wire.clj` (mirror the format; helpers `baos`/`buf`/`put-u32!`/`put-u64!` exist) and a `deftest` in `wire_conformance_test.clj` asserting each `encode-*` hex equals its golden and decoders round-trip. Run `clojure -M:test -n aether.vfs.wire-conformance-test` → PASS.
- [ ] **Step 5: commit** `feat(m4): wire codecs for OP_MKDIR/OP_RENAME/OP_SETATTR (golden-pinned)`.

---

## Task 2: JVM dispatch for delete / rename / mkdir / truncate

TDD, cross-platform (heap segment + `overlay` Writable Provider). Implements the reserved opcodes (currently `BAD_REQUEST`).

**Files:** Modify `src/aether/vfs/os/windows/server.clj`, `test/aether/vfs/os/windows/server_test.clj`.

**Interfaces:** `dispatch` handles `OP_DELETE`→`provider/unlink`, `OP_RENAME`→`provider/rename`, `OP_MKDIR`→`provider/mkdir`, `OP_SETATTR`→`provider/truncate`. Truncate is handle-based (`fh`,`size`) so **`do-open` must store `:vpath` in the fh table** and `do-truncate` maps `fh → vpath → (p/truncate vpath size)`.

- [ ] **Step 1: failing tests** — add deftests to `server_test.clj` (use an `overlay-provider` over a **File-joined** temp overrides dir; assert via the overrides dir on disk / status, NOT a fragile re-open-read — the Part-1 lesson):
  - delete: create `/x.txt`, close, `OP_DELETE /x.txt` → status 0; assert a `.wh.x.txt` whiteout exists in overrides (overlay records deletes as whiteouts).
  - mkdir: `OP_MKDIR /d` (mode 493) → status 0; assert `overrides/d` is a directory.
  - rename: create `/a.txt`, close, `OP_RENAME a.txt→b.txt` → status 0; assert `overrides/b.txt` exists (and/or a.txt whiteout).
  - truncate: create `/t.txt`, write 5 bytes, close; open `/t.txt` write again to get fh, `OP_SETATTR {fh,size:2}` → status 0; assert `overrides/t.txt` length is 2. (Adjust to the overlay's actual truncate semantics; `truncate!` copies-up then sets length.)
  Reconcile the exact overlay behaviors (whiteout naming, rename copy-up) against `src/aether/vfs/providers/overlay.clj`.
- [ ] **Step 2: run → fail** (`BAD_REQUEST`/unresolved).
- [ ] **Step 3: implement** — add `OP-DELETE 9`/`OP-RENAME 8`/`OP-MKDIR 10`/`OP-SETATTR 7`; `do-delete`/`do-rename`/`do-mkdir`/`do-truncate` using `aether.vfs.provider`'s `unlink`/`rename`/`mkdir`/`truncate` wrappers (all path-based; wrap raises via `error/on-not-found`/`:read-only`→status like `do-open`). Store `:vpath` in the fh table in `do-open` (both branches); `do-truncate` resolves `fh`→`:vpath`. Wire all four into `dispatch` before the default. Paths from `wire/decode-*` get `norm-vpath`.
- [ ] **Step 4: run → pass**; **Step 5: commit** `feat(m4): JVM dispatch for delete/rename/mkdir/truncate against Writable overlay`.

---

## Task 3: shim setinfo/create-dir routing to the ring (controller-run, native)

**Controller-run** (native shim work, like Part 1 Task 3). Route the write-set ops to `fuse_client` for under-root virtual handles/paths.

- [ ] **Step 1: `fuse_client` methods** — add `delete(vpath)`, `rename(from, to)`, `mkdir(vpath, mode)`, `truncate(fh, size)` to `fuse_client.rs`, each submitting the matching `OP_*` and checking the (empty) response status. Mirror `open_write`/`write`.
- [ ] **Step 2: `setinfo_hook` routing** — in `hook.rs`, for an under-root virtual handle (`is_fuse_synth` / `vpath_under_root`): `FileDispositionInformation`/`...Ex` (delete) → `fuse_client.delete(vpath)` (instead of the local `engine.whiteout`); `FileRenameInformation`/`...Ex` → `fuse_client.rename`; `FileEndOfFileInformation` (truncate) → `fuse_client.truncate(fh, size)`. Study the existing `setinfo_hook` delete path as the template. The shim needs the vpath for delete/rename — for a fuse_synth handle it has the ring `fh` (and `record_path` tracks the NT path per handle — reuse it to derive the vpath via `vpath_under_root`).
- [ ] **Step 3: `create_hook` directory create** — a create-disposition open with `FILE_DIRECTORY_FILE` under root → `fuse_client.mkdir(vpath, mode)` then open a virtual dir handle (or return success). Study how `try_fuse_create` handles dir opens.
- [ ] **Step 4: build + shim tests** — `cargo build -p vfs-shim-dll` and `cargo test -p vfs-shim` green; read path + Part-1 write still work.
- [ ] **Step 5: record** the shim diff in `.superpowers/sdd/m4p2-shim.md`.

---

## Task 4: end-to-end proofs + CI (controller)

- [ ] **Step 1: fixture(s)** — extend `vfs-fixture-write` (or a new `vfs-fixture-writeset`) to also: delete a file (then a read must fail/miss), rename, mkdir (dir appears), truncate (size shrinks); exit 0 iff all pass. Env-driven.
- [ ] **Step 2: e2e deftest** — add cases to `launch_test.clj` driving the writeset fixture via `launch.clj` + an `overlay` Provider; assert exit 0 and the overrides-dir effects. Windows-only, File-joined temp paths.
- [ ] **Step 3: verify locally** — build artifacts, `clojure -M:test -n aether.vfs.os.windows.launch-test` → all cases pass through real injection.
- [ ] **Step 4: commit** `test(m4): end-to-end delete/rename/mkdir/truncate via injection + CI`. (CI `windows-clojure` already builds the fixtures + runs launch-test.)

---

## Out of scope (Part 2) — tracked follow-ups
- Open-existing-writable without truncate (read-modify-write); bulk-arena writes; F1 anti-drift enum-exhaustive emitter; an `NtOpenFile`-path regression test. These remain separate follow-ups (not blocking the write set).

## Self-Review
Coverage: wire (Task 1), JVM dispatch (Task 2), shim routing (Task 3), e2e+CI (Task 4) for delete/rename/mkdir/truncate. Patterns mirror Part 1 (OP_WRITE codec, do-write dispatch, NtWriteFile hook). Temp-path File-join constraint carried (Part-1 lesson). Truncate's handle→vpath mapping flagged (needs `:vpath` in the fh table). Tasks 1–2 subagent; 3–4 controller.

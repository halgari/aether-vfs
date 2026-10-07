# M4 — Write path (pure-ring, JVM overlay authoritative)

**Date:** 2026-07-28
**Status:** Approved (design); plan pending
**Predecessors:** M1 (merge+anti-drift), M2 (FFM ring server, read), M3 (injection, real hooks, read) — all complete + CI-verified on `master`.
**Parent design:** `docs/superpowers/specs/2026-07-26-unified-cross-platform-vfs-design.md`

## What

Make virtualized files **writable** through the injected shim, pure-ring: the
shim routes create/write/delete/rename/mkdir/truncate to the ring, and the
**JVM's aether `overlay` Provider is authoritative** — it copies a base file up
to a writable overrides dir on first modification, records deletes as `.wh.*`
whiteouts, and never mutates the base. Read-your-writes flows back through the
ring (a subsequent read of a written path is served from the copied-up file via
the same ring). This completes the read+write path on Windows; reads already
work (M2/M3).

**Sequencing (minimal-proof-first, as approved):** prove ONE write end-to-end
first (create + `NtWriteFile` → `OP_WRITE` → JVM overlay → read-back through the
ring), de-risking the shim write-routing, then layer delete/rename/mkdir/
truncate. This spec covers the whole arc; the plan implements Part 1 (spike +
minimal write) first.

## Decisions (as approved)

1. **Pure-ring writes.** All write ops go to the ring; the JVM `overlay`
   Provider does copy-up + whiteouts. The JVM Provider stays the single source
   of truth for the whole tree (reads AND writes). (Rejected: the shim's
   existing *local* disk-overlay engine — it bypasses the Provider.)
2. **Minimal write proof first**, then the rest of the write set.
3. **The genuinely new shim work** (M3 needed none): a new `NtWriteFile` hook
   and routing `NtSetInformationFile`/create-write-disposition to the ring —
   because a pure-ring virtual file has **no real backing handle** for the game
   to write to, so the shim must intercept the write and forward it.

## What exists vs. what's new

- The shim `create_hook` is already write-disposition aware
  (`engine.decide_open(path, access, disposition)`), and `setinfo_hook` already
  turns a delete into a **local** overlay whiteout (`engine.whiteout`). These
  route to the shim's LOCAL overlay engine (copy-up to disk), NOT the ring.
- `fuse_client` is **read-only** (no write methods).
- `vfs-protocol` reserves `OP_WRITE(6)`, `OP_SETATTR(7)`, `OP_RENAME(8)`,
  `OP_DELETE(9)`, `OP_MKDIR(10)` — but there is **no wire codec** for them yet.
- aether's `Writable` protocol + `overlay` provider (copy-up, `.wh.*`
  whiteouts, base never mutated) are **already implemented and tested** on the
  Linux side — the JVM write dispatch reuses them.

## Architecture (one write + read-back)

```
   Target process (injected shim)                         JVM process
   ┌────────────────────────────────────┐                ┌──────────────────────────────┐
   │ NtCreateFile(vpath, WRITE-disp)     │  OP_OPEN(write) │ server: do-open write →       │
   │  → virtual writable fh (no disk)     │───────ring────▶│  overlay/create-file → fh      │
   │ NtWriteFile(fh, buf)  [NEW hook]     │  OP_WRITE       │ server: do-write →            │
   │  → fuse_client.write(fh,off,buf)     │───────ring────▶│  overlay/write-at (copy-up)    │
   │ NtReadFile(fh)  (existing)           │  OP_READ        │ server: read-at → copied-up   │
   │  → reads back the written bytes      │◀──────ring──────│  file bytes (read-your-writes) │
   └────────────────────────────────────┘                └──────────────────────────────┘
```

## Components

### Wire protocol — write codecs (new) + anti-drift
Design + add the wire format for the write opcodes in `vfs-protocol`
(Rust encoders/decoders), pinned by golden vectors, mirrored in
`aether.vfs.wire` (Clojure), all under the existing conformance test:
- `OP_OPEN` write: reuse `encode_open_req` with `OPEN_WRITE` flag (exists);
  the server branches on the flag.
- `OP_WRITE`: `fh:u64 | offset:u64 | len:u32 | pad:u32 | data[len]` (inline);
  large writes use the **bulk arena** (mirror of the read bulk path — the shim
  writes data into an arena bank, `OP_WRITE` carries `arena_offset`, the JVM
  reads it from the arena). Response: `bytes_written:u32`.
- `OP_DELETE`: `path_utf8`. `OP_MKDIR`: `mode:u32 | path_utf8`.
- `OP_RENAME`: `from_len:u32 | from | to_utf8`.
- `OP_SETATTR` (truncate): `fh_or_path` form carrying `size:u64` (choose a
  handle-based `fh:u64 | size:u64` to match `NtSetInformationFile`
  EOF-on-handle; confirm in the spike).
- **F1 anti-drift hardening lands here** (M1 final-review gap): because M4
  touches the opcode/message set, convert the `xtask-descriptor` opcode/status
  catalog to be driven by an exhaustive Rust source (an `enum` + `match`, or an
  asserted element count) so that **adding** an opcode fails the build until the
  emitter (and Clojure mirror) are updated — closing the "additive changes
  escape the gate" hole.

### Rust shim — `fuse_client` write methods + hook routing
- `fuse_client`: add `write(fh, offset, &[u8]) -> Result<usize,i32>` (inline +
  bulk arena), `create(vpath, flags) -> OpenResp`, `delete(vpath)`,
  `rename(from, to)`, `mkdir(vpath, mode)`, `truncate(fh, size)` — each submits
  the matching `OP_*` and decodes the response.
- Hooks (route to `fuse_client` for under-root virtual paths, pure-ring):
  - `create_hook`: a WRITE-disposition open of an under-root vpath →
    `fuse_client.create`/write-open → return a **virtual writable handle** (same
    virtual-handle machinery the read path uses for virtual opens).
  - **`NtWriteFile` hook (NEW)**: for a virtual write handle →
    `fuse_client.write`. (Symmetric to the existing `NtReadFile` hook.)
  - `setinfo_hook`: for an under-root virtual handle, route delete →
    `fuse_client.delete`, rename → `fuse_client.rename`, EOF/truncate →
    `fuse_client.truncate` (instead of the local `engine.whiteout`).
  - directory create → `fuse_client.mkdir`.
- These are gated to the pure-ring/fuse-authoritative path; the local overlay
  engine path is untouched for non-ring configs.

### JVM server — write dispatch (reuse aether `Writable`)
Implement the reserved opcodes in `server.clj` `dispatch` (they currently hit
`BAD_REQUEST`), against `aether.vfs.provider`'s `Writable` wrappers:
- write-disposition `OP_OPEN` → `provider/create` (or open-writable) → fh table.
- `OP_WRITE` → `provider/write-at` (inline or arena-sourced data).
- `OP_DELETE` → `provider/unlink`; `OP_RENAME` → `provider/rename`;
  `OP_MKDIR` → `provider/mkdir`; `OP_SETATTR` truncate → `provider/truncate`.
Errors map to `ST_*` (e.g. `:read-only` → a defined status).

### Overlay Provider on the launch
`launch.clj` (M3) currently serves an inline (read-only) Provider. M4 lets the
caller pass a **writable** Provider — aether's `overlay-provider` (or
`compose/build-data-root`): base read-only under a writable overrides host dir.
Writes copy-up into the overrides dir; reads merge upper-over-base.

### Write fixture + e2e proof
- Part 1: `vfs-fixture-write` — creates `%VFS_FIXTURE_PATH%`, writes known
  bytes, closes, reopens, reads them back, asserts equality; exit 0/1. Proves
  create + write + read-your-writes through the hooks over the ring.
- Part 2: extend for delete (then a read must miss / whiteout), rename (old
  path gone, new path reads), mkdir (dir appears in readdir), truncate (size
  shrinks). Windows-only, wired into the `windows-clojure` CI job.

## De-risk spike (Part 1, controller-run — like M3)

Before productionizing, prove the minimal write pure-ring with a crude JVM
driver + the shim: inject the write fixture, have it create+write+read-back a
virtual path, and confirm the bytes land in the JVM overlay and read back. The
spike determines the exact shim changes needed for the write hooks (the
`NtWriteFile` hook + virtual-write-handle + create-write routing), the same way
the M3 spike pinned the read path. Outcome: the confirmed minimal shim diff +
the working wire/JVM shape, which Part 1 productionizes.

## Testing & CI
- Wire write codecs: golden-pinned (Rust) + byte-for-byte Clojure conformance
  (extends the M1/M2 anti-drift tests). F1 hardening gets its own check
  (adding a dummy opcode must fail the emitter build/test).
- JVM write dispatch: cross-platform heap-segment tests (like M2 server tests)
  with an `overlay` Provider — create/write/read-back, delete/whiteout, rename,
  mkdir, truncate.
- End-to-end (Windows-only, `windows-clojure` CI): the write fixture through
  real injection via `launch.clj` with an overlay Provider.
- Load-safety, `.gitattributes` LF, drift gate — unchanged constraints.

## Risks
- **The shim write-routing** (the one real unknown) — front-loaded as the Part 1
  spike; the new `NtWriteFile` hook + virtual-write-handle are the crux. If the
  existing virtual-handle machinery doesn't cover write handles, the spike scopes
  the addition.
- **Copy-up base source in pure-ring**: aether's `overlay` copies a base file up
  on first write; over the ring the "base" is the underlying read Provider, so
  copy-up reads the base via the Provider and writes to the overrides dir — must
  confirm `overlay-provider` works with a non-filesystem base (it's designed to;
  Linux uses it over snapshots).
- **Bulk write** symmetry with bulk read — arena reused; confirm the arena
  bank ownership protocol handles shim-writes-into-arena (server reads) as well
  as server-writes-into-arena (shim reads).
- **`NtSetInformationFile` class coverage** — delete (13/64), rename (10/65),
  EOF (20); the spike/plan enumerates the exact classes.

## Milestones (Part 1 = minimal write + spike; Part 2 = full write set)

**Part 1 — minimal write proof:**
1. De-risk spike (controller): create+write+read-back a virtual file pure-ring;
   pin the shim write-hook diff.
2. `OP_WRITE` wire codec (Rust + golden + Clojure) + **F1 enum-exhaustive
   emitter** hardening.
3. JVM server: write-disposition `OP_OPEN` + `OP_WRITE` against `Writable`.
4. Shim: `NtWriteFile` hook + create-write routing + `fuse_client.write`/create.
5. `vfs-fixture-write` + `launch.clj` overlay-Provider support + e2e (create+
   write+read-back) in CI.

**Part 2 — the rest of the write set:**
6. `OP_DELETE` (whiteout), `OP_RENAME`, `OP_MKDIR`, `OP_SETATTR` (truncate) —
   wire codecs, JVM dispatch, shim `setinfo_hook`/mkdir routing, e2e per op.

## Out of scope (unchanged)
- Child-process propagation (later). Unified `mount`/`launch` entry (M5).
- OS event notifier (spin only). macOS. Full game launch (SKSE/zip).
- The shim's LOCAL overlay engine (kept for non-ring configs; not removed).

# vfs-server Request Dispatch — Design Spec

**Status:** Approved-to-proceed (user delegated the full cycle), ready for
implementation planning.
**Date:** 2026-07-13
**Slice:** Fourth implementable slice — the `vfs-server` **request dispatch and
protocol encoding**: the FUSE-like message payload format for a starter opcode
set, a dispatcher that answers from the authoritative `vfs-core` tree, and a
`Server` that serves `vfs-ipc` requests and publishes a `vfs-shared` snapshot.
**Parent docs:** *Out-of-Process (IPC) Architecture* (§2, §7, §9), *Rust
Implementation Guide* (§3).
**Depends on:** `vfs-core`, `vfs-shared` (feature `bridge`), `vfs-ipc` — all done.

---

## 1. Context & positioning

`vfs-server` is the authoritative side of the out-of-process design: it runs
`vfs-core`, is the **sole writer** of the `vfs-shared` snapshot, and services
requests over the `vfs-ipc` control ring (IPC §2). This is the first crate that
composes all three prior slices into a working request→response path.

`vfs-ipc` carries an opaque `(opcode, payload)`; this slice defines the concrete
**payload encoding** for the FUSE-like message catalog (IPC §7) — starting with
`GETATTR`, `READDIR`, and `HEARTBEAT` — plus the dispatcher that turns a request
into a `vfs-core` query and back into bytes.

### Scope decisions (consistent with slices 1–3)

1. **Dispatch + encoding only.** A starter opcode set served from the
   authoritative `vfs-core` tree, plus snapshot publication. No mutation, no
   materialize, no OS handles.
2. **OS-independent, stable Rust, `#![forbid(unsafe_code)]`.** All `unsafe` stays
   in `vfs-ipc`'s `SharedSeg`; `vfs-server` has none.
3. **Single server thread (MVP).** `Server::serve_one` handles one request; the
   caller loops. The worker pool is deferred.
4. **Read-only opcodes only.** `GETATTR`, `READDIR`, `HEARTBEAT`. Mutation
   opcodes wait on `vfs-core`'s runtime-mutation slice.
5. **Server publishes the snapshot.** `Server::snapshot()` flattens the tree via
   `vfs-shared`'s `bridge` — demonstrating the sole-writer role and exercising
   the whole stack. (In production the shim reads that snapshot directly for the
   zero-round-trip hot path; server-side `GETATTR`/`READDIR` over IPC is the
   miss/fallback path.)

---

## 2. Scope & crate boundary

`crates/vfs-server`, stable Rust, `#![forbid(unsafe_code)]`.

### In scope

- `proto` — request/response payload encode/decode for `GETATTR`, `READDIR`,
  `HEARTBEAT`; status constants. Pure, robust (`Option` on malformed input), LE,
  length-prefixed.
- `handler::dispatch(tree, opcode, payload) -> (status, Vec<u8>)` — decode →
  `vfs-core` query → encode. Unknown opcode / malformed payload → `ST_BAD_REQUEST`.
- `Server` — owns a `vfs-core::VfsTree`; `from_layers`, `handle`, `serve_one`
  (over a `vfs-ipc::RingServer`), and `snapshot()` (via `vfs-shared` `bridge`).
- End-to-end threaded test: a `RingClient` submits requests; a `Server` thread
  answers from a `vfs-core` tree over a real ring; plus a snapshot-matches-core
  test.

### Explicitly out of scope (later slices)

- **`MATERIALIZE` / handle duplication** and the hydration cache (Windows-specific).
- Mutation opcodes (`WRITE`/`RENAME`/`DELETE`/`MKDIR`/`SETATTR`) — need
  `vfs-core` runtime mutation first.
- The real Nt `Notifier`, worker pool, process registry, `REGISTER_PROCESS`.
- Paged/bulk responses beyond `payload_cap` (readdir of a huge dir); MVP caps a
  response at the ring's `payload_cap` and signals overflow via the ring status.
- The director, injection, and shim.

---

## 3. Protocol encoding (`proto`)

All little-endian, length-prefixed, and decode-robust (never panic; return
`None` on malformed input, since payloads arrive over IPC).

**Status codes** (the ring `status: i32`):
```
ST_OK = 0, ST_NOT_FOUND = -1, ST_NOT_A_DIRECTORY = -2, ST_BAD_REQUEST = -3
```

**Requests** — for `GETATTR`/`READDIR` the payload is simply the virtual path as
UTF-8 (`encode_path_req`/`decode_path_req`). `HEARTBEAT` has an empty payload.

**`GETATTR` response** (fixed 18 bytes):
```
found:u8, is_dir:u8, size:u64, mtime:i64
```
`AttrResp { found, is_dir, size, mtime }`. "not found" is a valid answer
(`found=0`), so `GETATTR` always returns `ST_OK`.

**`READDIR` response** — on success `ST_OK` + entries; on error the status
carries it (`ST_NOT_A_DIRECTORY`, `ST_NOT_FOUND`) with an empty payload:
```
count:u32, then per entry:
  name_len:u32, name:[u8; name_len], is_dir:u8, size:u64, mtime:i64
```
`Vec<DirEntryWire>` where `DirEntryWire { name, is_dir, size, mtime }`.

`AttrResp` and `DirEntryWire` derive `Debug, Clone, PartialEq, Eq` (used in
`assert_eq!`).

---

## 4. Dispatcher (`handler`)

```rust
pub fn dispatch(tree: &vfs_core::VfsTree, opcode: u32, payload: &[u8]) -> (i32, Vec<u8>);
```

- `OP_GETATTR` → decode vpath; `tree.getattr(&vp)` → `AttrResp` (found/kind/size/
  mtime, or `found=false`); `(ST_OK, encode)`.
- `OP_READDIR` → decode vpath; `tree.readdir(&vp, None)` → `Ok(entries)` →
  `(ST_OK, encode)`; `Err(NotADirectory)` → `(ST_NOT_A_DIRECTORY, [])`;
  `Err(NotFound)` → `(ST_NOT_FOUND, [])`.
- `OP_HEARTBEAT` → `(ST_OK, [])`.
- decode failure or unknown opcode → `(ST_BAD_REQUEST, [])`.

Opcode constants come from `vfs_ipc::layout` (`OP_GETATTR`, `OP_READDIR`,
`OP_HEARTBEAT`). The dispatcher is pure and total (never panics).

---

## 5. `Server`

```rust
pub struct Server { /* tree: vfs_core::VfsTree */ }

impl Server {
    pub fn new(tree: vfs_core::VfsTree) -> Self;
    pub fn from_layers(layers: Vec<vfs_core::Layer>) -> Result<Self, vfs_core::BuildError>;
    pub fn tree(&self) -> &vfs_core::VfsTree;

    /// Decode+answer a single (opcode, payload). Pure; delegates to `dispatch`.
    pub fn handle(&self, opcode: u32, payload: &[u8]) -> (i32, Vec<u8>);

    /// Serve one request off a vfs-ipc RingServer (Ok(true) if one was handled).
    pub fn serve_one<N: vfs_ipc::Notifier>(
        &self, ring: &vfs_ipc::RingServer<'_, N>,
    ) -> Result<bool, vfs_ipc::IpcError>;

    /// Publish the authoritative tree as a vfs-shared snapshot image.
    pub fn snapshot(&self) -> Vec<u8>;
}
```

`serve_one` wires the ring to the dispatcher:
`ring.serve_one(|req| self.handle(req.opcode, &req.payload))`. The caller runs the
loop (single-threaded MVP). `Server` is `Sync` (its `VfsTree` is), so it can be
shared across a server thread and the main thread in tests.

---

## 6. Error handling & testing

### Errors

No panics. `proto` decoders return `Option`; the dispatcher maps failures to
`ST_BAD_REQUEST`. `serve_one` propagates `vfs_ipc::IpcError`. `from_layers`
propagates `vfs_core::BuildError`.

### Testing

- **`proto` round-trips:** encode→decode for `AttrResp`, `Vec<DirEntryWire>`,
  path requests; truncated/short payloads decode to `None` (no panic).
- **`dispatch` unit tests** (against a `vfs-core` fixture tree): `GETATTR` hit
  (file + dir) and miss; `READDIR` of a dir (merged, ordered), of a file
  (`ST_NOT_A_DIRECTORY`), of a missing path (`ST_NOT_FOUND`); `HEARTBEAT`;
  unknown opcode and malformed payload (`ST_BAD_REQUEST`).
- **`Server` unit tests:** `handle` matches `dispatch`; `snapshot()` opens with
  `vfs_shared::SnapshotReader` and its answers match `Server::tree()`.
- **End-to-end threaded test** (`tests/e2e.rs`): build a `Server` from layers;
  `init` a ring on an `OwnedSeg`; spawn a server thread looping `serve_one`; a
  client thread submits `GETATTR`/`READDIR`/`HEARTBEAT` via `RingClient`, decodes
  responses with `proto`, and asserts they match `vfs-core`'s answers — proving
  the full `vfs-core`↔`vfs-shared`↔`vfs-ipc`↔`vfs-server` path. Uses
  `SpinNotifier`; no `unsafe` (the `SharedSeg` is shared via `OwnedSeg` +
  `thread::scope`).

---

## 7. Dependencies & toolchain

- **Toolchain:** stable Rust.
- **Dependencies:** `vfs-core`, `vfs-shared` (feature `bridge`), `vfs-ipc` (all
  path deps). No external crates.
- **Unsafe:** `#![forbid(unsafe_code)]` (none in this crate).
- **Workspace:** add `crates/vfs-server` to `members`.

---

## 8. Out-of-scope reminders (keep the slice tight)

- No `MATERIALIZE`, no handles, no hydration cache.
- No mutation opcodes.
- No real Nt `Notifier`, no worker pool, no process registry.
- No paging of oversize responses (cap at `payload_cap`).
- No director/injection/shim.

*End of spec.*

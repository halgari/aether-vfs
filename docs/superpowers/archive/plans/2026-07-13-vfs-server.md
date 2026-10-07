# vfs-server Request Dispatch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `vfs-server` — the integration crate that composes `vfs-core`, `vfs-shared`, and `vfs-ipc`: protocol payload encoding, a dispatcher answering from the authoritative `vfs-core` tree, and a `Server` that serves ring requests and publishes a `vfs-shared` snapshot.

**Architecture:** A new workspace crate `crates/vfs-server` (`#![forbid(unsafe_code)]`). `proto` encodes/decodes request/response payloads (LE, length-prefixed, decode-robust). `handler::dispatch` maps an `(opcode, payload)` to a `vfs-core` query and back to bytes. `Server` owns a `VfsTree`, exposes `serve_one` over a `vfs-ipc::RingServer`, and `snapshot()` via `vfs-shared`'s `bridge`.

**Tech Stack:** Rust (stable). Deps: `vfs-core`, `vfs-shared` (feature `bridge`), `vfs-ipc`.

## Global Constraints

- **Toolchain:** stable Rust.
- **Unsafe:** `#![forbid(unsafe_code)]` — none in this crate (all `unsafe` stays in `vfs-ipc`'s `SharedSeg`).
- **Read-only opcodes:** `GETATTR`, `READDIR`, `HEARTBEAT` only.
- **Decode-robust:** every `proto` decoder returns `Option` and never panics or pre-allocates from an untrusted count.
- **Status codes:** `ST_OK=0, ST_NOT_FOUND=-1, ST_NOT_A_DIRECTORY=-2, ST_BAD_REQUEST=-3`.
- **Opcode constants:** from `vfs_ipc::layout` (`OP_GETATTR`, `OP_READDIR`, `OP_HEARTBEAT`).
- **Derive discipline:** any type compared with `assert_eq!` derives `Debug, PartialEq, Eq` (`AttrResp`, `DirEntryWire`).

## Parallelization note

Sequential (one crate, shared `lib.rs`).

---

### Task 1: Scaffold `vfs-server` crate

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Create: `crates/vfs-server/Cargo.toml`, `src/lib.rs`, and placeholders `src/proto.rs`, `src/handler.rs`, `src/server.rs`

**Interfaces:**
- Produces: a compiling `vfs-server` library, `#![forbid(unsafe_code)]`, modules declared.

- [ ] **Step 1: Add to workspace members**

Edit root `Cargo.toml`:
```toml
[workspace]
resolver = "2"
members = ["crates/vfs-core", "crates/vfs-shared", "crates/vfs-ipc", "crates/vfs-server"]
```

- [ ] **Step 2: Create `crates/vfs-server/Cargo.toml`**
```toml
[package]
name = "vfs-server"
version = "0.1.0"
edition = "2021"

[dependencies]
vfs-core = { path = "../vfs-core" }
vfs-shared = { path = "../vfs-shared", features = ["bridge"] }
vfs-ipc = { path = "../vfs-ipc" }
```

- [ ] **Step 3: Create `crates/vfs-server/src/lib.rs`**
```rust
#![forbid(unsafe_code)]
//! `vfs-server`: authoritative side of the out-of-process VFS. Runs `vfs-core`,
//! publishes the `vfs-shared` snapshot, and services `vfs-ipc` requests.

pub mod handler;
pub mod proto;
pub mod server;

// pub use server::Server;  // uncommented in Task 4
```

- [ ] **Step 4: Create placeholder module files**
- `src/proto.rs` → `//! Protocol payload encoding for the message catalog.`
- `src/handler.rs` → `//! Opcode dispatcher.`
- `src/server.rs` → `//! The authoritative Server.`

- [ ] **Step 5: Verify build**

Run: `cargo build -p vfs-server`
Expected: compiles clean.

- [ ] **Step 6: Commit**
```bash
git add Cargo.toml crates/vfs-server
git commit -m "chore: scaffold vfs-server crate"
```

---

### Task 2: Protocol encoding (`proto.rs`)

**Files:**
- Modify: `crates/vfs-server/src/proto.rs`

**Interfaces:**
- Produces: status consts `ST_*`; `AttrResp`, `DirEntryWire`; `encode_path_req`/`decode_path_req`, `encode_getattr_resp`/`decode_getattr_resp`, `encode_readdir_resp`/`decode_readdir_resp`.

- [ ] **Step 1: Write the code + tests**

Append to `crates/vfs-server/src/proto.rs`:
```rust
pub const ST_OK: i32 = 0;
pub const ST_NOT_FOUND: i32 = -1;
pub const ST_NOT_A_DIRECTORY: i32 = -2;
pub const ST_BAD_REQUEST: i32 = -3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttrResp {
    pub found: bool,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntryWire {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
}

pub fn encode_path_req(vpath: &str) -> Vec<u8> {
    vpath.as_bytes().to_vec()
}

pub fn decode_path_req(payload: &[u8]) -> Option<String> {
    core::str::from_utf8(payload).ok().map(|s| s.to_string())
}

pub fn encode_getattr_resp(r: &AttrResp) -> Vec<u8> {
    let mut b = Vec::with_capacity(18);
    b.push(r.found as u8);
    b.push(r.is_dir as u8);
    b.extend_from_slice(&r.size.to_le_bytes());
    b.extend_from_slice(&r.mtime.to_le_bytes());
    b
}

pub fn decode_getattr_resp(p: &[u8]) -> Option<AttrResp> {
    if p.len() < 18 {
        return None;
    }
    let size = u64::from_le_bytes(p[2..10].try_into().ok()?);
    let mtime = i64::from_le_bytes(p[10..18].try_into().ok()?);
    Some(AttrResp { found: p[0] != 0, is_dir: p[1] != 0, size, mtime })
}

pub fn encode_readdir_resp(entries: &[DirEntryWire]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        let name = e.name.as_bytes();
        b.extend_from_slice(&(name.len() as u32).to_le_bytes());
        b.extend_from_slice(name);
        b.push(e.is_dir as u8);
        b.extend_from_slice(&e.size.to_le_bytes());
        b.extend_from_slice(&e.mtime.to_le_bytes());
    }
    b
}

fn take_u32(p: &[u8], off: &mut usize) -> Option<u32> {
    let end = off.checked_add(4)?;
    let s = p.get(*off..end)?;
    *off = end;
    Some(u32::from_le_bytes(s.try_into().ok()?))
}
fn take_u64(p: &[u8], off: &mut usize) -> Option<u64> {
    let end = off.checked_add(8)?;
    let s = p.get(*off..end)?;
    *off = end;
    Some(u64::from_le_bytes(s.try_into().ok()?))
}
fn take_u8(p: &[u8], off: &mut usize) -> Option<u8> {
    let v = *p.get(*off)?;
    *off += 1;
    Some(v)
}

pub fn decode_readdir_resp(p: &[u8]) -> Option<Vec<DirEntryWire>> {
    let mut off = 0usize;
    let count = take_u32(p, &mut off)?;
    // Do NOT pre-allocate from an untrusted count.
    let mut out = Vec::new();
    for _ in 0..count {
        let nlen = take_u32(p, &mut off)? as usize;
        let end = off.checked_add(nlen)?;
        let name = core::str::from_utf8(p.get(off..end)?).ok()?.to_string();
        off = end;
        let is_dir = take_u8(p, &mut off)? != 0;
        let size = take_u64(p, &mut off)?;
        let mtime = take_u64(p, &mut off)? as i64;
        out.push(DirEntryWire { name, is_dir, size, mtime });
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn getattr_resp_roundtrip() {
        let r = AttrResp { found: true, is_dir: false, size: 123, mtime: -7 };
        assert_eq!(decode_getattr_resp(&encode_getattr_resp(&r)), Some(r));
        let nf = AttrResp { found: false, is_dir: false, size: 0, mtime: 0 };
        assert_eq!(decode_getattr_resp(&encode_getattr_resp(&nf)), Some(nf));
    }

    #[test]
    fn getattr_resp_short_is_none() {
        assert_eq!(decode_getattr_resp(&[1, 0, 0]), None);
        assert_eq!(decode_getattr_resp(&[]), None);
    }

    #[test]
    fn readdir_resp_roundtrip() {
        let entries = vec![
            DirEntryWire { name: "a.esp".into(), is_dir: false, size: 10, mtime: 1 },
            DirEntryWire { name: "sub".into(), is_dir: true, size: 0, mtime: 0 },
        ];
        assert_eq!(decode_readdir_resp(&encode_readdir_resp(&entries)), Some(entries));
    }

    #[test]
    fn empty_readdir_roundtrips() {
        assert_eq!(decode_readdir_resp(&encode_readdir_resp(&[])), Some(vec![]));
    }

    #[test]
    fn readdir_resp_truncated_is_none() {
        let entries = vec![DirEntryWire { name: "abc".into(), is_dir: false, size: 5, mtime: 2 }];
        let mut enc = encode_readdir_resp(&entries);
        enc.truncate(enc.len() - 3);
        assert_eq!(decode_readdir_resp(&enc), None);
    }

    #[test]
    fn path_req_roundtrip() {
        assert_eq!(decode_path_req(&encode_path_req("data/a.esp")), Some("data/a.esp".to_string()));
        // invalid UTF-8 → None
        assert_eq!(decode_path_req(&[0xFF, 0xFE]), None);
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p vfs-server proto`
Expected: PASS (6 tests). If any fails, STOP and report.

- [ ] **Step 3: Commit**
```bash
git add crates/vfs-server/src/proto.rs
git commit -m "feat(vfs-server): protocol payload encoding"
```

---

### Task 3: Dispatcher (`handler.rs`)

**Files:**
- Modify: `crates/vfs-server/src/handler.rs`

**Interfaces:**
- Consumes: `vfs_core::{VfsTree, NodeKind, VfsError}`, `vfs_ipc::layout::OP_*`, `crate::proto::*`.
- Produces: `pub fn dispatch(tree: &VfsTree, opcode: u32, payload: &[u8]) -> (i32, Vec<u8>)`.

- [ ] **Step 1: Write the code + tests**

Append to `crates/vfs-server/src/handler.rs`:
```rust
use vfs_core::{NodeKind, VfsError, VfsTree};
use vfs_ipc::layout::{OP_GETATTR, OP_HEARTBEAT, OP_READDIR};

use crate::proto::*;

/// Decode a request, query the authoritative tree, encode a response.
/// Pure and total — never panics.
pub fn dispatch(tree: &VfsTree, opcode: u32, payload: &[u8]) -> (i32, Vec<u8>) {
    match opcode {
        OP_GETATTR => match decode_path_req(payload) {
            Some(vp) => {
                let resp = match tree.getattr(&vp) {
                    Some(s) => AttrResp {
                        found: true,
                        is_dir: s.kind == NodeKind::Dir,
                        size: s.size,
                        mtime: s.mtime,
                    },
                    None => AttrResp { found: false, is_dir: false, size: 0, mtime: 0 },
                };
                (ST_OK, encode_getattr_resp(&resp))
            }
            None => (ST_BAD_REQUEST, Vec::new()),
        },
        OP_READDIR => match decode_path_req(payload) {
            Some(vp) => match tree.readdir(&vp, None) {
                Ok(entries) => {
                    let wire: Vec<DirEntryWire> = entries
                        .into_iter()
                        .map(|e| DirEntryWire {
                            name: e.name,
                            is_dir: e.kind == NodeKind::Dir,
                            size: e.size,
                            mtime: e.mtime,
                        })
                        .collect();
                    (ST_OK, encode_readdir_resp(&wire))
                }
                Err(VfsError::NotADirectory) => (ST_NOT_A_DIRECTORY, Vec::new()),
                Err(VfsError::NotFound) => (ST_NOT_FOUND, Vec::new()),
            },
            None => (ST_BAD_REQUEST, Vec::new()),
        },
        OP_HEARTBEAT => (ST_OK, Vec::new()),
        _ => (ST_BAD_REQUEST, Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};

    fn tree() -> VfsTree {
        let e = |vpath: &str, source: &str, size: u64, mtime: i64| InputEntry {
            vpath: vpath.into(),
            kind: EntryKind::File,
            source: source.into(),
            size,
            mtime,
        };
        build(vec![Layer {
            id: LayerId(0),
            entries: vec![e("data/a.esp", "s/a", 10, 1), e("data/b.esp", "s/b", 20, 2)],
        }])
        .unwrap()
    }

    #[test]
    fn getattr_hit_dir_and_miss() {
        let t = tree();
        let (st, p) = dispatch(&t, OP_GETATTR, &encode_path_req("data/a.esp"));
        assert_eq!(st, ST_OK);
        let a = decode_getattr_resp(&p).unwrap();
        assert!(a.found && !a.is_dir && a.size == 10 && a.mtime == 1);

        let (_, p) = dispatch(&t, OP_GETATTR, &encode_path_req("data"));
        assert!(decode_getattr_resp(&p).unwrap().is_dir);

        let (_, p) = dispatch(&t, OP_GETATTR, &encode_path_req("nope"));
        assert!(!decode_getattr_resp(&p).unwrap().found);
    }

    #[test]
    fn readdir_dir_file_and_missing() {
        let t = tree();
        let (st, p) = dispatch(&t, OP_READDIR, &encode_path_req("data"));
        assert_eq!(st, ST_OK);
        let names: Vec<String> =
            decode_readdir_resp(&p).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["a.esp", "b.esp"]);

        assert_eq!(dispatch(&t, OP_READDIR, &encode_path_req("data/a.esp")).0, ST_NOT_A_DIRECTORY);
        assert_eq!(dispatch(&t, OP_READDIR, &encode_path_req("nope")).0, ST_NOT_FOUND);
    }

    #[test]
    fn heartbeat_unknown_and_malformed() {
        let t = tree();
        assert_eq!(dispatch(&t, OP_HEARTBEAT, &[]), (ST_OK, Vec::new()));
        assert_eq!(dispatch(&t, 9999, &[]), (ST_BAD_REQUEST, Vec::new()));
        assert_eq!(dispatch(&t, OP_GETATTR, &[0xFF, 0xFE]).0, ST_BAD_REQUEST);
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p vfs-server handler`
Expected: PASS (3 tests). If any fails, STOP and report.

- [ ] **Step 3: Commit**
```bash
git add crates/vfs-server/src/handler.rs
git commit -m "feat(vfs-server): opcode dispatcher over vfs-core"
```

---

### Task 4: `Server` (`server.rs`)

**Files:**
- Modify: `crates/vfs-server/src/server.rs`
- Modify: `crates/vfs-server/src/lib.rs` (uncomment `pub use server::Server;`)

**Interfaces:**
- Consumes: `vfs_core::{build, BuildError, Layer, VfsTree}`, `vfs_ipc::{Notifier, RingServer, IpcError}`, `crate::handler::dispatch`, `vfs_shared::bridge::flatten`.
- Produces: `Server` with `new`, `from_layers`, `tree`, `handle`, `serve_one`, `snapshot`.

- [ ] **Step 1: Write the code + tests**

Append to `crates/vfs-server/src/server.rs`:
```rust
use vfs_core::{build, BuildError, Layer, VfsTree};
use vfs_ipc::ring::IpcError;
use vfs_ipc::{Notifier, RingServer};

use crate::handler::dispatch;

/// The authoritative server: owns the merged tree, answers ring requests, and
/// publishes the shared-memory snapshot.
pub struct Server {
    tree: VfsTree,
}

impl Server {
    pub fn new(tree: VfsTree) -> Self {
        Server { tree }
    }

    pub fn from_layers(layers: Vec<Layer>) -> Result<Self, BuildError> {
        Ok(Server { tree: build(layers)? })
    }

    pub fn tree(&self) -> &VfsTree {
        &self.tree
    }

    /// Decode + answer one (opcode, payload).
    pub fn handle(&self, opcode: u32, payload: &[u8]) -> (i32, Vec<u8>) {
        dispatch(&self.tree, opcode, payload)
    }

    /// Serve one request off a ring (Ok(true) if one was handled).
    pub fn serve_one<N: Notifier>(&self, ring: &RingServer<'_, N>) -> Result<bool, IpcError> {
        ring.serve_one(|req| self.handle(req.opcode, &req.payload))
    }

    /// Publish the authoritative tree as a vfs-shared snapshot image.
    pub fn snapshot(&self) -> Vec<u8> {
        vfs_shared::bridge::flatten(&self.tree)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs_core::{EntryKind, InputEntry, LayerId};
    use vfs_ipc::layout::OP_GETATTR;
    use vfs_shared::{SnapResolution, SnapshotReader};

    use crate::proto::{decode_getattr_resp, encode_path_req};

    fn server() -> Server {
        Server::from_layers(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "data/a.esp".into(),
                kind: EntryKind::File,
                source: "s/a".into(),
                size: 10,
                mtime: 1,
            }],
        }])
        .unwrap()
    }

    #[test]
    fn handle_delegates_to_dispatch() {
        let s = server();
        let (st, p) = s.handle(OP_GETATTR, &encode_path_req("data/a.esp"));
        assert_eq!(st, 0);
        assert!(decode_getattr_resp(&p).unwrap().found);
    }

    #[test]
    fn snapshot_matches_tree() {
        let s = server();
        let img = s.snapshot();
        let r = SnapshotReader::open(&img).unwrap();
        // Folded keys: "data" / "a.esp".
        assert!(matches!(r.resolve(&["data", "a.esp"]), SnapResolution::File { .. }));
        assert_eq!(r.getattr(&["data", "a.esp"]).unwrap().size, 10);
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p vfs-server server`
Expected: PASS (2 tests). If any fails (especially a `vfs_shared::bridge` path — confirm the `bridge` feature is enabled in `Cargo.toml`), STOP and report.

- [ ] **Step 3: Uncomment the re-export in `lib.rs`**

Uncomment: `pub use server::Server;`
Run: `cargo build -p vfs-server` and `cargo test -p vfs-server`
Expected: compiles; all unit tests pass.

- [ ] **Step 4: Commit**
```bash
git add crates/vfs-server/src/server.rs crates/vfs-server/src/lib.rs
git commit -m "feat(vfs-server): Server serving ring requests and publishing snapshot"
```

---

### Task 5: End-to-end threaded test

**Files:**
- Create: `crates/vfs-server/tests/e2e.rs`

**Interfaces:**
- Consumes: `vfs_server::{Server, proto::*}`, `vfs_ipc::{OwnedSeg, RingClient, RingServer, SpinNotifier, ring::init, layout::OP_*}`, `vfs_core::*`.
- Produces: an integration test where a client queries a server over a real ring, proving the full four-crate path.

- [ ] **Step 1: Write the test**

Create `crates/vfs-server/tests/e2e.rs`:
```rust
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use vfs_core::{EntryKind, InputEntry, Layer, LayerId};
use vfs_ipc::layout::{OP_GETATTR, OP_HEARTBEAT, OP_READDIR};
use vfs_ipc::ring::init;
use vfs_ipc::{OwnedSeg, RingClient, RingServer, SpinNotifier};
use vfs_server::proto::{decode_getattr_resp, decode_readdir_resp, encode_path_req};
use vfs_server::Server;

fn server() -> Server {
    let e = |vpath: &str, source: &str, size: u64, mtime: i64| InputEntry {
        vpath: vpath.into(),
        kind: EntryKind::File,
        source: source.into(),
        size,
        mtime,
    };
    Server::from_layers(vec![Layer {
        id: LayerId(0),
        entries: vec![e("data/a.esp", "s/a", 10, 1), e("data/b.esp", "s/b", 20, 2)],
    }])
    .unwrap()
}

#[test]
fn client_queries_server_over_ring() {
    let server = server();
    let owned = OwnedSeg::new(64 * 1024);
    init(owned.seg(), 4, 4096).unwrap();
    let seg = owned.seg();
    let stop = AtomicBool::new(false);

    thread::scope(|scope| {
        // Server thread: drain requests until stopped and idle.
        scope.spawn(|| {
            let ring = RingServer::new(seg, SpinNotifier).unwrap();
            loop {
                match server.serve_one(&ring) {
                    Ok(true) => {}
                    Ok(false) => {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // Client (this thread): issue requests and check responses.
        let client = RingClient::new(seg, SpinNotifier).unwrap();

        let resp = client.submit(OP_GETATTR, 0, &encode_path_req("data/a.esp")).unwrap();
        assert_eq!(resp.status, 0);
        let a = decode_getattr_resp(&resp.payload).unwrap();
        assert!(a.found && !a.is_dir && a.size == 10);

        let resp = client.submit(OP_GETATTR, 0, &encode_path_req("nope")).unwrap();
        assert!(!decode_getattr_resp(&resp.payload).unwrap().found);

        let resp = client.submit(OP_READDIR, 0, &encode_path_req("data")).unwrap();
        assert_eq!(resp.status, 0);
        let names: Vec<String> =
            decode_readdir_resp(&resp.payload).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["a.esp", "b.esp"]);

        // readdir of a file → NOT_A_DIRECTORY
        let resp = client.submit(OP_READDIR, 0, &encode_path_req("data/a.esp")).unwrap();
        assert_eq!(resp.status, -2);

        let resp = client.submit(OP_HEARTBEAT, 0, &[]).unwrap();
        assert_eq!(resp.status, 0);

        stop.store(true, Ordering::Relaxed);
    });
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p vfs-server --test e2e`
Expected: PASS. Concurrent + busy-spin; finishes in well under a second. If it hangs beyond ~60s treat as failure and report. If an assertion fails, STOP and report.

- [ ] **Step 3: Run the whole workspace suite**

Run: `cargo test --workspace`
Expected: all crates PASS.

- [ ] **Step 4: Commit**
```bash
git add crates/vfs-server/tests/e2e.rs
git commit -m "test(vfs-server): end-to-end client/server over the ring"
```

---

## Self-review

**Spec coverage:**
- §3 proto (status consts, AttrResp/DirEntryWire, encode/decode, decode-robust) → Task 2. ✓
- §4 dispatcher → Task 3. ✓
- §5 Server (new/from_layers/tree/handle/serve_one/snapshot) → Task 4. ✓
- §6 tests: proto round-trips + truncation (Task 2), dispatch hits/misses/errors/bad-request (Task 3), Server handle + snapshot-matches-core (Task 4), end-to-end threaded (Task 5). ✓
- §2/§7 deps (vfs-core, vfs-shared[bridge], vfs-ipc), `#![forbid(unsafe_code)]`, workspace member → Task 1. ✓

**Deferred by spec (correctly absent):** MATERIALIZE/handles, mutation opcodes, real Nt Notifier, worker pool, process registry, paging, director/shim.

**Placeholder scan:** none. Every step has complete code.

**Type consistency:** `AttrResp`/`DirEntryWire` + `ST_*` + `encode_*`/`decode_*` from `proto` (Task 2) used identically in `handler` (Task 3), `server` (Task 4), and `e2e` (Task 5). `dispatch` (Task 3) signature matches its call in `Server::handle` (Task 4). `Server::serve_one` uses `vfs_ipc::RingServer`/`Notifier`/`IpcError` and the `req.opcode`/`req.payload` fields from `vfs_ipc::Request`. `snapshot()` uses `vfs_shared::bridge::flatten(&VfsTree)` (bridge feature, Task 1) and the test reads it with `vfs_shared::{SnapshotReader, SnapResolution}`. Opcode consts `OP_GETATTR`/`OP_READDIR`/`OP_HEARTBEAT` from `vfs_ipc::layout`. `vfs_core::{build, Layer, LayerId, InputEntry, EntryKind, NodeKind, VfsError, BuildError, VfsTree}` all match the slice-1 public API.

*End of plan.*

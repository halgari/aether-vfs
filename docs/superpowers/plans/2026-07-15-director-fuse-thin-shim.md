# Director-Centric FUSE Thin Shim Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move all managed-root data access into the parent director over a FUSE-like control ring so the injected shim never opens zips or holds a file tree—only opaque `fh`s and RPC.

**Architecture:** Parent `vfs-launch` builds `vfs-core` layers, runs a `RingServer` thread with a stateful open-file table (zip-window / disk READ), and injects a thin shim that maps `NtCreateFile`/`NtReadFile`/`NtClose`/dir/attr under the managed root to `OPEN`/`READ`/`CLOSE`/`GETATTR`/`READDIR`. Pure RPC for every READ (client fragments large I/Os). PE hollow stays parent-local this phase.

**Tech Stack:** Rust workspace, `vfs-ipc` control ring, `vfs-win` sections, `vfs-core`/`vfs-zip`, `windows-sys` for events, existing `vfs-shim` hooks.

**Spec:** `docs/superpowers/specs/2026-07-15-director-fuse-thin-shim-design.md`

## Global Constraints

- Stable Rust; workspace `panic = "abort"`.
- Shim under managed root: **no** zip open/map, **no** snapshot Serve as content authority.
- IPC recursion-free (G11): ring + events only—never hooked `NtCreateFile`/`NtReadFile` on the transport path.
- Pure FUSE RPC READ; no shared bulk maps this phase.
- Parent director topology (one session = one launch process).
- FUSE-style handles: `OPEN → fh`, `READ(fh,off,len)`, `CLOSE(fh)`.
- PE hollow / game-local Stage B–D remain parent/inject-side until a later plan.
- Zero archive extract under managed root (unchanged).
- Prefer TDD: failing test → implement → pass → commit per task.
- Opcode numbers already in `vfs_ipc::layout` must not be renumbered (`OP_OPEN=3`, `OP_READ=5`, `OP_CLOSE=11`, etc.).

## File map (create / modify)

| Path | Responsibility |
|------|----------------|
| `crates/vfs-protocol/` | Pure LE codecs + status constants (no vfs-core, no OS) |
| `crates/vfs-server/src/proto.rs` | Re-export or thin wrapper around `vfs-protocol` for GETATTR/READDIR compat |
| `crates/vfs-server/src/open_table.rs` | Stateful fh table + zip/disk READ |
| `crates/vfs-server/src/handler.rs` | Dispatch including OPEN/READ/CLOSE |
| `crates/vfs-server/src/server.rs` | Stateful `Server` (tree + open table + mutex) |
| `crates/vfs-win/src/` | Named section + event notifier helpers if missing |
| `crates/vfs-ipc/src/notifier.rs` | Optional Windows notifier or leave in vfs-win |
| `crates/vfs-shim/src/fuse_client.rs` | RingClient wrapper + fragment READ |
| `crates/vfs-shim/src/synth_fh.rs` | Synth HANDLE → {fh, size, is_dir, pos} |
| `crates/vfs-shim/src/hook.rs` | Root path → fuse client (no zipserve Serve) |
| `crates/vfs-shim/src/engine.rs` | Optional: stop requiring snapshot for data path |
| `crates/vfs-launch/src/main.rs` | Start server thread; thin config; probe via RPC |
| `Cargo.toml` | Workspace member `vfs-protocol` |

---

### Task 1: `vfs-protocol` crate — OPEN/READ/CLOSE codecs

**Files:**
- Create: `crates/vfs-protocol/Cargo.toml`
- Create: `crates/vfs-protocol/src/lib.rs`
- Modify: `Cargo.toml` (workspace members)

**Interfaces:**
- Produces:
  - Status: `ST_OK=0`, `ST_NOT_FOUND=-1`, `ST_NOT_A_DIRECTORY=-2`, `ST_BAD_REQUEST=-3`, `ST_IO_ERROR=-4`, `ST_IS_DIR=-5`, `ST_BAD_FH=-6`, `ST_NO_SPACE=-7`
  - Re-export or mirror opcode constants from `vfs_ipc::layout` as `pub use` docs only—**do not depend on vfs-ipc** if that pulls unwanted deps; define matching `OP_*` u32 constants equal to layout values.
  - `encode_path_req` / `decode_path_req` (existing GETATTR/READDIR)
  - `encode_getattr_resp` / `decode_getattr_resp`, `encode_readdir_resp` / `decode_readdir_resp`
  - `OpenFlags` bitflags: `READ=1`, `WRITE=2`
  - `encode_open_req(flags: u32, path: &str) -> Vec<u8>`
  - `decode_open_req(p: &[u8]) -> Option<(u32, String)>`
  - `OpenResp { fh: u64, size: u64, is_dir: bool }`
  - `encode_open_resp` / `decode_open_resp`
  - `ReadReq { fh: u64, offset: u64, len: u32 }`
  - `encode_read_req` / `decode_read_req`
  - `encode_read_resp(bytes: &[u8]) -> Vec<u8>` // prepends `bytes_read:u32` + pad
  - `decode_read_resp(p: &[u8]) -> Option<Vec<u8>>`
  - `encode_close_req(fh: u64)` / `decode_close_req(p) -> Option<u64>`
- Consumes: nothing

- [ ] **Step 1: Scaffold crate and failing tests**

`crates/vfs-protocol/Cargo.toml`:
```toml
[package]
name = "vfs-protocol"
version = "0.1.0"
edition = "2021"

[dependencies]
```

Workspace `Cargo.toml`: add `"crates/vfs-protocol"` to `members`.

In `lib.rs`, write tests first (codecs can be stubbed to fail):

```rust
#![forbid(unsafe_code)]

// constants + encode/decode (implement in step 3)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_req_roundtrip() {
        let p = encode_open_req(OPEN_READ, "Data/Skyrim.esm");
        let (f, path) = decode_open_req(&p).unwrap();
        assert_eq!(f, OPEN_READ);
        assert_eq!(path, "Data/Skyrim.esm");
    }

    #[test]
    fn open_resp_roundtrip() {
        let r = OpenResp { fh: 42, size: 1000, is_dir: false };
        assert_eq!(decode_open_resp(&encode_open_resp(&r)), Some(r));
    }

    #[test]
    fn read_req_resp_roundtrip() {
        let req = ReadReq { fh: 7, offset: 10, len: 4 };
        assert_eq!(decode_read_req(&encode_read_req(&req)), Some(req));
        let data = b"abcd";
        assert_eq!(decode_read_resp(&encode_read_resp(data)).as_deref(), Some(&data[..]));
    }

    #[test]
    fn close_req_roundtrip() {
        assert_eq!(decode_close_req(&encode_close_req(99)), Some(99));
    }

    #[test]
    fn opcode_constants_match_ipc_catalog() {
        assert_eq!(OP_OPEN, 3);
        assert_eq!(OP_READ, 5);
        assert_eq!(OP_CLOSE, 11);
        assert_eq!(OP_GETATTR, 1);
        assert_eq!(OP_READDIR, 2);
        assert_eq!(OP_HEARTBEAT, 13);
    }

    #[test]
    fn short_buffers_decode_none() {
        assert!(decode_open_req(&[1,2]).is_none());
        assert!(decode_read_req(&[0u8; 10]).is_none());
        assert!(decode_read_resp(&[1,0,0]).is_none()); // claims len but short
    }
}
```

- [ ] **Step 2: Run tests — expect fail / compile error**

```powershell
cd C:\oss\vfs
cargo test -p vfs-protocol --lib
```

Expected: compile errors (missing items) or FAIL.

- [ ] **Step 3: Implement codecs**

Wire layouts (all LE):

```
OPEN req:  flags:u32 | path_utf8...
OPEN resp: fh:u64 | size:u64 | is_dir:u8 | pad[7]
READ req:  fh:u64 | offset:u64 | len:u32 | pad:u32
READ resp: bytes_read:u32 | pad:u32 | data[bytes_read]
CLOSE req: fh:u64
```

Move/copy GETATTR/READDIR helpers from `vfs-server::proto` into this crate (same layouts as today) so one place owns wire format.

- [ ] **Step 4: Tests pass**

```powershell
cargo test -p vfs-protocol --lib
```

Expected: `ok`

- [ ] **Step 5: Commit**

```powershell
git add Cargo.toml crates/vfs-protocol
git commit -m "feat(vfs-protocol): OPEN/READ/CLOSE wire codecs for director FUSE"
```

---

### Task 2: Point `vfs-server` GETATTR/READDIR at `vfs-protocol`

**Files:**
- Modify: `crates/vfs-server/Cargo.toml` (depend on `vfs-protocol`)
- Modify: `crates/vfs-server/src/proto.rs` (re-export from vfs-protocol; keep local type aliases if needed)
- Modify: `crates/vfs-server/src/handler.rs` (import status/opcodes from vfs-protocol)
- Modify: any tests importing `crate::proto::*`

**Interfaces:**
- Consumes: `vfs_protocol::{encode_*, decode_*, ST_*, OP_* (or layout), AttrResp, DirEntryWire}`
- Produces: existing `dispatch` behavior unchanged for GETATTR/READDIR/HEARTBEAT

- [ ] **Step 1: Switch imports; run existing server tests**

```powershell
cargo test -p vfs-server
```

Expected: PASS (behavior-preserving refactor).

- [ ] **Step 2: Commit**

```powershell
git add crates/vfs-server
git commit -m "refactor(vfs-server): use vfs-protocol for wire codecs"
```

---

### Task 3: Director open-file table + OPEN/READ/CLOSE dispatch

**Files:**
- Create: `crates/vfs-server/src/open_table.rs`
- Modify: `crates/vfs-server/src/handler.rs` or add `dispatch_stateful`
- Modify: `crates/vfs-server/src/server.rs`
- Modify: `crates/vfs-server/src/lib.rs`
- Create: fixture zip helper in tests (tiny Stored zip) OR use disk sources only for unit tests + one zip test via `vfs-zip`

**Interfaces:**
- Produces:
  ```rust
  pub struct OpenTable { /* Mutex-protected map */ }
  impl OpenTable {
      pub fn open(&self, tree: &VfsTree, path: &str, flags: u32) -> Result<OpenResp, i32 /*ST_*/>;
      pub fn read(&self, fh: u64, offset: u64, len: u32, max_data: usize) -> Result<Vec<u8>, i32>;
      pub fn close(&self, fh: u64) -> Result<(), i32>;
  }
  pub fn dispatch_with_table(tree: &VfsTree, table: &OpenTable, opcode: u32, payload: &[u8], payload_cap: u32) -> (i32, Vec<u8>);
  ```
- Server becomes:
  ```rust
  pub struct Server {
      tree: VfsTree,
      table: OpenTable,
      payload_cap: u32,
  }
  // handle/serve_one use dispatch_with_table
  ```

**OPEN resolve rules:**
1. Decode path as vpath (e.g. `data/a.esp`).
2. `tree.getattr` / resolve:
   - not found → `ST_NOT_FOUND`
   - dir → allocate fh, kind Dir, size 0
   - file → decode `source` with `vfs_core::decode`:
     - `ZipWindow` → store container path + offset + length=size
     - `Disk` → store path string
3. If `flags & OPEN_WRITE` and no write support → `ST_BAD_REQUEST` this phase.
4. Allocate `fh` from `AtomicU64` starting at 1.

**READ rules:**
1. Missing fh → `ST_BAD_FH`
2. Dir → `ST_IS_DIR`
3. `max_data = payload_cap.saturating_sub(8)` (header)
4. `want = min(len as u64, max_data as u64, size.saturating_sub(offset))`
5. Zip: `File::open(container)`, `seek(data_offset+offset)`, `read_exact` or read to vec
6. Disk: same on path
7. I/O error → `ST_IO_ERROR`

- [ ] **Step 1: Failing tests in `open_table.rs` or `tests/open_read.rs`**

```rust
#[test]
fn open_read_close_disk_source() {
    // build tree with file source = temp file path, size = content len
    // OpenTable::open → fh
    // read all → content
    // close → ok
    // read again → ST_BAD_FH
}

#[test]
fn open_read_zip_window() {
    // create tiny Stored zip with one entry "hello.txt" = b"hello-world"
    // vfs_zip::read_layer → build tree
    // open "hello.txt", read offset 0 len 5 → b"hello"
    // read offset 6 len 5 → b"world"
}

#[test]
fn read_fragments_honor_max_data() {
    // file 1000 bytes; max_data=100; read len=1000 returns 100 bytes
}
```

- [ ] **Step 2: Run — FAIL**

```powershell
cargo test -p vfs-server open_read
```

- [ ] **Step 3: Implement `OpenTable` + wire into `Server::handle`**

Keep GETATTR/READDIR pure-tree (no fh). OPEN/READ/CLOSE use table.

For zip tests, use a minimal Stored zip builder in the test module (local file header + central directory) or write raw bytes—do not add a new dependency if avoidable. Alternatively write content to a temp file and use Disk source for most tests; one integration test uses real `vfs-zip` + hand-made zip.

- [ ] **Step 4: Tests pass**

```powershell
cargo test -p vfs-server
```

- [ ] **Step 5: Commit**

```powershell
git add crates/vfs-server
git commit -m "feat(vfs-server): stateful OPEN/READ/CLOSE over zip and disk sources"
```

---

### Task 4: Threaded IPC e2e for OPEN/READ/CLOSE

**Files:**
- Modify: `crates/vfs-server/tests/e2e.rs` (or create `fuse_e2e.rs`)

**Interfaces:**
- Consumes: `Server`, `OwnedSeg`, `RingClient`/`RingServer`, `SpinNotifier`, protocol codecs

- [ ] **Step 1: Write test**

Pattern from existing e2e:

```rust
#[test]
fn client_open_read_close_over_ring() {
    let content = b"fuse-ipc-bytes";
    // temp file + tree disk source
    let server = Server::from_layers_with_cap(...);
    let mut owned = OwnedSeg::new(ring_bytes);
    Ring::init(...);
    let thr = thread::spawn(move || {
        let ring = RingServer::new(seg, SpinNotifier).unwrap();
        for _ in 0..10 {
            let _ = server.serve_one(&ring);
        }
    });
    let client = RingClient::new(seg, SpinNotifier).unwrap();
    let open_pl = encode_open_req(OPEN_READ, "data/f.bin");
    let resp = client.submit(OP_OPEN, 0, &open_pl).unwrap();
    assert_eq!(resp.status, ST_OK);
    let OpenResp { fh, size, .. } = decode_open_resp(&resp.payload).unwrap();
    assert_eq!(size, content.len() as u64);
    let rresp = client.submit(OP_READ, 0, &encode_read_req(&ReadReq { fh, offset: 0, len: size as u32 })).unwrap();
    assert_eq!(decode_read_resp(&rresp.payload).unwrap(), content);
    let _ = client.submit(OP_CLOSE, 0, &encode_close_req(fh)).unwrap();
    thr.join().unwrap();
}
```

- [ ] **Step 2: Run — FAIL until Server.serve_one uses stateful handle**

```powershell
cargo test -p vfs-server --test e2e
# or --test fuse_e2e
```

- [ ] **Step 3: Fix until PASS**

- [ ] **Step 4: Commit**

```powershell
git add crates/vfs-server/tests
git commit -m "test(vfs-server): ring e2e OPEN/READ/CLOSE"
```

---

### Task 5: Named section + event notifier (Windows)

**Files:**
- Modify or create: `crates/vfs-win/src/section.rs` / `notifier_event.rs`
- Modify: `crates/vfs-win/src/lib.rs`
- Test: `crates/vfs-win/tests/ring_over_section.rs` (extend)

**Interfaces:**
- Produces:
  ```rust
  pub struct NamedSection { /* handle, base, len */ }
  impl NamedSection {
      pub fn create(name: &str, size: usize) -> Result<Self, ...>;
      pub fn open(name: &str) -> Result<Self, ...>;
      pub fn as_shared_seg(&self) -> SharedSeg; // or raw ptr+len
  }
  pub struct EventNotifier { /* client/server events */ }
  impl Notifier for EventNotifier { ... }
  ```

**Notifier behavior:**
- `notify_server` / `wait_server`: auto-reset or manual-reset event pair
- `wait_client(slot)`: wait on per-slot or global response event (MVP: one global "response ready" event + spin check is OK if event wakes often)
- Must not call CreateFile on game-hooked path when used from shim—use `NtCreateEvent` / `CreateEventW` only

- [ ] **Step 1: Test create section in process A, open in same process, ring HEARTBEAT with EventNotifier**

```powershell
cargo test -p vfs-win
```

- [ ] **Step 2: Implement until PASS**

- [ ] **Step 3: Commit**

```powershell
git add crates/vfs-win
git commit -m "feat(vfs-win): named section and event notifier for FUSE ring"
```

---

### Task 6: Director runtime in `vfs-launch`

**Files:**
- Modify: `crates/vfs-launch/src/main.rs`
- Create: `crates/vfs-launch/src/director.rs` (optional split if main is large)
- Modify: `crates/vfs-launch/Cargo.toml` (deps: vfs-server, vfs-ipc, vfs-win, vfs-protocol)

**Interfaces:**
- Produces:
  ```rust
  struct DirectorHandle {
      // join handle, section name, root, payload_cap
  }
  fn start_director(tree: VfsTree, payload_cap: u32, slot_count: u32) -> Result<DirectorHandle, String>;
  // spawns thread: loop { server.serve_one(&ring) }
  fn write_thin_shim_config(path: &Path, section_name: &str, root: &str, payload_cap: u32) -> Result<(), String>;
  ```

**Config file format (simple TOML or KEY=VALUE lines):**

```
section=Local\vfs_ring_<pid>_<random>
root=C:\GameLayers\runtime
payload_cap=262144
```

Env fallback: `VFS_RING_SECTION`, `VFS_VIRTUAL_DIR`.

**Probe mode change:**
- Start director
- In-process `RingClient` OPEN/READ first 4–16 bytes of `Data/Skyrim.esm` and SkyUI files; print size/magic
- Do not require full game inject for probe

- [ ] **Step 1: Manual/dev test after implement**

```powershell
cargo run -p vfs-launch -- --probe
```

Expected: prints correct sizes/magic via RPC; exit 0.

- [ ] **Step 2: Keep existing launch path working** (still may use snapshot for shim until Task 7—config may include both ring + snapshot temporarily)

- [ ] **Step 3: Commit**

```powershell
git add crates/vfs-launch
git commit -m "feat(vfs-launch): parent director ring server and thin config"
```

---

### Task 7: Thin shim FUSE client

**Files:**
- Create: `crates/vfs-shim/src/fuse_client.rs`
- Create: `crates/vfs-shim/src/synth_fh.rs`
- Modify: `crates/vfs-shim/src/lib.rs`
- Modify: `crates/vfs-shim/Cargo.toml` (vfs-protocol, vfs-ipc, vfs-win)

**Interfaces:**
- Produces:
  ```rust
  pub struct FuseClient { /* RingClient + SharedSeg ownership + root components */ }
  impl FuseClient {
      pub fn connect(section: &str, root: &str, payload_cap: u32) -> Result<Self, ...>;
      pub fn heartbeat(&self) -> Result<(), ...>;
      pub fn getattr(&self, vpath: &str) -> Result<AttrResp, i32>;
      pub fn readdir(&self, vpath: &str) -> Result<Vec<DirEntryWire>, i32>;
      pub fn open(&self, vpath: &str, flags: u32) -> Result<OpenResp, i32>;
      pub fn read_all_fragmented(&self, fh: u64, offset: u64, buf: &mut [u8]) -> Result<usize, i32>;
      pub fn close(&self, fh: u64) -> Result<(), i32>;
  }
  pub struct SynthTable { ... }
  // allocate tagged HANDLE, lookup, free
  ```

**Fragmentation algorithm for `read_all_fragmented`:**
```
filled = 0
while filled < buf.len():
  chunk = min(remaining, payload_cap - 8)
  submit READ(fh, offset+filled, chunk)
  if status != OK: return Err
  data = decode
  if data.is_empty(): break  // EOF
  copy into buf[filled..]
  filled += data.len()
  if data.len() < chunk: break
return Ok(filled)
```

- [ ] **Step 1: Unit test fragmentation with a mock or in-process server thread** (prefer in-process: start Server+ring in test thread, FuseClient against same section)

```powershell
cargo test -p vfs-shim fuse
```

- [ ] **Step 2: Implement client + synth table**

- [ ] **Step 3: PASS + commit**

```powershell
git add crates/vfs-shim
git commit -m "feat(vfs-shim): FuseClient and fh synth table"
```

---

### Task 8: Wire hooks under managed root to FuseClient (drop Serve/zipserve for files)

**Files:**
- Modify: `crates/vfs-shim/src/hook.rs`
- Modify: `crates/vfs-shim/src/bootstrap.rs` / `engine.rs` as needed
- Modify: `crates/vfs-shim-dll` config loading

**Behavior:**
1. Bootstrap: if thin config has `section=`, connect `FuseClient`, HEARTBEAT, store in static/`OnceLock`.
2. `create_hook` / `open_hook`: if path under root:
   - map to vpath
   - `client.open(vpath, READ)`
   - on ST_OK: mint synth handle with fh/size/is_dir
   - on ST_NOT_FOUND: `STATUS_OBJECT_NAME_NOT_FOUND`
   - on transport fail: `STATUS_DEVICE_NOT_READY`
   - **do not** call `Decision::Serve` / `zipserve::open_synth`
3. `read_hook`: if synth fh: `read_all_fragmented` into buffer; update position
4. `query_information` size/position from synth cache
5. `directory` hooks: `client.readdir`
6. `close`: `client.close(fh)`
7. Outside root: trampoline as today

**Dual-layer:** secondary dispatch on early stubs must call the same open/attr path (Engine::decide Serve removed for files). Redirect/overlay can remain director-side only this phase (overlay OPEN resolve on server).

- [ ] **Step 1: Prefer integration test** `crates/vfs-shim/tests/fuse_hooks.rs` if feasible; else extend launch probe

- [ ] **Step 2: `cargo test -p vfs-shim` and `cargo build -p vfs-shim-dll -p vfs-launch`**

- [ ] **Step 3: Manually run**

```powershell
cargo run -p vfs-launch -- --probe
```

Confirm logs show ring OPEN/READ, and **no** `zipserve` map of game zips in the probe/game process (director process may open zips).

- [ ] **Step 4: Commit**

```powershell
git add crates/vfs-shim crates/vfs-shim-dll crates/vfs-launch
git commit -m "feat(vfs-shim): managed-root I/O via director FUSE RPC only"
```

---

### Task 9: Cleanup + acceptance

**Files:**
- Modify: docs if needed (`docs/vfs-summary.md` short “FUSE phase” note—optional)
- Remove dead call paths from hooks (zipserve may remain for SEC_IMAGE experiments but unused for Data/)
- Ensure `prepare_layer` still creates dirs only

**Acceptance checklist (run and capture):**

```powershell
cd C:\oss\vfs
cargo test -p vfs-protocol -p vfs-server -p vfs-win -p vfs-shim -- --test-threads=1
cargo run -p vfs-launch -- --probe
# Optional stretch:
# cargo run -p vfs-launch -- --wait
# dual-launch window + SKSE as before
```

Probe must show correct ESM/SkyUI sizes via RPC.  
Managed root payload file count = 0.  
Shim must not CreateFile layer zips (director may).

- [ ] **Step 1: Run full acceptance**

- [ ] **Step 2: Commit any fixes**

```powershell
git add -A
git commit -m "test: director FUSE thin-shim acceptance (probe + unit)"
```

- [ ] **Step 3: Push**

```powershell
git push origin master
```

---

## Spec coverage checklist

| Spec section | Task(s) |
|--------------|---------|
| Pure RPC READ + fragmentation | 3, 7 |
| Parent director topology | 6 |
| OPEN/READ/CLOSE fh model | 1, 3, 4 |
| Thin shim no zip/tree | 7, 8 |
| Protocol codecs + status codes | 1 |
| Open-file table + zip I/O | 3 |
| Named section + event notifier | 5 |
| Launch wiring + thin config | 6 |
| Hook mapping | 8 |
| Probe acceptance | 6, 9 |
| PE hollow unchanged | (no task—do not regress inject) |
| Non-goals (bulk map, PE RPC, writes) | excluded |

## Placeholder scan

No TBD steps; opcodes fixed; layouts explicit; commands given.

---

## Execution handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-15-director-fuse-thin-shim.md`.

**Two execution options:**

1. **Subagent-Driven (recommended)** — fresh subagent per task, review between tasks  
2. **Inline Execution** — implement tasks in this session with checkpoints  

Which approach?

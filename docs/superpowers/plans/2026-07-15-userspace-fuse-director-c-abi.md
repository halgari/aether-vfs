# Userspace FUSE Director + C ABI — Implementation Plan

> **For agentic workers:** implement task-by-task; checkboxes optional.

**Goal:** Ship `vfs-director` as the userspace FUSE kernel with a C ABI; zip as a backend only; wire launch content path through it.

**Architecture:** Director owns mounts + fh table; backends implement ops; zip crate indexes CD and serves Stored windows; C ABI mirrors Rust ops.

**Tech Stack:** Rust 2021, `cdylib`/`staticlib`, C header, existing `vfs-protocol` status codes.

## Global Constraints

- No zip types inside `vfs-director`  
- No `unsafe` outside `ffi` module in director (except where C requires)  
- Status codes match `vfs-protocol` where possible  

---

### Task 1: `vfs-director` kernel + disk backend + tests

**Files:** `crates/vfs-director/**`

- Backend trait, Stat/DirEntry, Director mount/getattr/readdir/open/read/close  
- Disk backend for a root directory  
- Unit tests: disk tree open/read  

### Task 2: C ABI

**Files:** `crates/vfs-director/include/vfs.h`, `src/ffi.rs`

- Export create/destroy/mount/getattr/open/read/close/readdir  
- C-callable backend ops registration  

### Task 3: Zip backend

**Files:** `crates/vfs-zip/src/backend.rs` (+ lib exports)

- `ZipBackend::open(path)` builds index  
- Implements director `Backend`  
- Test: Stored zip open/read via Director  

### Task 4: Server/launch bridge

**Files:** `vfs-server` adapter or launch mounts zips on director for ring path  

- Prefer: ring `Server` can be constructed from `Arc<Director>` for OPEN/READ/meta  
- Launch: mount each layer zip; keep PE hollow transitional if needed  

### Task 5: Docs + commit

- Update design success checkboxes; commit  

---

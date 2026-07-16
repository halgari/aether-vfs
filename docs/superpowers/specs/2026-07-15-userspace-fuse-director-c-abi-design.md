# Userspace FUSE Director + C ABI — Design Spec

**Status:** Approved (dialogue 2026-07-15); implementation in progress  
**Date:** 2026-07-15  
**Type:** Architecture  

## 1. Goal

Provide a **full userspace FUSE** implementation whose **entrypoint is the director session**. Hosts **configure mounts**, **serve IPC**, and **`vfs_launch` a process** so all NT I/O under the virtual root is remapped. Hosts rarely call open/read themselves.

Mental model:

> Session = configure + serve + launch. Kernel = FUSE in userland. Backends = fuse_operations (zip/disk/C). Child process = primary client via inject + ring.

## 2. Non-goals (this slice)

- Kernel FUSE / WinFsp  
- Write path  
- Zip knowledge inside director or a “core” resolver  
- Rewriting PE hollow to C (may still read zip via the zip backend)  

## 3. Crates

| Crate | Role |
|-------|------|
| **`vfs-director`** | Userspace FUSE kernel: mount table, overlay resolve, open-file table, in-process API, **C ABI** |
| **`vfs-zip`** | Zip **backend** only (CD index + Stored window reads); implements director ops; no merge logic |
| **`vfs-launch`** | Thin host: create director, mount zip backends, serve ring |
| **`vfs-server`** | Ring dispatch over director (or thin adapter) |

Legacy `vfs-core` Layer/Source/ZipWindow remains for transitional code paths only; new content authority goes through director backends.

## 4. Backend ops (C + Rust)

```c
typedef struct vfs_stat {
  uint8_t kind;   /* 1=file, 2=dir, 3=tombstone */
  uint64_t size;
  int64_t mtime;
} vfs_stat;

typedef struct vfs_backend_ops {
  int (*getattr)(void *ud, const char *path, vfs_stat *out);
  int (*readdir)(void *ud, const char *path, void *fill_ctx,
                 int (*fill)(void *fill_ctx, const char *name, const vfs_stat *st));
  int (*open)(void *ud, const char *path, uint32_t flags, uint64_t *bh_out, uint64_t *size_out);
  int (*read)(void *ud, uint64_t bh, uint64_t offset, uint8_t *buf, uint32_t len, uint32_t *nread);
  int (*release)(void *ud, uint64_t bh);
} vfs_backend_ops;
```

Status codes align with `vfs-protocol` (`0` OK, negative errors).

Rust equivalent: `trait Backend: Send + Sync` with the same methods.

## 5. Host / driver API (C) — launch-centric

Primary workflow:

1. `vfs_director_create`
2. `vfs_director_set_root` / `set_overlay` / `set_state_dir`
3. `vfs_director_mount` (backends: zip via `ZipBackend`, disk, or C ops)
4. `vfs_director_serve` — start ring so the child can remap I/O
5. `vfs_launch` — CreateProcess + inject; child I/O under root goes through this session
6. `vfs_director_destroy`

Optional host inspection: `vfs_getattr` / `vfs_open` / `vfs_read` / `vfs_close` (not the hot path).

Mount order: **later mounts override earlier** for the same path (overlay).

Rust: [`Session`](crates/vfs-director/src/session.rs) (`serve` + `launch`).

## 6. Kernel behavior

1. Normalize path (`/` separators, no `..` escape).  
2. **getattr / open:** walk mounts high→low; first hit wins.  
3. **readdir:** merge names from all mounts that have the directory; later overrides same name.  
4. **open:** allocate global `fh` → `(backend_id, bh, size, is_dir)`.  
5. **read/close:** dispatch to that backend only.  

No zip encoding, no `SourceId` in the director.

## 7. Zip backend

- Parse central directory once; keep path → `{ data_off, size, mtime, is_dir }`.  
- `open` opens the container file; `read` seeks to `base+offset`.  
- Registered with `vfs_director_mount("", &zip_ops, zip_state)`.  

## 8. Success criteria

- [x] C header + cdylib/staticlib export (`crates/vfs-director/include/vfs.h`, `ffi`)  
- [x] In-process open/read of a disk backend and a zip backend  
- [x] Unit tests without the game inject path  
- [x] Launch mounts GameLayers zips via `ZipBackend` + ring `Server::from_director`  

## 9. Out of scope follow-ups

- Tombstones as first-class backend results  
- Multi-session daemon  
- Full removal of `vfs-core` from launch/PE path  

//! Thin director FUSE client: ring + bulk arena + optional event wake.

// Waking the director is a Win32 SetEvent on a handle we opened; the rest of
// the crate stays unsafe-free.
#![allow(unsafe_code)]

use std::sync::{Mutex, OnceLock};

use vfs_ipc::{Geom, RingClient};
use vfs_redirect::{RootId, RootMap};
use vfs_protocol::{
    decode_getattr_resp, decode_open_resp, decode_readdir_resp, decode_read_bulk_resp,
    decode_read_resp_into, decode_write_resp, encode_close_req, encode_mkdir_req, encode_open_req,
    encode_path_req, encode_read_req, encode_rename_req, encode_setattr_req, encode_write_req,
    is_read_resp_bulk, AttrResp, DirEntryWire, OpenResp, ReadReq, SetattrReq, WriteReq,
    FLAG_READ_BULK, OP_CLOSE, OP_DELETE, OP_GETATTR, OP_HEARTBEAT, OP_MKDIR, OP_OPEN, OP_READ,
    OP_READDIR, OP_RENAME, OP_SETATTR, OP_WRITE, OPEN_READ, OPEN_WRITE, ST_OK,
};
use vfs_win::SharedMapping;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Threading::{OpenEventW, SetEvent};

/// `EVENT_MODIFY_STATE` — all we need is `SetEvent`.
const EVENT_MODIFY_STATE: u32 = 0x0002;

static FUSE: OnceLock<FuseClient> = OnceLock::new();

pub fn global() -> Option<&'static FuseClient> {
    FUSE.get()
}

/// Why running on the large-stack worker failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LargeStackError {
    /// The worker thread could not be created.
    Spawn,
    /// The closure panicked on the worker.
    Panicked,
}

/// Run FUSE ring I/O on a **large stack** worker.
///
/// SkyrimSE's primary thread ships with a 1 MiB PE stack. Pipelined ring
/// submit + bulk arena copies there regularly hit `0xC0000409` / stack overflow.
/// Callers on game threads must use this for open/read/getattr paths.
pub fn on_large_stack<T, F>(f: F) -> Result<T, LargeStackError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    std::thread::Builder::new()
        .name("vfs-fuse-io".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(f)
        .map_err(|_| LargeStackError::Spawn)?
        .join()
        .map_err(|_| LargeStackError::Panicked)
}

/// Why [`try_init_from_env`] did not leave a live [`FuseClient`] installed.
///
/// Both variants are fatal to the caller (`bootstrap.rs` aborts the launch on
/// either). Standalone shim launches — no ring named at all, the local
/// `Engine` snapshot governing composition alone — used to be treated as a
/// legitimate deployment, and plenty of this crate's own tests used to run
/// exactly that way. That mode is retired: it is precisely the one in which a
/// game runs completely un-virtualised while looking like a normal launch —
/// the bypass this type exists to make impossible to ignore. The two cases
/// are still kept distinct because their messages differ (a name for one, a
/// connection failure reason for the other), not because either is safe to
/// swallow.
#[derive(Debug)]
pub enum FuseInitError {
    /// No ring was named (`VFS_RING_SECTION` unset). The process was not
    /// launched to talk to a director — no longer a supported deployment.
    NotConfigured,
    /// A ring was named but the client could not attach, or the post-connect
    /// heartbeat failed. The caller intended virtualisation and it silently
    /// did not happen — this must reach whoever launched the process.
    ConnectFailed(String),
}

pub fn try_init_from_env() -> Result<(), FuseInitError> {
    if FUSE.get().is_some() {
        return Ok(());
    }
    // Test-only escape hatch (see the constant's doc comment in `vfs-env`): lets
    // the launch-abort path be exercised without standing up a director that is
    // actually broken.
    if vfs_env::opt_in(vfs_env::TEST_FUSE_INIT_FAIL) {
        return Err(FuseInitError::ConnectFailed(
            format!("forced failure via {}", vfs_env::TEST_FUSE_INIT_FAIL),
        ));
    }
    let section = vfs_env::text(vfs_env::RING_SECTION).ok_or(FuseInitError::NotConfigured)?;
    let ring_bytes: usize = vfs_env::text(vfs_env::RING_BYTES)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2 * 1024 * 1024);
    let payload_cap: u32 = vfs_env::text(vfs_env::RING_PAYLOAD_CAP)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_048_576);
    let arena_len: usize = vfs_env::text(vfs_env::ARENA_LEN)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    // Required, not defaulted: this names *which tree is being virtualised*, and
    // there is no sensible guess. The default it used to carry pointed at a
    // layout that no longer exists, so an unset root connected the client to a
    // path nothing matched — surfacing much later as content simply missing.
    //
    // Reachable only once `section` above is `Some` — i.e. a director launch
    // was intended — so this is a `ConnectFailed`, not `NotConfigured`.
    let root = vfs_env::text(vfs_env::VIRTUAL_DIR).ok_or_else(|| {
        FuseInitError::ConnectFailed(
            "VFS_VIRTUAL_DIR unset: the managed root has no default".to_string(),
        )
    })?;
    let roots = roots_from_env(&root);
    let client = FuseClient::connect(&section, &roots, payload_cap, ring_bytes, arena_len)
        .map_err(FuseInitError::ConnectFailed)?;
    client.heartbeat().map_err(FuseInitError::ConnectFailed)?;
    let _ = FUSE.set(client);
    Ok(())
}

/// Concurrent bulk READs in flight (each uses its own arena bank via slot id).
/// Keep modest: deep pipelines + large banks correlated with early 0xC0000409
/// under the sealed director path (working director-only used depth 4 / 1 MiB).
const PIPELINE_DEPTH: usize = 4;
/// Prefer shared-section bulk over inline ring payload above this size.
const BULK_THRESHOLD: u32 = 64 * 1024;
/// Deep pipeline for multi‑MiB sequential streams (CreateSection fill).
const PIPELINE_DEPTH_STREAM: usize = 8;

/// Wakes the director on submit, then spins for the response.
///
/// The client used to be a plain `SpinNotifier`, whose `notify_server` is a
/// no-op — so `server_ev` was never signalled and a director that had gone to
/// sleep only noticed the request when its timed wait expired at the 15.6 ms
/// timer tick. Measured 2026-08-12: 16 of 231 `NtQueryFullAttributesFile` calls
/// stalled that way and owned ~93% of that hook's total time, with a 15.2 ms
/// worst case.
///
/// Spinning for the *response* stays right: it arrives in 20–209 µs, far below
/// the cost of sleeping for it.
struct WakeServerSpinClient {
    server_ev: HANDLE,
}

impl vfs_ipc::Notifier for WakeServerSpinClient {
    fn notify_server(&self) {
        if !self.server_ev.is_null() {
            // SAFETY: handle owned by FuseClient for its lifetime; SetEvent is
            // safe on an auto-reset event from any thread.
            unsafe {
                let _ = SetEvent(self.server_ev);
            }
        }
    }
    fn wait_client(&self, _slot: u32) {
        core::hint::spin_loop();
    }
    fn notify_slot_free(&self) {
        // A full ring can leave the director blocked; wake it on release too.
        if !self.server_ev.is_null() {
            unsafe {
                let _ = SetEvent(self.server_ev);
            }
        }
    }
}

pub struct FuseClient {
    mapping: SharedMapping,
    geom: Geom,
    payload_cap: u32,
    /// Every managed root this session virtualizes, plus the staged-launch
    /// directory as an alias for root 0 — and the *only* under-root predicate
    /// the shim has.
    ///
    /// **This used to be a pair of lowercased strings** (`root_lower` plus an
    /// optional `stage_root_lower`) tested with `strip_prefix`. That was the
    /// second of two under-root predicates in this tree, and it disagreed with
    /// the first: `RootMap` canonicalises (device paths, volume GUIDs,
    /// `GLOBALROOT`, UNC admin shares, junction aliases, 8.3 short names) and
    /// the string test did none of it, so five alternate spellings of an
    /// in-root path were classified by `RootMap::decide` but never *routed* by
    /// this client — and a name-based attribute query on one of them reached
    /// real disk. Stage 2b task 5 replaced the strings with the real thing, so
    /// there is now one predicate rather than two that can drift.
    roots: RootMap,
    arena_len: usize,
    /// Director wake event (`VFS_SERVER_EV`), null when it could not be opened —
    /// the ring still works, just with the old timer-tick latency.
    server_ev: HANDLE,
    /// Serializes ring claim/submit — not safe for concurrent clients on one ring.
    ring_lock: Mutex<()>,
}

// SAFETY: `server_ev` is only ever passed to SetEvent, which is thread-safe.
unsafe impl Send for FuseClient {}
unsafe impl Sync for FuseClient {}

impl FuseClient {
    /// `roots` is `(id, path)` for every managed root, root 0 first. The
    /// staged-launch directory (if any) is appended here as a second spelling
    /// of root 0 — see [`FuseClient::vpath_under_root`].
    pub fn connect(
        section: &str,
        roots: &[(RootId, String)],
        payload_cap: u32,
        ring_bytes: usize,
        arena_len: usize,
    ) -> Result<Self, String> {
        let mapping = SharedMapping::open(section, ring_bytes)
            .map_err(|e| format!("open section {section}: {e}"))?;
        let geom = vfs_ipc::ring::open(mapping.seg()).map_err(|e| format!("ring open: {e:?}"))?;
        // Opening the director's wake event is best-effort: without it the ring
        // still works, it just falls back to the director's timed wait.
        let server_ev = vfs_env::text(vfs_env::SERVER_EV)
            .map(|n| {
                let w: Vec<u16> = n.encode_utf16().chain(core::iter::once(0)).collect();
                // SAFETY: name is NUL-terminated; a failed open returns null.
                unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, w.as_ptr()) }
            })
            .unwrap_or(core::ptr::null_mut());

        // The staged launch directory is a second spelling of root 0, not a
        // root of its own: a staged game resolves `Data\` relative to its own
        // executable, so those opens must reach the same provider as the
        // managed-root spelling. Expressed as an alias entry rather than as a
        // separate prefix test, which is what it used to be.
        let mut decls: Vec<(RootId, String)> = roots.to_vec();
        if let Some(stage) = stage_root_from_env() {
            decls.push((RootId::DEFAULT, stage));
        }
        if decls.is_empty() {
            return Err("no managed root declared for the FUSE client".to_string());
        }
        // Resolved once, from the live OS, before any hook is installed
        // (`bootstrap` calls `try_init_from_env` ahead of `install`), so the
        // junction scan's own `CreateFileW` calls cannot re-enter this path.
        // Scoped to *every* declared root, not just root 0 — see
        // `resolve_volume_map_for`.
        let scan: Vec<&str> = decls.iter().map(|(_, p)| p.as_str()).collect();
        let volumes = vfs_redirect::resolve_volume_map_for(&scan);
        let refs: Vec<(RootId, &str)> =
            decls.iter().map(|(id, p)| (*id, p.as_str())).collect();
        let roots = RootMap::with_roots(&refs, volumes)
            .map_err(|e| format!("managed root is not a usable path: {e:?}"))?;

        Ok(FuseClient {
            mapping,
            geom,
            payload_cap,
            roots,
            arena_len,
            server_ev,
            ring_lock: Mutex::new(()),
        })
    }

    /// Test-only constructor for the predicate alone: no ring, no OS volume
    /// scan. `connect` needs a live shared section, which a unit test has no
    /// business standing up just to ask whether a path is under a root.
    #[cfg(test)]
    fn roots_only(decls: &[(RootId, &str)]) -> RootMap {
        RootMap::with_roots(decls, vfs_redirect::VolumeMap::empty()).unwrap()
    }

    fn client(&self) -> RingClient<'_, WakeServerSpinClient> {
        RingClient::with_geom(
            self.mapping.seg(),
            self.geom,
            WakeServerSpinClient {
                server_ev: self.server_ev,
            },
        )
    }

    pub fn heartbeat(&self) -> Result<(), String> {
        let _g = self.ring_lock.lock().map_err(|_| "ring lock poisoned".to_string())?;
        let c = self.client();
        let r = c
            .submit(OP_HEARTBEAT, 0, &[])
            .map_err(|e| format!("HEARTBEAT: {e:?}"))?;
        if r.status != ST_OK {
            return Err(format!("HEARTBEAT status {}", r.status));
        }
        Ok(())
    }

    pub fn getattr(&self, root: RootId, vpath: &str) -> Result<AttrResp, i32> {
        let _g = self.ring_lock.lock().map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        let c = self.client();
        let r = c
            .submit(OP_GETATTR, 0, &encode_path_req(root.0, vpath))
            .map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        if r.status != ST_OK {
            return Err(r.status);
        }
        decode_getattr_resp(&r.payload).ok_or(vfs_protocol::ST_BAD_REQUEST)
    }

    pub fn readdir(&self, root: RootId, vpath: &str) -> Result<Vec<DirEntryWire>, i32> {
        let _g = self.ring_lock.lock().map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        let c = self.client();
        let r = c
            .submit(OP_READDIR, 0, &encode_path_req(root.0, vpath))
            .map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        if r.status != ST_OK {
            return Err(r.status);
        }
        decode_readdir_resp(&r.payload).ok_or(vfs_protocol::ST_BAD_REQUEST)
    }

    pub fn open(&self, root: RootId, vpath: &str) -> Result<OpenResp, i32> {
        let _g = self.ring_lock.lock().map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        let c = self.client();
        let r = c
            .submit(OP_OPEN, 0, &encode_open_req(root.0, OPEN_READ, vpath))
            .map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        if r.status != ST_OK {
            return Err(r.status);
        }
        decode_open_resp(&r.payload).ok_or(vfs_protocol::ST_BAD_REQUEST)
    }

    /// Open a virtual path for WRITE. `create_flags` carries the
    /// `OPEN_CREATE`/`OPEN_EXCL`/`OPEN_TRUNC` bits derived from the caller's NT
    /// create-disposition (see `hook::open_create_flags`) — folded in here
    /// rather than hardcoding `OPEN_WRITE` alone, which is what used to make
    /// every brand-new file report `ST_NOT_FOUND` regardless of disposition.
    pub fn open_write(
        &self,
        root: RootId,
        vpath: &str,
        create_flags: u32,
    ) -> Result<OpenResp, i32> {
        let _g = self.ring_lock.lock().map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        let c = self.client();
        let r = c
            .submit(
                OP_OPEN,
                0,
                &encode_open_req(root.0, OPEN_WRITE | create_flags, vpath),
            )
            .map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        if r.status != ST_OK {
            return Err(r.status);
        }
        decode_open_resp(&r.payload).ok_or(vfs_protocol::ST_BAD_REQUEST)
    }

    /// Write `data` at `offset` to a virtual write handle. Chunks to fit the ring
    /// payload (inline; bulk-arena writes are a later step).
    pub fn write(&self, fh: u64, offset: u64, data: &[u8]) -> Result<usize, i32> {
        if data.is_empty() {
            return Ok(0);
        }
        let _g = self.ring_lock.lock().map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        let chunk = (self.payload_cap as usize).saturating_sub(24).max(1);
        let c = self.client();
        let mut written = 0usize;
        while written < data.len() {
            let end = (written + chunk).min(data.len());
            let piece = &data[written..end];
            let r = c
                .submit(
                    OP_WRITE,
                    0,
                    &encode_write_req(
                        &WriteReq {
                            fh,
                            offset: offset + written as u64,
                            len: piece.len() as u32,
                        },
                        piece,
                    ),
                )
                .map_err(|_| vfs_protocol::ST_IO_ERROR)?;
            if r.status != ST_OK {
                return Err(r.status);
            }
            let n = decode_write_resp(&r.payload).ok_or(vfs_protocol::ST_BAD_REQUEST)? as usize;
            written += n;
            if n < piece.len() {
                break;
            }
        }
        Ok(written)
    }

    /// Bytes per bulk READ = one arena bank (shared section, not ring payload).
    fn bulk_bank_bytes(&self) -> usize {
        if self.arena_len == 0 {
            return self.payload_cap.saturating_sub(8) as usize;
        }
        let slots = self.geom.slot_count.max(1) as usize;
        // Cap at 1 MiB per RTT — full bank (4–16 MiB) was used during the
        // 0xC0000409 regression window; large copies + deep pipelines on the
        // game thread (or long join windows) are not worth the risk yet.
        (self.arena_len / slots).clamp(256 * 1024, 1024 * 1024)
    }

    /// Read into `buf` (game NtReadFile buffer or CreateSection destination).
    ///
    /// Large fragments use the **shared bulk arena** (control ring only carries
    /// length+offset); small use inline ring payload. Data never rides as a
    /// multi‑MiB ring blob.
    pub fn read_fragmented(
        &self,
        fh: u64,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, i32> {
        if buf.is_empty() {
            return Ok(0);
        }
        let _g = self.ring_lock.lock().map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        let bulk_chunk = self.bulk_bank_bytes();
        let inline_chunk = self.payload_cap.saturating_sub(8) as usize;
        // Deep pipeline for multi‑MiB sequential streams (section fill / BSA).
        let pipeline = if buf.len() >= 4 * 1024 * 1024 {
            PIPELINE_DEPTH_STREAM.min(self.geom.slot_count as usize).max(1)
        } else {
            PIPELINE_DEPTH.min(self.geom.slot_count as usize).max(1)
        };
        let c = self.client();
        let mut filled = 0usize;

        while filled < buf.len() {
            let mut reqs: Vec<(u32, u32, Vec<u8>)> = Vec::new();
            let mut wants: Vec<usize> = Vec::new();
            let mut batch_off = filled;
            while reqs.len() < pipeline && batch_off < buf.len() {
                let rem = buf.len() - batch_off;
                let bulk = rem as u32 >= BULK_THRESHOLD && self.arena_len > 0;
                let chunk = if bulk {
                    rem.min(bulk_chunk)
                } else {
                    rem.min(inline_chunk)
                } as u32;
                if chunk == 0 {
                    break;
                }
                let flags = if bulk { FLAG_READ_BULK } else { 0 };
                reqs.push((
                    OP_READ,
                    flags,
                    encode_read_req(&ReadReq {
                        fh,
                        offset: offset + batch_off as u64,
                        len: chunk,
                    }),
                ));
                wants.push(chunk as usize);
                batch_off += chunk as usize;
            }
            if reqs.is_empty() {
                break;
            }

            // Hold slots until bulk arena banks are copied — free-before-copy
            // races with bank reuse and can corrupt BSA streams (game then dies
            // with 0xC0000409 / bad archive parse after ~full Animations.bsa).
            let (responses, held) = c
                .submit_many_held(&reqs)
                .map_err(|_| vfs_protocol::ST_IO_ERROR)?;

            let mut batch_filled = 0usize;
            let mut eof = false;
            let mut copy_err: Option<i32> = None;
            for (resp, want) in responses.iter().zip(wants.iter()) {
                if resp.status != ST_OK {
                    copy_err = Some(resp.status);
                    break;
                }
                let frag_start = filled + batch_filled;
                let dest = &mut buf[frag_start..frag_start + *want];
                let n = if is_read_resp_bulk(&resp.payload) {
                    let (bn, aoff) = match decode_read_bulk_resp(&resp.payload) {
                        Some(x) => x,
                        None => {
                            copy_err = Some(vfs_protocol::ST_BAD_REQUEST);
                            break;
                        }
                    };
                    let n = (bn as usize).min(dest.len());
                    if n > 0 {
                        // Shared arena → destination (one memcpy; not via ring).
                        if self
                            .mapping
                            .seg()
                            .copy_to(aoff as usize, &mut dest[..n])
                            .is_none()
                        {
                            copy_err = Some(vfs_protocol::ST_IO_ERROR);
                            break;
                        }
                    }
                    n
                } else {
                    match decode_read_resp_into(&resp.payload, dest) {
                        Some(n) => n,
                        None => {
                            copy_err = Some(vfs_protocol::ST_BAD_REQUEST);
                            break;
                        }
                    }
                };
                batch_filled += n;
                if n < *want {
                    eof = true;
                    break;
                }
            }
            c.release_slots(&held);
            if let Some(st) = copy_err {
                return Err(st);
            }
            filled += batch_filled;
            if eof || batch_filled == 0 {
                break;
            }
        }
        Ok(filled)
    }

    /// Delete (whiteout) a virtual path via the JVM overlay (`OP_DELETE`).
    pub fn delete(&self, root: RootId, vpath: &str) -> Result<(), i32> {
        let _g = self.ring_lock.lock().map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        let c = self.client();
        let r = c
            .submit(OP_DELETE, 0, &encode_path_req(root.0, vpath))
            .map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        if r.status != ST_OK {
            return Err(r.status);
        }
        Ok(())
    }

    /// Rename a virtual path to another virtual path (`OP_RENAME`).
    ///
    /// One root for both sides: the director resolves `from` and `to` against
    /// the same root, and a caller whose two paths land under *different*
    /// roots must not route the rename here at all — see `hook.rs`'s
    /// rename/delete arm, which declines rather than guessing.
    pub fn rename(&self, root: RootId, from: &str, to: &str) -> Result<(), i32> {
        let _g = self.ring_lock.lock().map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        let c = self.client();
        let r = c
            .submit(OP_RENAME, 0, &encode_rename_req(root.0, from, to))
            .map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        if r.status != ST_OK {
            return Err(r.status);
        }
        Ok(())
    }

    /// Create a virtual directory via the JVM overlay (`OP_MKDIR`).
    pub fn mkdir(&self, root: RootId, vpath: &str, mode: u32) -> Result<(), i32> {
        let _g = self.ring_lock.lock().map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        let c = self.client();
        let r = c
            .submit(OP_MKDIR, 0, &encode_mkdir_req(root.0, mode, vpath))
            .map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        if r.status != ST_OK {
            return Err(r.status);
        }
        Ok(())
    }

    /// Truncate/extend a virtual write handle to `size` bytes (`OP_SETATTR`).
    pub fn truncate(&self, fh: u64, size: u64) -> Result<(), i32> {
        let _g = self.ring_lock.lock().map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        let c = self.client();
        let r = c
            .submit(OP_SETATTR, 0, &encode_setattr_req(&SetattrReq { fh, size }))
            .map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        if r.status != ST_OK {
            return Err(r.status);
        }
        Ok(())
    }

    pub fn close(&self, fh: u64) -> Result<(), i32> {
        let _g = self.ring_lock.lock().map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        let c = self.client();
        let r = c
            .submit(OP_CLOSE, 0, &encode_close_req(fh))
            .map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        if r.status != ST_OK {
            return Err(r.status);
        }
        Ok(())
    }

    /// Map an absolute path into the virtual namespace: **which root** it
    /// belongs to, and its path relative to that root.
    ///
    /// Several roots resolve, not one, and the staged launch directory is one
    /// more spelling of root 0. The game is launched from a staged directory
    /// holding only its PE closure, and it resolves `Data\` relative to **its
    /// own executable**, not the working directory — measured 2026-08-12, a
    /// new game asked for `…\vfs-stage-21728\data\ccasvsse001-almsivi.esm` and
    /// friends. With only the virtual root mapped those fell through to a
    /// directory containing six files, so the plugin set came back empty and
    /// world load never completed while the main menu, which resolves through
    /// the root, worked fine. It also fixes tools that resolve beside the
    /// executable — SKSE looks for `Data\SKSE\Plugins\` there.
    ///
    /// This is now `RootMap`'s canonicalising answer rather than a lowercased
    /// `strip_prefix`, so a device-path, volume-GUID, `GLOBALROOT`, UNC
    /// admin-share, junction-alias or 8.3 short-name spelling of an in-root
    /// path routes here exactly as `RootMap::decide` already classified it.
    /// See the `roots` field for what that asymmetry cost.
    /// An alternate-data-stream suffix (`f.esp:s`) is carried through into the
    /// vpath, because `canonicalise` — correctly, for its own purpose —
    /// discards it: `f.esp:s` and `f.esp` are spellings of the same *file*,
    /// which is what a canonicaliser unifying spellings should say. But a
    /// request for a named stream is not a request for the file's default
    /// stream, and resolving one to the other would answer `f.esp:s` with
    /// `f.esp`'s bytes. The string predicate this replaced kept the suffix by
    /// accident (it never parsed the path at all); keeping it deliberately
    /// preserves that behaviour, so a stream nothing serves still comes back
    /// not-found. Verified by `vfs-fixture-escape`'s vector 11, which flipped
    /// to `opened` the moment the suffix was dropped.
    pub fn vpath_under_root(&self, path: &str) -> Option<(RootId, String)> {
        let (_, stream) = vfs_redirect::split_stream_suffix(path);
        let (root, comps) = self.roots.resolve(path)?;
        let mut vpath = comps.join("/");
        if let Some(stream) = stream {
            // `vfs_core::fold`, matching the components `RootMap` already
            // folded: one string on the wire, one fold. Behaviourally
            // identical for the ASCII stream names anything real uses, but a
            // second fold spelled differently inside the very function that
            // builds the wire vpath is how the ring's two sides drifted apart
            // in the first place.
            vpath.push_str(&vfs_core::fold(stream));
        }
        Some((root, vpath))
    }
}

/// Parse [`vfs_env::VIRTUAL_ROOTS`] (`id=path;id=path…`) on top of
/// [`vfs_env::VIRTUAL_DIR`] (root 0).
///
/// Root 0 is seeded from `VIRTUAL_DIR` first so a session that declares no
/// extra roots — every session before stage 2b — is byte-for-byte the old
/// single-root case. An entry whose id will not parse is skipped rather than
/// failing the launch; an entry naming id 0 replaces `VIRTUAL_DIR`'s path,
/// because a caller that spelled root 0 explicitly meant it.
fn roots_from_env(virtual_dir: &str) -> Vec<(RootId, String)> {
    merge_extra_roots(virtual_dir, vfs_env::text(vfs_env::VIRTUAL_ROOTS).as_deref())
}

/// The parsing half of [`roots_from_env`], split out so it can be tested
/// without writing to the process environment — which is global mutable state
/// every other test in this binary shares.
fn merge_extra_roots(virtual_dir: &str, spec: Option<&str>) -> Vec<(RootId, String)> {
    let mut out: Vec<(RootId, String)> = vec![(RootId::DEFAULT, virtual_dir.to_string())];
    let Some(extra) = spec else {
        return out;
    };
    for entry in extra.split(';') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let Some((id, path)) = entry.split_once('=') else {
            continue;
        };
        let (Ok(id), path) = (id.trim().parse::<u32>(), path.trim()) else {
            continue;
        };
        if path.is_empty() {
            continue;
        }
        match out.iter_mut().find(|(r, _)| r.0 == id) {
            Some(slot) => slot.1 = path.to_string(),
            None => out.push((RootId(id), path.to_string())),
        }
    }
    out
}

/// Directory holding the staged launch image, derived from [`vfs_env::LAUNCH_IMAGE`].
///
/// The staged EXE and its imports live there physically; everything else the
/// game asks for beside it must come from the VFS.
///
/// This read used the pre-rename name until 2026-08-13, so the alias silently
/// resolved to nothing after the hollow removal renamed the writer. It did not
/// bite because the same change moved the child's cwd to the managed root — but
/// nothing reported it, which is why the name now comes from `vfs-env`.
fn stage_root_from_env() -> Option<String> {
    let host = vfs_env::text(vfs_env::LAUNCH_IMAGE)?;
    let dir = std::path::Path::new(&host).parent()?;
    let s = dir.to_string_lossy();
    if s.is_empty() {
        return None;
    }
    Some(normalize_path_for_root(&s))
}

pub fn strip_nt_device(p: &str) -> &str {
    p.strip_prefix(r"\??\")
        .or_else(|| p.strip_prefix(r"\\?\"))
        .unwrap_or(p)
}

pub fn normalize_path_for_root(p: &str) -> String {
    strip_nt_device(p)
        .replace('/', "\\")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The unified predicate, exercised the way `FuseClient::vpath_under_root`
    /// exercises it (`RootMap::resolve` then join), without needing a live
    /// ring. `vpath_under_root_norm` — the lowercased `strip_prefix` this
    /// replaced — is gone; these are its cases, re-asserted against the
    /// canonicalising predicate so nothing it used to answer regressed.
    fn resolve(map: &RootMap, path: &str) -> Option<(RootId, String)> {
        let (_, stream) = vfs_redirect::split_stream_suffix(path);
        map.resolve(path).map(|(r, c)| {
            let mut v = c.join("/");
            if let Some(s) = stream {
                v.push_str(&vfs_core::fold(s));
            }
            (r, v)
        })
    }

    /// A named alternate data stream must not resolve to the file's default
    /// stream. `canonicalise` discards the suffix — right for unifying
    /// spellings of a file, wrong for building the vpath the director is
    /// asked about — so the client re-attaches it. Without this,
    /// `vfs-fixture-escape`'s vector 11 (read-only `OPEN_EXISTING` on a
    /// stream nothing pre-creates) opens and gets the base file's bytes
    /// instead of not-found.
    #[test]
    fn an_alternate_data_stream_keeps_its_suffix_in_the_vpath() {
        let m = FuseClient::roots_only(&[(RootId::DEFAULT, r"C:\Games\Skyrim")]);
        assert_eq!(
            resolve(&m, r"\??\C:\Games\Skyrim\Data\a.esp:probe"),
            Some((RootId::DEFAULT, "data/a.esp:probe".to_string()))
        );
        // The drive's own colon is not a stream separator.
        assert_eq!(
            resolve(&m, r"\??\C:\Games\Skyrim\Data\a.esp"),
            Some((RootId::DEFAULT, "data/a.esp".to_string()))
        );
    }

    #[test]
    fn vpath_under_root_nt_device() {
        let m = FuseClient::roots_only(&[(RootId::DEFAULT, r"C:\GameLayers\runtime")]);
        assert_eq!(
            resolve(&m, r"\??\C:\GameLayers\runtime\Data\x.esp"),
            Some((RootId::DEFAULT, "data/x.esp".to_string()))
        );
    }

    #[test]
    fn vpath_under_root_win32() {
        let m = FuseClient::roots_only(&[(RootId::DEFAULT, r"C:\GameLayers\runtime")]);
        assert_eq!(
            resolve(&m, r"C:\GameLayers\runtime\Data\a.esm"),
            Some((RootId::DEFAULT, "data/a.esm".to_string()))
        );
    }

    /// The staged launch directory must map into the same namespace as the root.
    ///
    /// Measured 2026-08-12: a new game asked for
    /// `<stage>\data\ccasvsse001-almsivi.esm` because Skyrim resolves `Data`
    /// relative to its executable, not the working directory. With only the
    /// root mapped, those fell through to bare disk, the plugin set came back
    /// empty, and world load hung while the main menu worked.
    ///
    /// It is now an alias *entry* sharing root 0's id rather than a second
    /// prefix test, so it must still resolve — and still answer with root 0,
    /// which is what routes the request to the same provider.
    #[test]
    fn stage_dir_aliases_the_virtual_root() {
        let stage = r"C:\tmp\skyrim-data\stage\vfs-stage-21728";
        let m = FuseClient::roots_only(&[
            (RootId::DEFAULT, r"C:\GameLayers\runtime"),
            (RootId::DEFAULT, stage),
        ]);
        assert_eq!(
            resolve(
                &m,
                r"\??\c:\tmp\skyrim-data\stage\vfs-stage-21728\data\ccasvsse001-almsivi.esm"
            ),
            Some((RootId::DEFAULT, "data/ccasvsse001-almsivi.esm".to_string()))
        );
        // The game probes the bare-root spelling as well as Data\.
        assert_eq!(
            resolve(
                &m,
                r"\??\c:\tmp\skyrim-data\stage\vfs-stage-21728\ccasvsse001-almsivi.esm"
            ),
            Some((RootId::DEFAULT, "ccasvsse001-almsivi.esm".to_string()))
        );
        // A sibling staging directory must not match.
        assert_eq!(resolve(&m, r"\??\c:\tmp\skyrim-data\stage\other\data\x.esl"), None);
    }

    #[test]
    fn vpath_under_root_rejects_outside() {
        let m = FuseClient::roots_only(&[(RootId::DEFAULT, r"C:\GameLayers\runtime")]);
        assert_eq!(resolve(&m, r"\??\C:\Windows\x.dll"), None);
    }

    /// Stage 2b task 5: the client predicate answers with a root id, and a
    /// second root routes to itself rather than to root 0. Before this the
    /// client held one root path and one alias, so a path under a second root
    /// was simply "not ours" and fell through to real disk.
    #[test]
    fn a_second_root_routes_to_its_own_id() {
        let m = FuseClient::roots_only(&[
            (RootId(0), r"C:\Games\Skyrim"),
            (RootId(1), r"C:\Users\me\Documents\My Games\Skyrim"),
        ]);
        assert_eq!(
            resolve(&m, r"\??\C:\Games\Skyrim\Data\a.esm"),
            Some((RootId(0), "data/a.esm".to_string()))
        );
        assert_eq!(
            resolve(&m, r"\??\C:\Users\me\Documents\My Games\Skyrim\Saves\a.ess"),
            Some((RootId(1), "saves/a.ess".to_string()))
        );
        assert_eq!(resolve(&m, r"\??\C:\Windows\x.dll"), None);
    }

    /// The predicate's own root spelling resolves to the empty remainder,
    /// which every caller turns into `"."`. Both roots, not just the first.
    #[test]
    fn the_root_itself_resolves_to_an_empty_remainder() {
        let m = FuseClient::roots_only(&[
            (RootId(0), r"C:\Games\Skyrim"),
            (RootId(1), r"C:\Docs\Skyrim"),
        ]);
        assert_eq!(resolve(&m, r"\??\C:\Games\Skyrim"), Some((RootId(0), String::new())));
        assert_eq!(resolve(&m, r"\??\C:\Docs\Skyrim"), Some((RootId(1), String::new())));
    }

    /// **This is the unification, stated as a test.** The five alternate
    /// spellings `RootMap` recognises and the old string predicate did not
    /// must now route, not merely classify. A device-path spelling is the
    /// cheapest of the five to build without touching the filesystem (the 8.3
    /// and junction cases need real on-disk state and are covered in
    /// `vfs-redirect`'s own tests), so it stands in for the family here.
    ///
    /// If the unification were reverted — the client going back to a
    /// lowercased `strip_prefix` — this assertion fails: `strip_prefix` has no
    /// device table and would answer `None`.
    #[test]
    fn the_client_predicate_recognises_the_spellings_only_rootmap_used_to() {
        let mut volumes = vfs_redirect::VolumeMap::empty();
        volumes.insert(r"\Device\HarddiskVolume3", 'C');
        let m = RootMap::with_roots(
            &[(RootId(0), r"C:\Games\Skyrim"), (RootId(1), r"C:\Docs\Skyrim")],
            volumes,
        )
        .unwrap();
        assert_eq!(
            resolve(&m, r"\Device\HarddiskVolume3\Games\Skyrim\Data\a.esp"),
            Some((RootId(0), "data/a.esp".to_string())),
            "a device-path spelling of an in-root path must now route, not just classify"
        );
        assert_eq!(
            resolve(&m, r"\Device\HarddiskVolume3\Docs\Skyrim\Saves\a.ess"),
            Some((RootId(1), "saves/a.ess".to_string())),
            "and for every root, not only the first"
        );
        // The over-eager direction stays closed: registering a device prefix
        // must not swallow the rest of the volume.
        assert_eq!(resolve(&m, r"\Device\HarddiskVolume3\Windows\System32\x.dll"), None);
    }

    /// `VFS_VIRTUAL_ROOTS` is additive on top of `VFS_VIRTUAL_DIR`. Exercised
    /// through the pure half, not the process environment — that is global
    /// mutable state every other test in this binary shares.
    #[test]
    fn extra_roots_parse_additively_over_root_zero() {
        let game = r"C:\Games\Skyrim";
        // Unset: exactly the single-root case every pre-stage-2b session had.
        assert_eq!(
            merge_extra_roots(game, None),
            vec![(RootId::DEFAULT, game.to_string())]
        );
        // One extra root, plus tolerance for whitespace and a trailing `;`.
        assert_eq!(
            merge_extra_roots(game, Some(r" 1 = C:\Docs\Skyrim ; ")),
            vec![
                (RootId(0), game.to_string()),
                (RootId(1), r"C:\Docs\Skyrim".to_string()),
            ]
        );
        // An explicit root 0 replaces VFS_VIRTUAL_DIR rather than duplicating
        // it — two entries for one id would be read as an alias, which is not
        // what a caller respelling root 0 means.
        assert_eq!(
            merge_extra_roots(game, Some(r"0=C:\Other")),
            vec![(RootId(0), r"C:\Other".to_string())]
        );
        // Malformed entries are skipped, not fatal, and never shift the roots
        // that did parse.
        assert_eq!(
            merge_extra_roots(game, Some(r"nope;2=;=C:\x;3=C:\Three")),
            vec![
                (RootId(0), game.to_string()),
                (RootId(3), r"C:\Three".to_string()),
            ]
        );
    }
}

//! Thin director FUSE client: ring + bulk arena + optional remote READ + event wake.

use std::sync::OnceLock;

use vfs_ipc::{Geom, RingClient, SpinNotifier};
use vfs_protocol::{
    decode_getattr_resp, decode_open_resp, decode_read_bulk_resp, decode_read_remote_resp,
    decode_readdir_resp, decode_read_resp_into, encode_close_req, encode_open_req, encode_path_req,
    encode_read_req, encode_register_process_req, is_read_resp_bulk, is_read_resp_remote, AttrResp,
    DirEntryWire, OpenResp, ReadReq, FLAG_READ_BULK, FLAG_READ_REMOTE, OP_CLOSE, OP_GETATTR,
    OP_HEARTBEAT, OP_OPEN, OP_READ, OP_READDIR, OP_REGISTER_PROCESS, OPEN_READ, ST_OK,
};
use vfs_win::SharedMapping;

static FUSE: OnceLock<FuseClient> = OnceLock::new();

pub fn global() -> Option<&'static FuseClient> {
    FUSE.get()
}

pub fn try_init_from_env() -> Result<(), String> {
    if FUSE.get().is_some() {
        return Ok(());
    }
    let section = std::env::var("VFS_RING_SECTION").map_err(|_| "VFS_RING_SECTION unset")?;
    let ring_bytes: usize = std::env::var("VFS_RING_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2 * 1024 * 1024);
    let payload_cap: u32 = std::env::var("VFS_RING_PAYLOAD_CAP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_048_576);
    let arena_len: usize = std::env::var("VFS_ARENA_LEN")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let root = std::env::var("VFS_VIRTUAL_DIR").unwrap_or_else(|_| r"C:\GameLayers\runtime".into());
    // Phase 2 remote READ: register for WPM. Default: bulk preferred when arena exists
    // (bench: bulk ~3× faster than WPM). Set VFS_PREFER_REMOTE=1 to force remote for large READs.
    let remote_enabled = std::env::var("VFS_REMOTE_READ")
        .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(true);
    let prefer_remote = std::env::var("VFS_PREFER_REMOTE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let client = FuseClient::connect(
        &section,
        &root,
        payload_cap,
        ring_bytes,
        arena_len,
        remote_enabled,
        prefer_remote,
    )?;
    client.heartbeat()?;
    if remote_enabled {
        let _ = client.register_process();
    }
    let _ = FUSE.set(client);
    Ok(())
}

const PIPELINE_DEPTH: usize = 4;
const BULK_THRESHOLD: u32 = 64 * 1024;
/// Director WPM threshold when remote is selected (**phase 2**).
const REMOTE_THRESHOLD: u32 = 256 * 1024;

pub struct FuseClient {
    mapping: SharedMapping,
    geom: Geom,
    payload_cap: u32,
    root_lower: String,
    arena_len: usize,
    /// Director accepted REGISTER_PROCESS (or we still try and fall back).
    remote_ok: std::sync::atomic::AtomicBool,
    /// When true, prefer WPM over bulk arena for large fragments.
    prefer_remote: bool,
}

impl FuseClient {
    pub fn connect(
        section: &str,
        root: &str,
        payload_cap: u32,
        ring_bytes: usize,
        arena_len: usize,
        remote_enabled: bool,
        prefer_remote: bool,
    ) -> Result<Self, String> {
        let mapping = SharedMapping::open(section, ring_bytes)
            .map_err(|e| format!("open section {section}: {e}"))?;
        let geom = vfs_ipc::ring::open(mapping.seg()).map_err(|e| format!("ring open: {e:?}"))?;
        Ok(FuseClient {
            mapping,
            geom,
            payload_cap,
            root_lower: root.replace('/', "\\").to_ascii_lowercase(),
            arena_len,
            remote_ok: std::sync::atomic::AtomicBool::new(remote_enabled),
            prefer_remote,
        })
    }

    fn client(&self) -> RingClient<'_, SpinNotifier> {
        RingClient::with_geom(self.mapping.seg(), self.geom, SpinNotifier)
    }

    pub fn heartbeat(&self) -> Result<(), String> {
        let c = self.client();
        let r = c
            .submit(OP_HEARTBEAT, 0, &[])
            .map_err(|e| format!("HEARTBEAT: {e:?}"))?;
        if r.status != ST_OK {
            return Err(format!("HEARTBEAT status {}", r.status));
        }
        Ok(())
    }

    /// Register this process so the director can WPM into our buffers.
    pub fn register_process(&self) -> Result<(), String> {
        let pid = std::process::id();
        let c = self.client();
        let r = c
            .submit(
                OP_REGISTER_PROCESS,
                0,
                &encode_register_process_req(pid),
            )
            .map_err(|e| format!("REGISTER_PROCESS: {e:?}"))?;
        if r.status != ST_OK {
            self.remote_ok
                .store(false, std::sync::atomic::Ordering::Relaxed);
            return Err(format!("REGISTER_PROCESS status {}", r.status));
        }
        self.remote_ok
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    pub fn getattr(&self, vpath: &str) -> Result<AttrResp, i32> {
        let c = self.client();
        let r = c
            .submit(OP_GETATTR, 0, &encode_path_req(vpath))
            .map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        if r.status != ST_OK {
            return Err(r.status);
        }
        decode_getattr_resp(&r.payload).ok_or(vfs_protocol::ST_BAD_REQUEST)
    }

    pub fn readdir(&self, vpath: &str) -> Result<Vec<DirEntryWire>, i32> {
        let c = self.client();
        let r = c
            .submit(OP_READDIR, 0, &encode_path_req(vpath))
            .map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        if r.status != ST_OK {
            return Err(r.status);
        }
        decode_readdir_resp(&r.payload).ok_or(vfs_protocol::ST_BAD_REQUEST)
    }

    pub fn open(&self, vpath: &str) -> Result<OpenResp, i32> {
        let c = self.client();
        let r = c
            .submit(OP_OPEN, 0, &encode_open_req(OPEN_READ, vpath))
            .map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        if r.status != ST_OK {
            return Err(r.status);
        }
        decode_open_resp(&r.payload).ok_or(vfs_protocol::ST_BAD_REQUEST)
    }

    /// Read into `buf` (typically the game's NtReadFile buffer — **phase 1**).
    ///
    /// Large fragments use **phase 2** remote WPM when registered; otherwise bulk arena
    /// + `copy_to` into `buf`, or inline ring payload.
    pub fn read_fragmented(
        &self,
        fh: u64,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, i32> {
        if buf.is_empty() {
            return Ok(0);
        }
        let bulk_chunk = if self.arena_len > 0 {
            (self.arena_len / self.geom.slot_count as usize)
                .max(256 * 1024)
                .min(1024 * 1024)
        } else {
            self.payload_cap.saturating_sub(8) as usize
        };
        let remote_chunk = bulk_chunk.min(1024 * 1024);
        let inline_chunk = self.payload_cap.saturating_sub(8) as usize;
        let remote_ok = self.remote_ok.load(std::sync::atomic::Ordering::Relaxed);
        let c = self.client();
        let mut filled = 0usize;

        while filled < buf.len() {
            let mut reqs: Vec<(u32, u32, Vec<u8>)> = Vec::new();
            let mut wants: Vec<usize> = Vec::new();
            let mut modes: Vec<ReadMode> = Vec::new();
            let mut batch_off = filled;
            while reqs.len() < PIPELINE_DEPTH && batch_off < buf.len() {
                let rem = buf.len() - batch_off;
                // Prefer bulk arena when available (faster than WPM on measured hosts).
                // Remote when: no arena, or VFS_PREFER_REMOTE, and size ≥ threshold.
                let use_remote = remote_ok
                    && rem as u32 >= REMOTE_THRESHOLD
                    && (self.arena_len == 0 || self.prefer_remote);
                let bulk = !use_remote && rem as u32 >= BULK_THRESHOLD && self.arena_len > 0;
                let chunk = if use_remote {
                    rem.min(remote_chunk)
                } else if bulk {
                    rem.min(bulk_chunk)
                } else {
                    rem.min(inline_chunk)
                } as u32;
                if chunk == 0 {
                    break;
                }
                let dest_va = buf.as_mut_ptr() as u64 + batch_off as u64;
                let (flags, target_va, mode) = if use_remote {
                    (
                        FLAG_READ_REMOTE,
                        Some(dest_va),
                        ReadMode::Remote,
                    )
                } else if bulk {
                    (FLAG_READ_BULK, None, ReadMode::Bulk)
                } else {
                    (0, None, ReadMode::Inline)
                };
                reqs.push((
                    OP_READ,
                    flags,
                    encode_read_req(&ReadReq {
                        fh,
                        offset: offset + batch_off as u64,
                        len: chunk,
                        target_va,
                    }),
                ));
                wants.push(chunk as usize);
                modes.push(mode);
                batch_off += chunk as usize;
            }
            if reqs.is_empty() {
                break;
            }

            let responses = c
                .submit_many(&reqs)
                .map_err(|_| vfs_protocol::ST_IO_ERROR)?;

            let mut batch_filled = 0usize;
            let mut eof = false;
            for ((resp, want), mode) in responses
                .iter()
                .zip(wants.iter())
                .zip(modes.iter())
            {
                if resp.status != ST_OK {
                    // Remote failed (e.g. no registered process): disable and surface error
                    // so the caller can retry or fail; for mid-batch, fail the read.
                    if *mode == ReadMode::Remote {
                        self.remote_ok
                            .store(false, std::sync::atomic::Ordering::Relaxed);
                    }
                    return Err(resp.status);
                }
                let frag_start = filled + batch_filled;
                let dest = &mut buf[frag_start..frag_start + *want];
                let n = if is_read_resp_remote(&resp.payload) {
                    // Phase 2: director already wrote into dest VA.
                    let bn = decode_read_remote_resp(&resp.payload)
                        .ok_or(vfs_protocol::ST_BAD_REQUEST)?;
                    (bn as usize).min(dest.len())
                } else if is_read_resp_bulk(&resp.payload) {
                    let (bn, aoff) = decode_read_bulk_resp(&resp.payload)
                        .ok_or(vfs_protocol::ST_BAD_REQUEST)?;
                    let n = (bn as usize).min(dest.len());
                    if n > 0 {
                        // Phase 1: arena → game buffer (no intermediate Vec).
                        self.mapping
                            .seg()
                            .copy_to(aoff as usize, &mut dest[..n])
                            .ok_or(vfs_protocol::ST_IO_ERROR)?;
                    }
                    n
                } else {
                    decode_read_resp_into(&resp.payload, dest)
                        .ok_or(vfs_protocol::ST_BAD_REQUEST)?
                };
                batch_filled += n;
                if n < *want {
                    eof = true;
                    break;
                }
            }
            filled += batch_filled;
            if eof || batch_filled == 0 {
                break;
            }
        }
        Ok(filled)
    }

    pub fn close(&self, fh: u64) -> Result<(), i32> {
        let c = self.client();
        let r = c
            .submit(OP_CLOSE, 0, &encode_close_req(fh))
            .map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        if r.status != ST_OK {
            return Err(r.status);
        }
        Ok(())
    }

    pub fn vpath_under_root(&self, path: &str) -> Option<String> {
        vpath_under_root_norm(path, &self.root_lower)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadMode {
    Inline,
    Bulk,
    Remote,
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

pub fn vpath_under_root_norm(path: &str, root: &str) -> Option<String> {
    let p = normalize_path_for_root(path);
    let root = normalize_path_for_root(root);
    let root = root.trim_end_matches('\\');
    if p == root {
        return Some(String::new());
    }
    let prefix = format!("{root}\\");
    let rest = p.strip_prefix(&prefix)?;
    Some(rest.replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vpath_under_root_nt_device() {
        assert_eq!(
            vpath_under_root_norm(r"\??\C:\GameLayers\runtime\Data\x.esp", r"C:\GameLayers\runtime")
                .as_deref(),
            Some("data/x.esp")
        );
    }

    #[test]
    fn vpath_under_root_win32() {
        assert_eq!(
            vpath_under_root_norm(r"C:\GameLayers\runtime\Data\a.esm", r"C:\GameLayers\runtime")
                .as_deref(),
            Some("data/a.esm")
        );
    }

    #[test]
    fn vpath_under_root_rejects_outside() {
        assert_eq!(
            vpath_under_root_norm(r"\??\C:\Windows\x.dll", r"C:\GameLayers\runtime"),
            None
        );
    }
}

//! Thin director FUSE client: ring + bulk arena + optional event wake.

use std::sync::OnceLock;

use vfs_ipc::{Geom, RingClient, SpinNotifier};
use vfs_protocol::{
    decode_getattr_resp, decode_open_resp, decode_readdir_resp, decode_read_bulk_resp,
    decode_read_resp_into, decode_write_resp, encode_close_req, encode_mkdir_req, encode_open_req,
    encode_path_req, encode_read_req, encode_rename_req, encode_setattr_req, encode_write_req,
    is_read_resp_bulk, AttrResp, DirEntryWire, OpenResp, ReadReq, SetattrReq, WriteReq,
    FLAG_READ_BULK, OP_CLOSE, OP_DELETE, OP_GETATTR, OP_HEARTBEAT, OP_MKDIR, OP_OPEN, OP_READ,
    OP_READDIR, OP_RENAME, OP_SETATTR, OP_WRITE, OPEN_READ, OPEN_WRITE, ST_OK,
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
    let client = FuseClient::connect(&section, &root, payload_cap, ring_bytes, arena_len)?;
    client.heartbeat()?;
    let _ = FUSE.set(client);
    Ok(())
}

/// Concurrent bulk READs in flight (each uses its own arena bank via slot id).
const PIPELINE_DEPTH: usize = 8;
/// Prefer shared-section bulk over inline ring payload above this size.
const BULK_THRESHOLD: u32 = 32 * 1024;
/// Deep pipeline for multi‑MiB sequential streams (CreateSection fill).
const PIPELINE_DEPTH_STREAM: usize = 16;

pub struct FuseClient {
    mapping: SharedMapping,
    geom: Geom,
    payload_cap: u32,
    root_lower: String,
    arena_len: usize,
}

impl FuseClient {
    pub fn connect(
        section: &str,
        root: &str,
        payload_cap: u32,
        ring_bytes: usize,
        arena_len: usize,
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

    /// Open a virtual path for WRITE (the JVM overlay creates/opens it writable).
    pub fn open_write(&self, vpath: &str) -> Result<OpenResp, i32> {
        let c = self.client();
        let r = c
            .submit(OP_OPEN, 0, &encode_open_req(OPEN_WRITE, vpath))
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
        // Full bank size — do not cap at 1 MiB (that undid a 128 MiB arena).
        (self.arena_len / slots).max(256 * 1024)
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

            let responses = c
                .submit_many(&reqs)
                .map_err(|_| vfs_protocol::ST_IO_ERROR)?;

            let mut batch_filled = 0usize;
            let mut eof = false;
            for (resp, want) in responses.iter().zip(wants.iter()) {
                if resp.status != ST_OK {
                    return Err(resp.status);
                }
                let frag_start = filled + batch_filled;
                let dest = &mut buf[frag_start..frag_start + *want];
                let n = if is_read_resp_bulk(&resp.payload) {
                    let (bn, aoff) = decode_read_bulk_resp(&resp.payload)
                        .ok_or(vfs_protocol::ST_BAD_REQUEST)?;
                    let n = (bn as usize).min(dest.len());
                    if n > 0 {
                        // Shared arena → destination (one memcpy; not via ring).
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

    /// Delete (whiteout) a virtual path via the JVM overlay (`OP_DELETE`).
    pub fn delete(&self, vpath: &str) -> Result<(), i32> {
        let c = self.client();
        let r = c
            .submit(OP_DELETE, 0, &encode_path_req(vpath))
            .map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        if r.status != ST_OK {
            return Err(r.status);
        }
        Ok(())
    }

    /// Rename a virtual path to another virtual path (`OP_RENAME`).
    pub fn rename(&self, from: &str, to: &str) -> Result<(), i32> {
        let c = self.client();
        let r = c
            .submit(OP_RENAME, 0, &encode_rename_req(from, to))
            .map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        if r.status != ST_OK {
            return Err(r.status);
        }
        Ok(())
    }

    /// Create a virtual directory via the JVM overlay (`OP_MKDIR`).
    pub fn mkdir(&self, vpath: &str, mode: u32) -> Result<(), i32> {
        let c = self.client();
        let r = c
            .submit(OP_MKDIR, 0, &encode_mkdir_req(mode, vpath))
            .map_err(|_| vfs_protocol::ST_IO_ERROR)?;
        if r.status != ST_OK {
            return Err(r.status);
        }
        Ok(())
    }

    /// Truncate/extend a virtual write handle to `size` bytes (`OP_SETATTR`).
    pub fn truncate(&self, fh: u64, size: u64) -> Result<(), i32> {
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

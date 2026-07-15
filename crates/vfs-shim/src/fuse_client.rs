//! Thin director FUSE client: RingClient + fragmented READ. No zip access.
//!
//! **A3:** `decode_read_resp_into` — no second Vec for READ data.
//! **A4:** cache ring `Geom`; reuse `RingClient::with_geom` (no re-open header).
//! **A5:** pipelined multi-slot READs for sequential fragments.

use std::sync::OnceLock;

use vfs_ipc::{Geom, RingClient, SpinNotifier};
use vfs_protocol::{
    decode_getattr_resp, decode_open_resp, decode_readdir_resp, decode_read_resp_into,
    encode_close_req, encode_open_req, encode_path_req, encode_read_req, AttrResp, DirEntryWire,
    OpenResp, ReadReq, OP_CLOSE, OP_GETATTR, OP_HEARTBEAT, OP_OPEN, OP_READ, OP_READDIR, OPEN_READ,
    ST_OK,
};
use vfs_win::SharedMapping;

static FUSE: OnceLock<FuseClient> = OnceLock::new();

/// Global thin client (set once at bootstrap when fuse.cfg / env is present).
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
        .unwrap_or(1024 * 1024);
    let payload_cap: u32 = std::env::var("VFS_RING_PAYLOAD_CAP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(262_144);
    let root = std::env::var("VFS_VIRTUAL_DIR").unwrap_or_else(|_| r"C:\GameLayers\runtime".into());
    let client = FuseClient::connect(&section, &root, payload_cap, ring_bytes)?;
    client.heartbeat()?;
    let _ = FUSE.set(client);
    Ok(())
}

/// Max in-flight READ slots for A5 pipelining (must be ≤ ring slot_count).
const PIPELINE_DEPTH: usize = 4;

pub struct FuseClient {
    /// Keep mapping alive for process lifetime.
    _mapping: SharedMapping,
    /// **A4:** cached geometry from connect-time `ring::open`.
    geom: Geom,
    payload_cap: u32,
    root_lower: String,
}

impl FuseClient {
    pub fn connect(
        section: &str,
        root: &str,
        payload_cap: u32,
        ring_bytes: usize,
    ) -> Result<Self, String> {
        let mapping = SharedMapping::open(section, ring_bytes)
            .map_err(|e| format!("open section {section}: {e}"))?;
        let geom = vfs_ipc::ring::open(mapping.seg()).map_err(|e| format!("ring open: {e:?}"))?;
        Ok(FuseClient {
            _mapping: mapping,
            geom,
            payload_cap,
            root_lower: root.replace('/', "\\").to_ascii_lowercase(),
        })
    }

    /// **A4:** cheap client construction with cached geom.
    fn client(&self) -> RingClient<'_, SpinNotifier> {
        RingClient::with_geom(self._mapping.seg(), self.geom, SpinNotifier)
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

    /// Fragmented READ into `buf`.
    /// **A3:** no intermediate data Vec (decode into `buf` slices).
    /// **A5:** pipeline up to `PIPELINE_DEPTH` outstanding READs.
    pub fn read_fragmented(
        &self,
        fh: u64,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, i32> {
        if buf.is_empty() {
            return Ok(0);
        }
        let max_chunk = self.payload_cap.saturating_sub(8) as usize;
        let c = self.client();
        let mut filled = 0usize;

        while filled < buf.len() {
            // Plan a batch of sequential fragments.
            let mut reqs: Vec<(u32, u32, Vec<u8>)> = Vec::new();
            let mut batch_off = filled;
            while reqs.len() < PIPELINE_DEPTH && batch_off < buf.len() {
                let chunk = (buf.len() - batch_off).min(max_chunk) as u32;
                if chunk == 0 {
                    break;
                }
                let payload = encode_read_req(&ReadReq {
                    fh,
                    offset: offset + batch_off as u64,
                    len: chunk,
                });
                reqs.push((OP_READ, 0, payload));
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
            for (i, resp) in responses.iter().enumerate() {
                if resp.status != ST_OK {
                    return Err(resp.status);
                }
                // Destination slice for this fragment.
                let frag_start = filled + batch_filled;
                let want = {
                    // re-derive chunk length from request plan
                    let rem = buf.len() - frag_start;
                    rem.min(max_chunk)
                };
                if want == 0 {
                    break;
                }
                let dest = &mut buf[frag_start..frag_start + want];
                let n = decode_read_resp_into(&resp.payload, dest)
                    .ok_or(vfs_protocol::ST_BAD_REQUEST)?;
                batch_filled += n;
                if n < want {
                    eof = true;
                    break;
                }
                let _ = i;
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

    /// If `path` is under managed root, return vpath with `/` separators.
    pub fn vpath_under_root(&self, path: &str) -> Option<String> {
        vpath_under_root_norm(path, &self.root_lower)
    }
}

/// Strip `\??\` / `\\?\` device prefixes; leave Win32 path (drive intact).
pub fn strip_nt_device(p: &str) -> &str {
    p.strip_prefix(r"\??\")
        .or_else(|| p.strip_prefix(r"\\?\"))
        .unwrap_or(p)
}

/// Normalize path for root comparison: strip NT device, unify slashes, lower.
pub fn normalize_path_for_root(p: &str) -> String {
    strip_nt_device(p)
        .replace('/', "\\")
        .to_ascii_lowercase()
}

/// Pure: if `path` is under `root` (either may be NT or Win32), return relative
/// vpath with `/` separators; empty string means the root directory itself.
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
    fn strip_nt_device_forms() {
        assert_eq!(strip_nt_device(r"\??\C:\GameLayers\runtime"), r"C:\GameLayers\runtime");
        assert_eq!(strip_nt_device(r"\\?\C:\GameLayers\runtime"), r"C:\GameLayers\runtime");
        assert_eq!(strip_nt_device(r"C:\GameLayers\runtime"), r"C:\GameLayers\runtime");
    }

    #[test]
    fn vpath_under_root_win32() {
        let root = r"C:\GameLayers\runtime";
        assert_eq!(
            vpath_under_root_norm(r"C:\GameLayers\runtime\Data\Skyrim.esm", root).as_deref(),
            Some("data/skyrim.esm")
        );
        assert_eq!(vpath_under_root_norm(root, root).as_deref(), Some(""));
    }

    #[test]
    fn vpath_under_root_nt_device() {
        let root = r"C:\GameLayers\runtime";
        assert_eq!(
            vpath_under_root_norm(r"\??\C:\GameLayers\runtime\Data\x.esp", root).as_deref(),
            Some("data/x.esp")
        );
        assert_eq!(
            vpath_under_root_norm(r"\\?\C:\GameLayers\runtime\Data\x.esp", root).as_deref(),
            Some("data/x.esp")
        );
        assert_eq!(
            vpath_under_root_norm(r"\??\C:\GameLayers\runtime", root).as_deref(),
            Some("")
        );
        assert_eq!(
            vpath_under_root_norm(
                r"\??\C:\GameLayers\runtime\Data\Skyrim.esm",
                r"\??\C:\GameLayers\runtime"
            )
            .as_deref(),
            Some("data/skyrim.esm")
        );
    }

    #[test]
    fn vpath_under_root_rejects_outside() {
        let root = r"C:\GameLayers\runtime";
        assert_eq!(
            vpath_under_root_norm(r"\??\C:\Windows\System32\kernel32.dll", root),
            None
        );
        assert_eq!(
            vpath_under_root_norm(r"C:\GameLayers\other\file.bin", root),
            None
        );
    }
}

//! Thin director FUSE client: RingClient + fragmented READ. No zip access.

use std::sync::OnceLock;

use vfs_ipc::{RingClient, SpinNotifier};
use vfs_protocol::{
    decode_getattr_resp, decode_open_resp, decode_readdir_resp, decode_read_resp,
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

pub struct FuseClient {
    // Keep mapping alive for process lifetime.
    _mapping: SharedMapping,
    // RingClient borrows SharedSeg — we re-create per call from mapping to avoid
    // self-referential structs. SpinNotifier is zero-sized.
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
        // Validate ring layout.
        let _ = vfs_ipc::ring::open(mapping.seg()).map_err(|e| format!("ring open: {e:?}"))?;
        Ok(FuseClient {
            _mapping: mapping,
            payload_cap,
            root_lower: root.replace('/', "\\").to_ascii_lowercase(),
        })
    }

    fn with_client<R>(&self, f: impl FnOnce(&RingClient<'_, SpinNotifier>) -> R) -> R {
        // SAFETY: mapping view lives as long as self.
        let seg = self._mapping.seg();
        let client = RingClient::new(seg, SpinNotifier).expect("ring open already validated");
        f(&client)
    }

    pub fn heartbeat(&self) -> Result<(), String> {
        self.with_client(|c| {
            let r = c
                .submit(OP_HEARTBEAT, 0, &[])
                .map_err(|e| format!("HEARTBEAT: {e:?}"))?;
            if r.status != ST_OK {
                return Err(format!("HEARTBEAT status {}", r.status));
            }
            Ok(())
        })
    }

    pub fn getattr(&self, vpath: &str) -> Result<AttrResp, i32> {
        self.with_client(|c| {
            let r = c
                .submit(OP_GETATTR, 0, &encode_path_req(vpath))
                .map_err(|_| vfs_protocol::ST_IO_ERROR)?;
            if r.status != ST_OK {
                return Err(r.status);
            }
            decode_getattr_resp(&r.payload).ok_or(vfs_protocol::ST_BAD_REQUEST)
        })
    }

    pub fn readdir(&self, vpath: &str) -> Result<Vec<DirEntryWire>, i32> {
        self.with_client(|c| {
            let r = c
                .submit(OP_READDIR, 0, &encode_path_req(vpath))
                .map_err(|_| vfs_protocol::ST_IO_ERROR)?;
            if r.status != ST_OK {
                return Err(r.status);
            }
            decode_readdir_resp(&r.payload).ok_or(vfs_protocol::ST_BAD_REQUEST)
        })
    }

    pub fn open(&self, vpath: &str) -> Result<OpenResp, i32> {
        self.with_client(|c| {
            let r = c
                .submit(OP_OPEN, 0, &encode_open_req(OPEN_READ, vpath))
                .map_err(|_| vfs_protocol::ST_IO_ERROR)?;
            if r.status != ST_OK {
                return Err(r.status);
            }
            decode_open_resp(&r.payload).ok_or(vfs_protocol::ST_BAD_REQUEST)
        })
    }

    pub fn read_fragmented(
        &self,
        fh: u64,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, i32> {
        let max_chunk = self.payload_cap.saturating_sub(8) as usize;
        let mut filled = 0usize;
        while filled < buf.len() {
            let chunk = (buf.len() - filled).min(max_chunk) as u32;
            let data = self.with_client(|c| {
                let r = c
                    .submit(
                        OP_READ,
                        0,
                        &encode_read_req(&ReadReq {
                            fh,
                            offset: offset + filled as u64,
                            len: chunk,
                        }),
                    )
                    .map_err(|_| vfs_protocol::ST_IO_ERROR)?;
                if r.status != ST_OK {
                    return Err(r.status);
                }
                decode_read_resp(&r.payload).ok_or(vfs_protocol::ST_BAD_REQUEST)
            })?;
            if data.is_empty() {
                break;
            }
            let n = data.len().min(buf.len() - filled);
            buf[filled..filled + n].copy_from_slice(&data[..n]);
            filled += n;
            if data.len() < chunk as usize {
                break;
            }
        }
        Ok(filled)
    }

    pub fn close(&self, fh: u64) -> Result<(), i32> {
        self.with_client(|c| {
            let r = c
                .submit(OP_CLOSE, 0, &encode_close_req(fh))
                .map_err(|_| vfs_protocol::ST_IO_ERROR)?;
            if r.status != ST_OK {
                return Err(r.status);
            }
            Ok(())
        })
    }

    /// If `win32_path` is under managed root, return vpath with `/` separators.
    pub fn vpath_under_root(&self, win32_path: &str) -> Option<String> {
        let p = win32_path.replace('/', "\\").to_ascii_lowercase();
        let root = self.root_lower.trim_end_matches('\\');
        if p == root {
            return Some(String::new());
        }
        let prefix = format!("{root}\\");
        let rest = p.strip_prefix(&prefix)?;
        Some(rest.replace('\\', "/"))
    }
}

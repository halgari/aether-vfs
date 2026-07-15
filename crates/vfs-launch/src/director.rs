//! Parent-process director: control ring server over a named shared section.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use vfs_core::VfsTree;
use vfs_ipc::ring::init;
use vfs_ipc::{RingClient, RingServer, SpinNotifier};
use vfs_protocol::{
    decode_open_resp, decode_read_resp, encode_close_req, encode_open_req, encode_read_req,
    OpenResp, ReadReq, OP_CLOSE, OP_HEARTBEAT, OP_OPEN, OP_READ, OPEN_READ, ST_OK,
};
use vfs_server::Server;
use vfs_win::SharedMapping;

pub const DEFAULT_PAYLOAD_CAP: u32 = 262_144;
pub const DEFAULT_SLOT_COUNT: u32 = 32;

struct DirectorInner {
    mapping: SharedMapping,
    server: Server,
    stop: AtomicBool,
}

/// Running director: keeps the mapping + server thread alive.
pub struct Director {
    pub section_name: String,
    pub payload_cap: u32,
    pub ring_bytes: usize,
    inner: Arc<DirectorInner>,
    join: Option<JoinHandle<()>>,
}

impl Director {
    /// Create a named section, init the ring, spawn the serve loop.
    pub fn start(tree: VfsTree, section_name: String) -> Result<Self, String> {
        let payload_cap = DEFAULT_PAYLOAD_CAP;
        let slot_count = DEFAULT_SLOT_COUNT;
        let stride = ((32 + payload_cap as usize) + 7) & !7;
        let ring_bytes = 40 + slot_count as usize * stride;
        let map_size = ((ring_bytes + 0xFFFF) & !0xFFFF).max(1024 * 1024);

        let mapping = SharedMapping::create(&section_name, map_size)
            .map_err(|e| format!("create section {section_name}: {e}"))?;
        init(mapping.seg(), slot_count, payload_cap)
            .map_err(|e| format!("ring init: {e:?}"))?;

        let server = Server::with_payload_cap(tree, payload_cap);
        let inner = Arc::new(DirectorInner {
            mapping,
            server,
            stop: AtomicBool::new(false),
        });
        let inner2 = inner.clone();
        let join = thread::spawn(move || {
            let ring = match RingServer::new(inner2.mapping.seg(), SpinNotifier) {
                Ok(r) => r,
                Err(_) => return,
            };
            while !inner2.stop.load(Ordering::Relaxed) {
                match inner2.server.serve_one(&ring) {
                    Ok(true) => {}
                    Ok(false) => thread::sleep(Duration::from_millis(1)),
                    Err(_) => break,
                }
            }
        });

        Ok(Director {
            section_name,
            payload_cap,
            ring_bytes: map_size,
            inner,
            join: Some(join),
        })
    }

    /// In-process ring client against this director (same process probe).
    pub fn client(&self) -> Result<RingClient<'_, SpinNotifier>, String> {
        RingClient::new(self.inner.mapping.seg(), SpinNotifier)
            .map_err(|e| format!("RingClient: {e:?}"))
    }

    pub fn stop(mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for Director {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Thin shim config: KEY=value lines the child parses for ring attach.
pub fn write_thin_config(
    path: &std::path::Path,
    section: &str,
    root: &str,
    payload_cap: u32,
    ring_bytes: usize,
) -> Result<(), String> {
    let body = format!(
        "section={section}\nroot={root}\npayload_cap={payload_cap}\nring_bytes={ring_bytes}\n"
    );
    std::fs::write(path, body).map_err(|e| format!("write thin config: {e}"))
}

/// OPEN+READ a virtual path fully via the ring client (fragmented).
pub fn rpc_read_all(
    client: &RingClient<'_, SpinNotifier>,
    vpath: &str,
    payload_cap: u32,
) -> Result<(u64, Vec<u8>), String> {
    let hb = client
        .submit(OP_HEARTBEAT, 0, &[])
        .map_err(|e| format!("HEARTBEAT: {e:?}"))?;
    if hb.status != ST_OK {
        return Err(format!("HEARTBEAT status {}", hb.status));
    }
    let open = client
        .submit(OP_OPEN, 0, &encode_open_req(OPEN_READ, vpath))
        .map_err(|e| format!("OPEN {vpath}: {e:?}"))?;
    if open.status != ST_OK {
        return Err(format!("OPEN {vpath} status {}", open.status));
    }
    let OpenResp { fh, size, .. } =
        decode_open_resp(&open.payload).ok_or_else(|| "OPEN decode".to_string())?;
    let max_chunk = payload_cap.saturating_sub(8) as u64;
    let mut out = Vec::with_capacity(size as usize);
    let mut off = 0u64;
    while off < size {
        let want = ((size - off).min(max_chunk)) as u32;
        let resp = client
            .submit(
                OP_READ,
                0,
                &encode_read_req(&ReadReq {
                    fh,
                    offset: off,
                    len: want,
                }),
            )
            .map_err(|e| format!("READ {vpath}: {e:?}"))?;
        if resp.status != ST_OK {
            let _ = client.submit(OP_CLOSE, 0, &encode_close_req(fh));
            return Err(format!("READ {vpath} status {}", resp.status));
        }
        let chunk =
            decode_read_resp(&resp.payload).ok_or_else(|| "READ decode".to_string())?;
        if chunk.is_empty() {
            break;
        }
        off += chunk.len() as u64;
        out.extend_from_slice(&chunk);
        if chunk.len() < want as usize {
            break;
        }
    }
    let _ = client.submit(OP_CLOSE, 0, &encode_close_req(fh));
    Ok((size, out))
}

/// Parse thin config from a file.
pub fn parse_thin_config(text: &str) -> Option<(String, String, u32, usize)> {
    let mut section = None;
    let mut root = None;
    let mut payload_cap = DEFAULT_PAYLOAD_CAP;
    let mut ring_bytes = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k.trim() {
            "section" => section = Some(v.trim().to_string()),
            "root" => root = Some(v.trim().to_string()),
            "payload_cap" => payload_cap = v.trim().parse().unwrap_or(payload_cap),
            "ring_bytes" => ring_bytes = v.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    Some((section?, root?, payload_cap, ring_bytes))
}

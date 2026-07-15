//! Parent-process director: control ring + bulk arena + worker pool + events.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use vfs_core::VfsTree;
use vfs_ipc::ring::{self, Geom};
use vfs_ipc::{Notifier, RingClient, RingServer, SpinNotifier};
use vfs_protocol::{
    decode_open_resp, decode_read_bulk_resp, decode_read_resp, decode_register_process_req,
    encode_close_req, encode_open_req, encode_read_req, is_read_resp_bulk, OpenResp, ReadReq,
    FLAG_READ_BULK, OP_CLOSE, OP_HEARTBEAT, OP_OPEN, OP_READ, OP_REGISTER_PROCESS, OPEN_READ, ST_OK,
    ST_BAD_REQUEST, ST_IO_ERROR,
};
use vfs_server::{DataArena, RemoteMemWriter, Server, DEFAULT_PAYLOAD_CAP, DEFAULT_WORKER_COUNT};
use vfs_win::{EventNotifier, ProcessVm, SharedMapping};

pub const DEFAULT_SLOT_COUNT: u32 = 32;
/// **B1/C2:** bulk arena after the control ring (32 MiB → ~1 MiB banks @ 32 slots).
pub const DEFAULT_ARENA_BYTES: usize = 32 * 1024 * 1024;

struct ProcessVmWriter(ProcessVm);

impl RemoteMemWriter for ProcessVmWriter {
    fn write_at(&self, va: u64, data: &[u8]) -> Result<(), i32> {
        self.0.write_at(va, data).map_err(|_| ST_IO_ERROR)
    }
}

struct DirectorInner {
    mapping: SharedMapping,
    server: Server,
    stop: AtomicBool,
    geom: Geom,
    arena_offset: usize,
    arena_len: usize,
    /// Keep named events alive for workers + shim (**B4**).
    _events: EventNotifier,
    server_ev_name: String,
    client_ev_name: String,
    /// Game process for phase-2 remote READ (WPM).
    target: Mutex<Option<Arc<ProcessVmWriter>>>,
}

/// Running director: keeps the mapping + workers alive.
pub struct Director {
    pub section_name: String,
    pub payload_cap: u32,
    pub ring_bytes: usize,
    pub arena_offset: usize,
    pub arena_len: usize,
    pub server_ev_name: String,
    pub client_ev_name: String,
    inner: Arc<DirectorInner>,
    joins: Vec<JoinHandle<()>>,
}

impl Director {
    /// Create named section (ring + arena), event pair, and **B3** worker pool.
    pub fn start(tree: VfsTree, section_name: String) -> Result<Self, String> {
        let payload_cap = DEFAULT_PAYLOAD_CAP;
        let slot_count = DEFAULT_SLOT_COUNT;
        let stride = ((32 + payload_cap as usize) + 7) & !7;
        let ring_bytes = 40 + slot_count as usize * stride;
        let arena_len = DEFAULT_ARENA_BYTES;
        let map_size = ((ring_bytes + arena_len + 0xFFFF) & !0xFFFF).max(2 * 1024 * 1024);

        let mapping = SharedMapping::create(&section_name, map_size)
            .map_err(|e| format!("create section {section_name}: {e}"))?;
        let geom = ring::init(mapping.seg(), slot_count, payload_cap)
            .map_err(|e| format!("ring init: {e:?}"))?;

        let server_ev_name = format!("{section_name}_srv");
        let client_ev_name = format!("{section_name}_cli");
        let events = EventNotifier::create(&server_ev_name, &client_ev_name)
            .map_err(|e| format!("create events: {e}"))?;

        let server = Server::with_payload_cap(tree, payload_cap);
        let arena_offset = ring_bytes;
        let inner = Arc::new(DirectorInner {
            mapping,
            server,
            stop: AtomicBool::new(false),
            geom,
            arena_offset,
            arena_len,
            _events: events,
            server_ev_name: server_ev_name.clone(),
            client_ev_name: client_ev_name.clone(),
            target: Mutex::new(None),
        });

        let workers = DEFAULT_WORKER_COUNT.max(1);
        let mut joins = Vec::with_capacity(workers);
        for _ in 0..workers {
            let inner2 = inner.clone();
            let sev = server_ev_name.clone();
            let cev = client_ev_name.clone();
            joins.push(thread::spawn(move || {
                let notifier = EventNotifier::open(&sev, &cev)
                    .or_else(|_| EventNotifier::create(&sev, &cev))
                    .ok();
                // Prefer events; fall back to spin if open fails.
                if let Some(n) = notifier {
                    worker_loop(&inner2, n);
                } else {
                    worker_loop(&inner2, SpinNotifier);
                }
            }));
        }

        Ok(Director {
            section_name,
            payload_cap,
            ring_bytes: map_size,
            arena_offset,
            arena_len,
            server_ev_name,
            client_ev_name,
            inner,
            joins,
        })
    }

    pub fn client(&self) -> Result<RingClient<'_, SpinNotifier>, String> {
        Ok(RingClient::with_geom(
            self.inner.mapping.seg(),
            self.inner.geom,
            SpinNotifier,
        ))
    }

    /// Shared segment for bulk arena copies (same process probe).
    pub fn shared_seg(&self) -> &vfs_ipc::SharedSeg {
        self.inner.mapping.seg()
    }

    pub fn stop(mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        // Nudge workers off WaitForSingleObject.
        if let Ok(n) = EventNotifier::open(&self.server_ev_name, &self.client_ev_name) {
            n.notify_server();
            n.notify_client(0);
            drop(n);
        }
        for j in self.joins.drain(..) {
            let _ = j.join();
        }
    }
}

fn worker_loop<N: vfs_ipc::Notifier>(inner: &DirectorInner, notifier: N) {
    let ring = match RingServer::new(inner.mapping.seg(), notifier) {
        Ok(r) => r,
        Err(_) => return,
    };
    let arena = DataArena::new(
        inner.mapping.seg(),
        inner.arena_offset,
        inner.arena_len,
        inner.geom.slot_count as usize,
    );
    while !inner.stop.load(Ordering::Relaxed) {
        let target = inner
            .target
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(Arc::clone));
        let remote: Option<&dyn RemoteMemWriter> =
            target.as_ref().map(|t| t.as_ref() as &dyn RemoteMemWriter);

        let handled = ring.serve_one(|req| {
            if req.opcode == OP_REGISTER_PROCESS {
                return match decode_register_process_req(&req.payload) {
                    Some(pid) => match ProcessVm::open(pid) {
                        Ok(vm) => {
                            if let Ok(mut g) = inner.target.lock() {
                                *g = Some(Arc::new(ProcessVmWriter(vm)));
                            }
                            (ST_OK, Vec::new())
                        }
                        Err(_) => (ST_IO_ERROR, Vec::new()),
                    },
                    None => (ST_BAD_REQUEST, Vec::new()),
                };
            }
            vfs_server::handler::dispatch_full(
                inner.server.tree(),
                inner.server.table(),
                req.opcode,
                &req.payload,
                req.flags,
                inner.server.payload_cap(),
                Some((&arena, req.slot)),
                remote,
            )
        });
        match handled {
            Ok(true) => {}
            Ok(false) => {}
            Err(_) => break,
        }
    }
}

impl Drop for Director {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        if let Ok(n) = EventNotifier::open(&self.inner.server_ev_name, &self.inner.client_ev_name)
        {
            n.notify_server();
            n.notify_client(0);
        }
        for j in self.joins.drain(..) {
            let _ = j.join();
        }
    }
}

/// Thin shim config: KEY=value lines.
pub fn write_thin_config(
    path: &std::path::Path,
    section: &str,
    root: &str,
    payload_cap: u32,
    ring_bytes: usize,
    arena_offset: usize,
    arena_len: usize,
    server_ev: &str,
    client_ev: &str,
) -> Result<(), String> {
    let body = format!(
        "section={section}\nroot={root}\npayload_cap={payload_cap}\nring_bytes={ring_bytes}\n\
         arena_offset={arena_offset}\narena_len={arena_len}\n\
         server_ev={server_ev}\nclient_ev={client_ev}\n"
    );
    std::fs::write(path, body).map_err(|e| format!("write thin config: {e}"))
}

/// OPEN+READ a virtual path fully via the ring client (fragmented, bulk-aware).
pub fn rpc_read_all(
    client: &RingClient<'_, SpinNotifier>,
    vpath: &str,
    payload_cap: u32,
    seg: Option<&vfs_ipc::SharedSeg>,
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
    let max_chunk = if seg.is_some() {
        1024 * 1024u32
    } else {
        payload_cap.saturating_sub(8)
    };
    let mut out = Vec::with_capacity(size as usize);
    let mut off = 0u64;
    while off < size {
        let want = ((size - off) as u32).min(max_chunk);
        let flags = if want >= 64 * 1024 && seg.is_some() {
            FLAG_READ_BULK
        } else {
            0
        };
        let resp = client
            .submit(
                OP_READ,
                flags,
                &encode_read_req(&ReadReq {
                    fh,
                    offset: off,
                    len: want,
                    target_va: None,
                }),
            )
            .map_err(|e| format!("READ {vpath}: {e:?}"))?;
        if resp.status != ST_OK {
            let _ = client.submit(OP_CLOSE, 0, &encode_close_req(fh));
            return Err(format!("READ {vpath} status {}", resp.status));
        }
        if is_read_resp_bulk(&resp.payload) {
            let (n, aoff) =
                decode_read_bulk_resp(&resp.payload).ok_or_else(|| "bulk decode".to_string())?;
            if n == 0 {
                break;
            }
            let s = seg.ok_or_else(|| "bulk without seg".to_string())?;
            let start = out.len();
            out.resize(start + n as usize, 0);
            s.copy_to(aoff as usize, &mut out[start..])
                .ok_or_else(|| "arena copy_to".to_string())?;
            off += n as u64;
            // Continue until off >= size; bank may be smaller than `want`.
        } else {
            let chunk =
                decode_read_resp(&resp.payload).ok_or_else(|| "READ decode".to_string())?;
            if chunk.is_empty() {
                break;
            }
            off += chunk.len() as u64;
            out.extend_from_slice(&chunk);
            if (chunk.len() as u32) < want {
                break;
            }
        }
    }
    let _ = client.submit(OP_CLOSE, 0, &encode_close_req(fh));
    Ok((size, out))
}

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

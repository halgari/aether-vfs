//! Control ring + bulk arena workers that serve a [`Director`] kernel to the shim.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use vfs_ipc::ring::{self, Geom};
use vfs_ipc::{Notifier, RingClient, RingServer, SpinNotifier};
use vfs_ipc::{DataArena, DEFAULT_PAYLOAD_CAP, DEFAULT_WORKER_COUNT};
use vfs_win::{EventNotifier, SharedMapping};

use crate::director::Director;
use crate::ops::RootId;
use crate::ring_dispatch::dispatch_director;

pub const DEFAULT_SLOT_COUNT: u32 = 32;
/// Re-export for callers; keep in sync with [`vfs_ipc::DEFAULT_ARENA_BYTES`].
pub const DEFAULT_ARENA_BYTES: usize = vfs_ipc::DEFAULT_ARENA_BYTES;

struct Inner {
    mapping: SharedMapping,
    kernel: Arc<Director>,
    payload_cap: u32,
    stop: AtomicBool,
    geom: Geom,
    arena_offset: usize,
    arena_len: usize,
    _events: EventNotifier,
    server_ev_name: String,
    client_ev_name: String,
}

/// Running IPC server bound to a director kernel (keeps workers alive).
///
/// This is the **production ring host** for remapped child I/O (not the legacy
/// `vfs_server::Server` tree path).
pub struct IpcServe {
    pub section_name: String,
    pub payload_cap: u32,
    /// Full shared mapping size (ring + arena). Used as `VFS_RING_BYTES` so the
    /// shim's `SharedMapping::open` maps the entire section.
    pub map_bytes: usize,
    /// Byte offset of the bulk arena (= control-ring length).
    pub arena_offset: usize,
    pub arena_len: usize,
    pub server_ev_name: String,
    pub client_ev_name: String,
    inner: Arc<Inner>,
    joins: Vec<JoinHandle<()>>,
}

impl IpcServe {
    pub fn start(kernel: Arc<Director>, section_name: String) -> Result<Self, String> {
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

        let arena_offset = ring_bytes;
        let inner = Arc::new(Inner {
            mapping,
            kernel,
            payload_cap,
            stop: AtomicBool::new(false),
            geom,
            arena_offset,
            arena_len,
            _events: events,
            server_ev_name: server_ev_name.clone(),
            client_ev_name: client_ev_name.clone(),
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
                if let Some(n) = notifier {
                    worker_loop(&inner2, n);
                } else {
                    worker_loop(&inner2, SpinNotifier);
                }
            }));
        }

        Ok(IpcServe {
            section_name,
            payload_cap,
            map_bytes: map_size,
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

    pub fn shared_seg(&self) -> &vfs_ipc::SharedSeg {
        self.inner.mapping.seg()
    }

    pub fn stop(mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        if let Ok(n) = EventNotifier::open(&self.server_ev_name, &self.client_ev_name) {
            n.notify_server();
            n.notify_client(0);
            drop(n);
        }
        for j in self.joins.drain(..) {
            let _ = j.join();
        }
    }

    pub fn write_thin_config(&self, path: &std::path::Path, root: &str) -> Result<(), String> {
        // `ring_bytes` key = full map size (historical name; shim maps this many bytes).
        let body = format!(
            "section={}\nroot={root}\npayload_cap={}\nring_bytes={}\n\
             arena_offset={}\narena_len={}\n\
             server_ev={}\nclient_ev={}\n",
            self.section_name,
            self.payload_cap,
            self.map_bytes,
            self.arena_offset,
            self.arena_len,
            self.server_ev_name,
            self.client_ev_name,
        );
        std::fs::write(path, body).map_err(|e| format!("write thin config: {e}"))
    }

    pub fn apply_env(&self, virtual_root: &str, thin_cfg: &std::path::Path) {
        // Process-global env is for the injected child (and single-session hosts).
        std::env::set_var(vfs_env::RING_SECTION, &self.section_name);
        std::env::set_var(vfs_env::RING_BYTES, self.map_bytes.to_string());
        std::env::set_var(vfs_env::RING_PAYLOAD_CAP, self.payload_cap.to_string());
        std::env::set_var(vfs_env::ARENA_OFFSET, self.arena_offset.to_string());
        std::env::set_var(vfs_env::ARENA_LEN, self.arena_len.to_string());
        std::env::set_var(vfs_env::SERVER_EV, &self.server_ev_name);
        std::env::set_var(vfs_env::CLIENT_EV, &self.client_ev_name);
        std::env::set_var(vfs_env::FUSE_CFG, thin_cfg.to_string_lossy().as_ref());
        std::env::set_var(vfs_env::VIRTUAL_DIR, virtual_root);
    }
}

fn worker_loop<N: vfs_ipc::Notifier>(inner: &Inner, notifier: N) {
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
        let handled = ring.serve_one(|req| {
            // The shim is single-root until stage 2b task 5; the ring wire
            // carries no root field to select otherwise (see the doc comment
            // on `dispatch_director`), so every request from an injected
            // child resolves against `RootId::DEFAULT`, unchanged from
            // before this stage.
            dispatch_director(
                &inner.kernel,
                RootId::DEFAULT,
                req.opcode,
                &req.payload,
                req.flags,
                inner.payload_cap,
                Some((&arena, req.slot)),
            )
        });
        match handled {
            Ok(true) => {}
            Ok(false) => {}
            Err(_) => break,
        }
    }
}

impl Drop for IpcServe {
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

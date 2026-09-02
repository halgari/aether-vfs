//! Control ring + bulk arena workers that serve a [`Director`] kernel to the shim.
//!
//! Two ways in, one serve loop. On Windows the ring lives in a **named,
//! page-file-backed section** and the two sides wake each other with event
//! objects ([`IpcServe::start`]); on Unix it lives in a **real file** that both
//! sides `mmap` by path, and both spin ([`IpcServe::start_file_backed`]) — see
//! that constructor for why a Wine-hosted shim leaves nothing to signal.
//! Everything between the mapping and `dispatch_director` is written once.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use vfs_ipc::ring::{self, Geom};
use vfs_ipc::{RingClient, RingServer, SpinNotifier};
use vfs_ipc::{DataArena, DEFAULT_WORKER_COUNT};
#[cfg(windows)]
use vfs_ipc::{Notifier, DEFAULT_PAYLOAD_CAP};
#[cfg(windows)]
use vfs_win::{EventNotifier, SharedMapping};

use crate::director::Director;

use crate::ring_dispatch::dispatch_director;

/// The ring's shared-memory backing, chosen by target.
///
/// Both types expose `seg()`, `len()` and `as_mut_ptr()` with identical
/// meaning — deliberately, so everything above this line is written once.
/// `SharedMapping` is a named page-file-backed section; `FileMapping` is an
/// `mmap` over a real file, which is what lets a shim inside Wine and a native
/// Linux Director share one ring.
#[cfg(windows)]
type RingMapping = vfs_win::SharedMapping;
#[cfg(unix)]
type RingMapping = vfs_unix::FileMapping;

pub const DEFAULT_SLOT_COUNT: u32 = 32;
/// Re-export for callers; keep in sync with [`vfs_ipc::DEFAULT_ARENA_BYTES`].
pub const DEFAULT_ARENA_BYTES: usize = vfs_ipc::DEFAULT_ARENA_BYTES;

struct Inner {
    mapping: RingMapping,
    kernel: Arc<Director>,
    payload_cap: u32,
    stop: AtomicBool,
    geom: Geom,
    arena_offset: usize,
    arena_len: usize,
    /// The event pair the named-section path wakes workers with. Windows-only,
    /// and behind `cfg` rather than in a second `Inner`: nothing else about the
    /// serve loop differs by target, so duplicating the struct would duplicate
    /// six portable fields to add one.
    #[cfg(windows)]
    _events: EventNotifier,
    #[cfg(windows)]
    server_ev_name: String,
    #[cfg(windows)]
    client_ev_name: String,
}

impl Inner {
    /// Nudge both ends so a worker asleep on an event notices [`Inner::stop`].
    ///
    /// On Unix this is deliberately empty: there are no event objects, every
    /// worker spins, and so storing `stop` is by itself enough to end the loop
    /// on its next turn.
    #[cfg(windows)]
    fn wake_for_stop(&self) {
        if let Ok(n) = EventNotifier::open(&self.server_ev_name, &self.client_ev_name) {
            n.notify_server();
            n.notify_client(0);
        }
    }

    #[cfg(not(windows))]
    fn wake_for_stop(&self) {}
}

/// Running IPC server bound to a director kernel (keeps workers alive).
///
/// This is the **production ring host** for remapped child I/O (not the legacy
/// `vfs_server::Server` tree path).
pub struct IpcServe {
    /// Windows-only, with the three event-name fields below: they name the
    /// *named-section* handshake, which has no counterpart in the file-backed
    /// mode — there the ring is identified by [`Self::ring_path`] instead.
    /// Gated rather than left as empty strings so a file-backed caller cannot
    /// silently publish `VFS_RING_SECTION=""` and be believed.
    #[cfg(windows)]
    pub section_name: String,
    pub payload_cap: u32,
    /// Full shared mapping size (ring + arena). Used as `VFS_RING_BYTES` so the
    /// shim's `SharedMapping::open` maps the entire section, and as the length a
    /// file-backed client passes to `FileMapping::open`.
    pub map_bytes: usize,
    /// Byte offset of the bulk arena (= control-ring length).
    pub arena_offset: usize,
    pub arena_len: usize,
    #[cfg(windows)]
    pub server_ev_name: String,
    #[cfg(windows)]
    pub client_ev_name: String,
    /// The ring file, when this server was started file-backed; `None` for the
    /// named-section path, which has no path to report.
    ring_path: Option<std::path::PathBuf>,
    inner: Arc<Inner>,
    joins: Vec<JoinHandle<()>>,
}

impl IpcServe {
    #[cfg(windows)]
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
            ring_path: None,
            inner,
            joins,
        })
    }

    /// Serve this kernel over a ring held in the **file** at `ring_path`, with
    /// no OS event objects anywhere.
    ///
    /// This is the Wine-hosted path. A Windows shim running under Proton maps
    /// the same file with `CreateFileMappingW` over a real handle, so the two
    /// processes agree by *path* — a named, page-file-backed section has no
    /// identity a native Linux process could open, which is why [`Self::start`]
    /// is unusable here.
    ///
    /// Every worker gets a [`SpinNotifier`], and that is not a simplification
    /// to be tidied up later. The shim's `WakeServerSpinClient::notify_server`
    /// calls `SetEvent` on a **Wine** event object, which cannot wake a native
    /// Linux process; under Wine that call therefore signals nothing. A
    /// Director that slept would be woken only by the 15.6 ms timer tick — the
    /// stall measured on 2026-08-12, where 16 of 231 `NtQueryFullAttributesFile`
    /// calls waited that long and owned ~93% of that hook's total time. Spinning
    /// costs CPU and sees each request immediately; that trade is accepted for
    /// this increment, and a shared-memory futex is the eventual answer.
    ///
    /// The geometry arithmetic below is [`Self::start`]'s, copied rather than
    /// factored differently, because a client computes the same numbers.
    #[cfg(unix)]
    pub fn start_file_backed(
        kernel: Arc<Director>,
        ring_path: &std::path::Path,
        payload_cap: u32,
    ) -> Result<Self, String> {
        let slot_count = DEFAULT_SLOT_COUNT;
        let stride = ((32 + payload_cap as usize) + 7) & !7;
        let ring_bytes = 40 + slot_count as usize * stride;
        let arena_len = DEFAULT_ARENA_BYTES;
        let map_size = ((ring_bytes + arena_len + 0xFFFF) & !0xFFFF).max(2 * 1024 * 1024);

        // Grow-only, and sized before mapping: `mmap` past EOF succeeds and
        // then SIGBUSes on first touch. See `FileMapping::create`'s docs.
        let mapping = RingMapping::create(ring_path, map_size)
            .map_err(|e| format!("create ring file {}: {e}", ring_path.display()))?;
        let geom = ring::init(mapping.seg(), slot_count, payload_cap)
            .map_err(|e| format!("ring init: {e:?}"))?;

        let arena_offset = ring_bytes;
        let inner = Arc::new(Inner {
            mapping,
            kernel,
            payload_cap,
            stop: AtomicBool::new(false),
            geom,
            arena_offset,
            arena_len,
        });

        let workers = DEFAULT_WORKER_COUNT.max(1);
        let mut joins = Vec::with_capacity(workers);
        for _ in 0..workers {
            let inner2 = inner.clone();
            joins.push(thread::spawn(move || worker_loop(&inner2, SpinNotifier)));
        }

        Ok(IpcServe {
            payload_cap,
            map_bytes: map_size,
            arena_offset,
            arena_len,
            ring_path: Some(ring_path.to_path_buf()),
            inner,
            joins,
        })
    }

    /// The ring file this server is serving over, or `None` when it was started
    /// on a named section instead.
    pub fn ring_path(&self) -> Option<&std::path::Path> {
        self.ring_path.as_deref()
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
        self.inner.wake_for_stop();
        for j in self.joins.drain(..) {
            let _ = j.join();
        }
    }

    /// Windows-only: this file names the section and the event pair, which is
    /// the named-section handshake. The file-backed mode publishes
    /// [`vfs_env::RING_PATH`] instead and has no events to name.
    #[cfg(windows)]
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

    /// Windows-only, with [`Self::apply_env_roots`]: it publishes the section
    /// name and both event names, none of which exist in the file-backed mode.
    #[cfg(windows)]
    pub fn apply_env(&self, virtual_root: &str, thin_cfg: &std::path::Path) {
        self.apply_env_roots(virtual_root, &[], thin_cfg)
    }

    /// [`Self::apply_env`] for a session that virtualizes more than one root.
    ///
    /// `extra_roots` is `(id, path)` for every root **beyond root 0**, which
    /// `virtual_root` names. The shim needs the full set because the root id
    /// is what its ring requests now carry: a root it has never been told
    /// about is one whose paths it classifies as belonging to no one and lets
    /// fall through to real disk, silently.
    #[cfg(windows)]
    pub fn apply_env_roots(
        &self,
        virtual_root: &str,
        extra_roots: &[(u32, String)],
        thin_cfg: &std::path::Path,
    ) {
        // Process-global env is for the injected child (and single-session hosts).
        std::env::set_var(vfs_env::RING_SECTION, &self.section_name);
        // Cleared for the same reason `VIRTUAL_ROOTS` is below, and it is the
        // more dangerous of the two: `VFS_RING_PATH` **wins** over
        // `VFS_RING_SECTION` in the shim (see `fuse_client::ring_source`), so a
        // stale value left by an earlier file-backed session in this process
        // would send the child to that old ring file — attaching it to a
        // director that is gone, or to a stale ring another one is still
        // serving — while this session's brand-new section sat unused and every
        // log said the launch was configured correctly.
        std::env::remove_var(vfs_env::RING_PATH);
        std::env::set_var(vfs_env::RING_BYTES, self.map_bytes.to_string());
        std::env::set_var(vfs_env::RING_PAYLOAD_CAP, self.payload_cap.to_string());
        std::env::set_var(vfs_env::ARENA_OFFSET, self.arena_offset.to_string());
        std::env::set_var(vfs_env::ARENA_LEN, self.arena_len.to_string());
        std::env::set_var(vfs_env::SERVER_EV, &self.server_ev_name);
        std::env::set_var(vfs_env::CLIENT_EV, &self.client_ev_name);
        std::env::set_var(vfs_env::FUSE_CFG, thin_cfg.to_string_lossy().as_ref());
        std::env::set_var(vfs_env::VIRTUAL_DIR, virtual_root);
        if extra_roots.is_empty() {
            // Cleared, not left alone: a previous single-session host in this
            // process may have set it, and inheriting a stale second root is
            // exactly the "declared root that is not there" failure this var
            // exists to prevent.
            std::env::remove_var(vfs_env::VIRTUAL_ROOTS);
        } else {
            let spec = extra_roots
                .iter()
                .map(|(id, path)| format!("{id}={path}"))
                .collect::<Vec<_>>()
                .join(";");
            std::env::set_var(vfs_env::VIRTUAL_ROOTS, spec);
        }
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
            // Stage 2b task 5: the root travels in the payload, so this loop
            // no longer pins every request from an injected child to
            // `RootId::DEFAULT` — the shim says which root it meant and
            // `dispatch_director` routes on it.
            dispatch_director(
                &inner.kernel,
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
        self.inner.wake_for_stop();
        for j in self.joins.drain(..) {
            let _ = j.join();
        }
    }
}

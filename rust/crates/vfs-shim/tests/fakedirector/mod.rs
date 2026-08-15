//! An in-process stand-in for the director's ring server.
//!
//! `Engine::cow_seed` reads copy-up content through the director, so a test
//! that wants to know *where the bytes came from* has to be able to put bytes
//! somewhere only the director can see. There is no way to fake that with
//! files: the whole claim under test is that content on the real filesystem
//! under a managed root is unreachable, so any fixture built out of real files
//! under the root is a fixture the shim is supposed to ignore.
//!
//! So this serves a real ring — the same `vfs_ipc` `RingServer` the director
//! runs, over a real named section — from a table held in memory. The shim's
//! own `FuseClient` connects to it through `try_init_from_env` exactly as it
//! does in production, so the code path under test is the production one; only
//! the far side of the ring is a fake.
//!
//! It answers only what copy-up uses (HEARTBEAT, GETATTR, OPEN, READ, CLOSE)
//! and deliberately gives a test control over *how* a READ is answered
//! ([`ReadStyle`]), because the read loop's correctness is mostly about what
//! it does with awkward answers: a short read that is not EOF, a read that
//! fails part-way, a file that turns out shorter than OPEN promised.
//!
//! **Both transports.** A READ is answered inline (data in the ring payload)
//! or in **bulk** (data written into the shared arena, ring carries only
//! length + offset), chosen exactly the way `dispatch_director` chooses: the
//! client's `FLAG_READ_BULK`, or a request at or above `BULK_THRESHOLD`. This
//! is not decoration. Live, `arena_len > 0` and copy-up's `SEED_CHUNK` is
//! 256 KiB — four times the threshold — so **every real copy-up of a large
//! file takes the bulk path**. A fixture with no arena tests fragment
//! reassembly only on the transport production does not use, and silent
//! truncation of a large file is the worst failure available here.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use vfs_ipc::{DataArena, RingServer, SpinNotifier};
use vfs_protocol as P;
use vfs_win::SharedMapping;

/// Deliberately small — 4 KiB caps an inline READ response at 4088 bytes, so
/// any fixture bigger than that is guaranteed to span several ring round
/// trips. A test that only ever moved one payload's worth of data would pass
/// against an implementation that reads once and calls it done.
pub const PAYLOAD_CAP: u32 = 4096;
pub const SLOTS: u32 = 8;

/// Requests at or above this go bulk, matching `dispatch_director`'s and
/// `FuseClient::read_fragmented`'s own constant. A fixture below it stays
/// inline whatever the arena is, which is how one ring covers both transports.
pub const BULK_THRESHOLD: u32 = 64 * 1024;

/// An arena of `SLOTS` × 256 KiB. The bank size the client computes
/// (`arena_len / slot_count`, clamped to at least 256 KiB) then agrees exactly
/// with the one the server hands out, so a bulk read is not silently truncated
/// to a smaller bank and resumed — which would still pass a byte-exactness
/// test while hiding whether the bank sizing was right.
pub const ARENA_LEN: usize = SLOTS as usize * 256 * 1024;

/// Control ring length, and therefore the arena's offset within the section —
/// same layout `IpcServe::start` uses (`arena_offset = ring_bytes`).
fn ring_bytes() -> usize {
    let stride = (32 + PAYLOAD_CAP as usize).next_multiple_of(8);
    40 + SLOTS as usize * stride
}

/// How the fake answers READ for one file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadStyle {
    /// Serve as much as was asked for (up to EOF) — an ordinary provider.
    Whole,
    /// Never serve more than `n` bytes per READ, however much was asked for
    /// and however far from EOF. A real provider does this whenever its own
    /// backing read comes back partial; it is *not* a signal of EOF, and a
    /// reader that treats it as one silently truncates.
    Short(usize),
    /// OPEN reports the real size; every READ fails with `ST_IO_ERROR`.
    Error,
    /// OPEN reports the real size but the file stops at `n` bytes — the
    /// director having less than it said it had.
    ShorterThanClaimed(u64),
}

struct Entry {
    bytes: Vec<u8>,
    style: ReadStyle,
}

/// Per-vpath request tallies. Keyed by vpath rather than kept as a single
/// running total because the tests in a binary run in parallel against one
/// fake: a global counter can only be asserted on as "at least", which is not
/// an assertion at all for "every handle was closed". A vpath used by one test
/// gives that test a private, exact count.
#[derive(Default)]
pub struct Tally {
    opened: Mutex<HashMap<String, u64>>,
    closed: Mutex<HashMap<String, u64>>,
    reads: Mutex<HashMap<String, u64>>,
    bulk_reads: Mutex<HashMap<String, u64>>,
}

impl Tally {
    fn bump(m: &Mutex<HashMap<String, u64>>, vpath: &str) {
        *m.lock().unwrap().entry(vpath.to_string()).or_insert(0) += 1;
    }
    fn get(m: &Mutex<HashMap<String, u64>>, vpath: &str) -> u64 {
        m.lock().unwrap().get(vpath).copied().unwrap_or(0)
    }
    /// OPENs that issued a handle (a not-found OPEN issues none).
    pub fn opens(&self, vpath: &str) -> u64 {
        Self::get(&self.opened, vpath)
    }
    pub fn closes(&self, vpath: &str) -> u64 {
        Self::get(&self.closed, vpath)
    }
    /// READ requests that reached the server for this file — the ring round
    /// trips a copy-up of it actually cost.
    pub fn reads(&self, vpath: &str) -> u64 {
        Self::get(&self.reads, vpath)
    }
    /// Of those, the ones answered through the shared **arena** rather than
    /// inline. This is what says a test covered the transport a real copy-up
    /// of a large file uses, rather than only the small-file one.
    pub fn bulk_reads(&self, vpath: &str) -> u64 {
        Self::get(&self.bulk_reads, vpath)
    }
}

pub struct Fake {
    files: HashMap<String, Entry>,
    handles: Mutex<HashMap<u64, String>>,
    next_fh: AtomicU64,
    refuse_writes: bool,
    pub tally: Tally,
}

impl Fake {
    pub fn new() -> Fake {
        Fake {
            files: HashMap::new(),
            handles: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            refuse_writes: false,
            tally: Tally::default(),
        }
    }

    /// Answer `OPEN_WRITE` with `ST_READ_ONLY`, the way a read-only provider
    /// does. This is what puts the shim on its write-fallback path
    /// (`try_fuse_create`'s "Director rejects OPEN_WRITE … fall through so
    /// write/create under the root hits the overlay redirect path"), which is
    /// the only route by which a hooked process reaches `Engine::cow_seed` at
    /// all. A fake that accepted writes would serve the whole open through the
    /// director and never exercise copy-up.
    pub fn read_only(mut self) -> Fake {
        self.refuse_writes = true;
        self
    }

    /// Add a file to the provider graph. `vpath` is the folded, `/`-joined
    /// remainder the shim builds from the path (`Data\A.esp` -> `data/a.esp`).
    pub fn with(mut self, vpath: &str, bytes: Vec<u8>, style: ReadStyle) -> Fake {
        self.files.insert(vpath.to_string(), Entry { bytes, style });
        self
    }

    /// `arena` is `(arena, slot)` for this request, mirroring
    /// `dispatch_director`'s own parameter — `None` only when the fixture was
    /// installed with no arena at all.
    fn handle(
        &self,
        opcode: u32,
        flags: u32,
        payload: &[u8],
        arena: Option<(&DataArena<'_>, u32)>,
    ) -> (i32, Vec<u8>) {
        match opcode {
            P::OP_HEARTBEAT => (P::ST_OK, Vec::new()),
            P::OP_GETATTR => {
                let Some((_root, vpath)) = P::decode_path_req(payload) else {
                    return (P::ST_BAD_REQUEST, Vec::new());
                };
                let found = self.files.get(&vpath);
                (
                    P::ST_OK,
                    P::encode_getattr_resp(&P::AttrResp {
                        found: found.is_some(),
                        is_dir: false,
                        size: found.map(|e| e.bytes.len() as u64).unwrap_or(0),
                        mtime: 0,
                    }),
                )
            }
            P::OP_OPEN => {
                let Some((_root, flags, vpath)) = P::decode_open_req(payload) else {
                    return (P::ST_BAD_REQUEST, Vec::new());
                };
                if self.refuse_writes && flags & P::OPEN_WRITE != 0 {
                    return (P::ST_READ_ONLY, Vec::new());
                }
                let Some(e) = self.files.get(&vpath) else {
                    return (P::ST_NOT_FOUND, Vec::new());
                };
                let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
                Tally::bump(&self.tally.opened, &vpath);
                self.handles.lock().unwrap().insert(fh, vpath);
                (
                    P::ST_OK,
                    P::encode_open_resp(&P::OpenResp {
                        fh,
                        size: e.bytes.len() as u64,
                        is_dir: false,
                    }),
                )
            }
            P::OP_READ => {
                let Some(req) = P::decode_read_req(payload) else {
                    return (P::ST_BAD_REQUEST, Vec::new());
                };
                let vpath = match self.handles.lock().unwrap().get(&req.fh) {
                    Some(v) => v.clone(),
                    None => return (P::ST_BAD_REQUEST, Vec::new()),
                };
                Tally::bump(&self.tally.reads, &vpath);
                let e = &self.files[&vpath];
                let limit = match e.style {
                    ReadStyle::Error => return (P::ST_IO_ERROR, Vec::new()),
                    ReadStyle::Whole => e.bytes.len() as u64,
                    ReadStyle::Short(_) => e.bytes.len() as u64,
                    ReadStyle::ShorterThanClaimed(n) => n,
                };
                let start = req.offset.min(limit) as usize;
                let mut want = (limit as usize - start).min(req.len as usize);
                if let ReadStyle::Short(n) = e.style {
                    want = want.min(n);
                }
                let src = &e.bytes[start..start + want];
                // Bulk chosen exactly as `dispatch_director` chooses it, so a
                // fixture inherits production's transport rather than the
                // fake's opinion of it.
                let want_bulk = (flags & P::FLAG_READ_BULK) != 0 || req.len >= BULK_THRESHOLD;
                if want_bulk {
                    if let Some((arena, slot)) = arena {
                        Tally::bump(&self.tally.bulk_reads, &vpath);
                        let max = arena.bank_size.min(src.len());
                        return match arena.fill_bank(slot, max, |buf| {
                            let n = buf.len().min(src.len());
                            buf[..n].copy_from_slice(&src[..n]);
                            Ok(n)
                        }) {
                            Ok((off, n)) => (P::ST_OK, P::encode_read_resp_bulk(n as u32, off)),
                            Err(st) => (st, Vec::new()),
                        };
                    }
                }
                (P::ST_OK, P::encode_read_resp(src))
            }
            P::OP_CLOSE => {
                if let Some(fh) = P::decode_close_req(payload) {
                    if let Some(vpath) = self.handles.lock().unwrap().remove(&fh) {
                        Tally::bump(&self.tally.closed, &vpath);
                    }
                }
                (P::ST_OK, Vec::new())
            }
            // Nothing else is part of copy-up. Answering "not supported"
            // rather than silently succeeding keeps an unexpected opcode from
            // looking like a healthy exchange.
            _ => (P::ST_NOT_SUPPORTED, Vec::new()),
        }
    }
}

/// Publish `fake` on a real named section and point the shim's `FuseClient` at
/// it, once per test process. Returns the fake so tests can read its counters.
///
/// The section, the server thread and the fake are all leaked deliberately:
/// they must outlive every test in the binary, and the process is the only
/// thing that can decide when that is.
///
/// `virtual_dir` becomes `VFS_VIRTUAL_DIR` — root 0 for both halves of the
/// shim, so the `RootId` the `Engine` resolves is the one the client sends.
///
/// `arena_len` of 0 forces every READ inline; [`ARENA_LEN`] gives the section
/// a real bulk arena laid out the way `IpcServe::start` lays one out, so a
/// request at or above [`BULK_THRESHOLD`] takes the same transport it takes
/// live. Below the threshold reads stay inline either way, so one ring can
/// cover both.
pub fn install(virtual_dir: &std::path::Path, fake: Fake, arena_len: usize) -> &'static Fake {
    static FAKE: OnceLock<&'static Fake> = OnceLock::new();
    FAKE.get_or_init(|| {
        let fake: &'static Fake = Box::leak(Box::new(fake));
        let name = format!("Local\\vfs-shim-cowseed-{}", std::process::id());
        let arena_offset = ring_bytes();
        // Whole section, ring first then arena — `VFS_RING_BYTES` names the
        // whole thing because the shim maps all of it and a bulk response's
        // offset is section-absolute.
        let map_bytes = ((arena_offset + arena_len + 0xFFFF) & !0xFFFF).max(256 * 1024);
        let mapping: &'static SharedMapping =
            Box::leak(Box::new(SharedMapping::create(&name, map_bytes).expect("section")));
        vfs_ipc::ring::init(mapping.seg(), SLOTS, PAYLOAD_CAP).expect("ring init");

        std::thread::Builder::new()
            .name("fake-director".into())
            .spawn(move || {
                let server = RingServer::new(mapping.seg(), SpinNotifier).expect("ring open");
                // One arena for the life of the thread, banked per slot —
                // `worker_loop`'s shape. `banks == slot_count` is what makes
                // the client's `arena_len / slot_count` bank size agree with
                // the server's.
                let arena = (arena_len > 0).then(|| {
                    DataArena::new(mapping.seg(), arena_offset, arena_len, SLOTS as usize)
                });
                // Runs until the ring goes away with the process. `serve_one`
                // answers at most one request and returns `Ok(false)` when
                // idle, so this is a spin — fine for a fixture whose whole
                // life is a handful of copy-ups.
                while server
                    .serve_one(|req| {
                        fake.handle(
                            req.opcode,
                            req.flags,
                            &req.payload,
                            arena.as_ref().map(|a| (a, req.slot)),
                        )
                    })
                    .is_ok()
                {}
            })
            .expect("server thread");

        std::env::set_var(vfs_env::RING_SECTION, &name);
        std::env::set_var(vfs_env::RING_BYTES, map_bytes.to_string());
        std::env::set_var(vfs_env::RING_PAYLOAD_CAP, PAYLOAD_CAP.to_string());
        std::env::set_var(vfs_env::ARENA_OFFSET, arena_offset.to_string());
        std::env::set_var(vfs_env::ARENA_LEN, arena_len.to_string());
        std::env::set_var(vfs_env::VIRTUAL_DIR, virtual_dir);
        vfs_shim::fuse_client::try_init_from_env().expect("fuse client");
        fake
    })
}

/// A byte pattern no accidental fill can imitate, and whose every position is
/// distinguishable — a copy that drops, duplicates or reorders a fragment
/// fails a full comparison against it rather than matching by luck.
pub fn pattern(len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut x: u32 = 0x9E37_79B9;
    for i in 0..len {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        v.push((x >> 24) as u8 ^ (i as u8));
    }
    v
}

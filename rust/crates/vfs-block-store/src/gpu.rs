//! GPU batch compression for bulk writes (feature `gpu-zstd`).
//!
//! A [`GpuEncoder`] is one service per store. Writers hand it the new blocks of a write and wait;
//! one batcher thread collects the blocks of every concurrent writer into GPU batches (a batch
//! goes when it is full, or once its oldest block has waited [`GpuConfig::batch_deadline`]),
//! keeps several batches in flight on the GPU (`gzc-gpu`'s stream), and hands every writer the
//! zstd frames of its blocks. Each frame is one standard zstd frame of one block, which the
//! store's decode path reads like any frame libzstd writes.
//!
//! - **Memory** is bounded: blocks queued or on the GPU take at most
//!   [`GpuConfig::max_inflight_bytes`]; a writer that would go over waits (backpressure).
//! - **VRAM** is taken only while bulk writes arrive: the GPU compressor is built at the first
//!   block and dropped after [`GpuConfig::idle_timeout`] without one.
//! - **Failure** never loses a write: if the GPU cannot be opened, a batch fails, the batcher
//!   panics or a batch stalls past [`GpuConfig::stall_timeout`], the encoder turns itself off
//!   for good (logged once) and every block it has not delivered, and every later one, is
//!   compressed by the writer on the CPU.
//! - **Shutdown** ([`GpuEncoder::shutdown`], at store close) compresses what is queued, then
//!   stops the thread.
//!
//! The GPU itself sits behind [`Engine`], so the batching, backpressure, fallback and shutdown
//! logic is tested without one.

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The most bytes a GPU block may hold (`gzc-gpu` compresses blocks of at most 64 KiB).
pub const GPU_MAX_BLOCK: usize = 64 * 1024;

/// A GPU compression preset of `gzc-gpu`, named as it names them. Each matches (or beats) the
/// ratio of the libzstd level in its description, on 64 KiB blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GpuLevel {
    /// `lvl3`: greedy parse, ratio of zstd -3.
    Lvl3,
    /// `lvl9s12seg`: lazy parse, ratio of zstd -9.
    Lvl9s12seg,
    /// `opt14`: optimal parse, ratio of zstd -14.
    Opt14,
    /// `opt16p1`: optimal parse in one pass, ratio of zstd -16.
    Opt16p1,
}

impl GpuLevel {
    /// Every preset, fastest first.
    pub const ALL: [GpuLevel; 4] = [
        GpuLevel::Lvl3,
        GpuLevel::Lvl9s12seg,
        GpuLevel::Opt14,
        GpuLevel::Opt16p1,
    ];

    /// The preset's name: `lvl3`, `lvl9s12seg`, `opt14` or `opt16p1`.
    pub fn name(self) -> &'static str {
        match self {
            GpuLevel::Lvl3 => "lvl3",
            GpuLevel::Lvl9s12seg => "lvl9s12seg",
            GpuLevel::Opt14 => "opt14",
            GpuLevel::Opt16p1 => "opt16p1",
        }
    }

    /// The preset called `name` (exactly as [`GpuLevel::name`] spells it).
    pub fn from_name(name: &str) -> Option<GpuLevel> {
        GpuLevel::ALL.into_iter().find(|l| l.name() == name)
    }

    fn level(self) -> gzc_gpu::Level {
        match self {
            GpuLevel::Lvl3 => gzc_gpu::Level::Zstd3,
            GpuLevel::Lvl9s12seg => gzc_gpu::Level::Zstd9,
            GpuLevel::Opt14 => gzc_gpu::Level::Zstd14,
            GpuLevel::Opt16p1 => gzc_gpu::Level::Zstd16,
        }
    }
}

/// How bulk writes are compressed on the GPU ([`crate::BulkCompression::Gpu`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuConfig {
    pub level: GpuLevel,
    /// GPU memory the compressor may take, MiB. It sizes the batch (`gzc-gpu` takes the largest
    /// batch that fits). Taken while bulk writes run, given back after `idle_timeout`.
    pub vram_budget_mib: u64,
    /// When the GPU has room for a batch, the longest the oldest queued block waits for more
    /// to join it before the batch goes as it is.
    pub batch_deadline: Duration,
    /// Batches on the GPU at once. With this many submitted and not yet delivered, the next
    /// batch keeps filling (up to the GPU's batch size) until one is delivered: under load,
    /// batches grow to match it, and the GPU's fixed cost per batch is paid less often.
    pub gpu_depth: usize,
    /// Bytes of blocks queued or on the GPU, at most; a writer that would exceed it waits.
    pub max_inflight_bytes: usize,
    /// The GPU compressor (and its VRAM) is dropped after this long without a block, and built
    /// again by the next one.
    pub idle_timeout: Duration,
    /// A writer that waits this long for its blocks without any batch completing gives up on the
    /// GPU: it compresses its blocks on the CPU and turns the GPU off for the store's lifetime.
    pub stall_timeout: Duration,
    /// Decode every frame the GPU returns and compare it with its block before storing it. A
    /// mismatch stores the block CPU-compressed and turns the GPU off. On by default on
    /// Windows, where `gzc-gpu` documents bad output from DX12.
    pub verify: bool,
}

impl Default for GpuConfig {
    fn default() -> Self {
        GpuConfig {
            level: GpuLevel::Opt16p1,
            vram_budget_mib: 4096,
            batch_deadline: Duration::from_millis(2),
            gpu_depth: 1,
            max_inflight_bytes: 1 << 30,
            idle_timeout: Duration::from_secs(5),
            stall_timeout: Duration::from_secs(120),
            verify: cfg!(windows),
        }
    }
}

impl GpuConfig {
    pub(crate) fn validate(&self, block_size: u32) -> Result<(), String> {
        if block_size as usize > GPU_MAX_BLOCK {
            return Err(format!(
                "GPU compression needs blocks of at most {GPU_MAX_BLOCK} bytes (block_size is {block_size})"
            ));
        }
        if self.vram_budget_mib == 0 {
            return Err("gpu vram_budget_mib must be above 0".into());
        }
        if self.max_inflight_bytes < GPU_MAX_BLOCK {
            return Err("gpu max_inflight_bytes must hold at least one block".into());
        }
        if self.gpu_depth == 0 {
            return Err("gpu_depth must be at least 1".into());
        }
        if self.stall_timeout.is_zero() || self.idle_timeout.is_zero() {
            return Err("gpu idle_timeout and stall_timeout must be above 0".into());
        }
        Ok(())
    }
}

/// What the GPU encoder has done since the store opened.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GpuStats {
    /// The preset's name.
    pub preset: &'static str,
    /// Batches the GPU compressed.
    pub batches: u64,
    /// Blocks the GPU compressed.
    pub blocks: u64,
    /// Bulk blocks compressed on the CPU instead (the GPU unavailable, failed or shut down).
    pub cpu_fallback_blocks: u64,
    /// Times the GPU compressor was built (once, plus once after each idle drop).
    pub opens: u64,
    /// Why the GPU was turned off, if it was.
    pub failed: Option<String>,
    /// The adapter, as the last compressor described it.
    pub adapter: Option<String>,
}

/// One finished batch: frame `k` is the frame of the batch's block `k`.
pub(crate) trait FrameSource {
    fn len(&self) -> usize;
    fn frame(&self, k: usize) -> &[u8];
}

/// Takes the blocks of the next batch, at most the given capacity; `None` ends the run.
pub(crate) type Fill<'a> = dyn FnMut(usize) -> Option<Vec<Vec<u8>>> + 'a;
/// Receives each finished batch, in submission order, possibly on another thread.
pub(crate) type Deliver<'a> = dyn FnMut(&dyn FrameSource) + Send + 'a;

/// A GPU compressor, or a stand-in for one in tests.
pub(crate) trait Engine: Send {
    /// One line naming the device.
    fn describe(&self) -> String;
    /// Compresses batches from `fill` until it returns `None`, each finished batch to `deliver`
    /// in order; returns once every submitted batch was delivered.
    fn run(&self, fill: &mut Fill<'_>, deliver: &mut Deliver<'_>) -> Result<(), String>;
}

/// Builds an engine (opens the GPU).
pub(crate) type EngineFactory =
    Box<dyn Fn(&GpuConfig) -> Result<Box<dyn Engine>, String> + Send + Sync>;

/// The real engine: a `gzc_gpu::Compressor`.
struct GzcEngine {
    c: gzc_gpu::Compressor,
}

impl FrameSource for gzc_gpu::FrameBatch {
    fn len(&self) -> usize {
        gzc_gpu::FrameBatch::len(self)
    }
    fn frame(&self, k: usize) -> &[u8] {
        gzc_gpu::FrameBatch::frame(self, k)
    }
}

impl Engine for GzcEngine {
    fn describe(&self) -> String {
        format!(
            "{} (batch {} blocks)",
            self.c.describe(),
            self.c.batch_blocks()
        )
    }

    fn run(&self, fill: &mut Fill<'_>, deliver: &mut Deliver<'_>) -> Result<(), String> {
        use rayon::prelude::*;
        self.c
            .stream(
                |batch| {
                    deliver(&batch);
                    Ok(())
                },
                |stream| loop {
                    // Take the GPU slot first: blocks keep arriving while every slot is busy,
                    // and all of them go into this batch.
                    let mut batch = stream.next_batch()?;
                    let Some(blocks) = fill(batch.capacity()) else {
                        return Ok(());
                    };
                    let lens: Vec<usize> = blocks.iter().map(Vec::len).collect();
                    let mut payloads = batch.reserve(&lens)?;
                    payloads
                        .par_iter_mut()
                        .zip(blocks.par_iter())
                        .for_each(|(p, b)| p.write(b));
                    drop(payloads);
                    batch.submit()?;
                },
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Opens the default GPU for `cfg`.
pub(crate) fn gzc_factory() -> EngineFactory {
    Box::new(|cfg: &GpuConfig| {
        let options = gzc_gpu::CompressorOptions {
            vram_budget_mib: cfg.vram_budget_mib,
            ..gzc_gpu::CompressorOptions::new(cfg.level.level())
        };
        let c = gzc_gpu::Compressor::with_options(options).map_err(|e| e.to_string())?;
        Ok(Box::new(GzcEngine { c }) as Box<dyn Engine>)
    })
}

/// One writer call's blocks and their results.
struct Request {
    state: Mutex<RequestState>,
    done: Condvar,
}

struct RequestState {
    /// Per block: `None` while pending; `Some(None)` compress it on the CPU; `Some(Some(f))` its
    /// GPU frame.
    out: Vec<Option<Option<Vec<u8>>>>,
    left: usize,
}

impl Request {
    /// Sets block `i`'s result unless it has one (a writer that gave up on a stalled GPU has
    /// already settled its blocks).
    fn resolve(&self, i: usize, r: Option<Vec<u8>>) {
        let mut st = lock(&self.state);
        if st.out[i].is_none() {
            st.out[i] = Some(r);
            st.left -= 1;
            if st.left == 0 {
                self.done.notify_all();
            }
        }
    }
}

/// Where one block's result goes.
struct Slot {
    req: Arc<Request>,
    i: usize,
    len: usize,
}

struct Item {
    data: Vec<u8>,
    slot: Slot,
    at: Instant,
}

struct State {
    queue: VecDeque<Item>,
    /// Bytes of blocks queued or submitted and not yet delivered.
    bytes: usize,
    shutdown: bool,
    failed: Option<String>,
}

struct Shared {
    cfg: GpuConfig,
    state: Mutex<State>,
    /// Signalled when blocks are queued or the encoder shuts down or fails.
    work: Condvar,
    /// Signalled when bytes are freed or the encoder shuts down or fails.
    space: Condvar,
    /// Submitted batches' slots, in submission order.
    inflight: Mutex<VecDeque<Vec<Slot>>>,
    /// Bumped by every delivered batch: a writer's stall clock restarts on progress.
    progress: AtomicU64,
    batches: AtomicU64,
    blocks: AtomicU64,
    fallback: AtomicU64,
    opens: AtomicU64,
    adapter: Mutex<Option<String>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    /// Turns the GPU off for good (logged the first time), and settles every block it holds,
    /// queued or on the GPU, as "compress on the CPU".
    fn fail(&self, why: String) {
        let queued: Vec<Item> = {
            let mut st = lock(&self.state);
            if st.failed.is_none() {
                tracing::warn!(
                    reason = %why,
                    "GPU zstd compression is off for this store; bulk writes are compressed on the CPU"
                );
                st.failed = Some(why);
            }
            st.queue.drain(..).collect()
        };
        let inflight: Vec<Vec<Slot>> = lock(&self.inflight).drain(..).collect();
        let mut freed = 0;
        for slot in queued
            .into_iter()
            .map(|it| it.slot)
            .chain(inflight.into_iter().flatten())
        {
            freed += slot.len;
            slot.req.resolve(slot.i, None);
        }
        let mut st = lock(&self.state);
        st.bytes = st.bytes.saturating_sub(freed);
        drop(st);
        self.space.notify_all();
        self.work.notify_all();
    }

    /// The next batch's blocks, at most `cap`: waits until `cap` blocks are queued, or the
    /// oldest has waited `batch_deadline`, or (with nothing queued) `idle_timeout` passes
    /// (`None`, and `idle` set) or the encoder shuts down or fails (`None`).
    fn fill(&self, cap: usize, idle: &mut bool) -> Option<Vec<Vec<u8>>> {
        let cap = cap.max(1);
        let mut st = lock(&self.state);
        loop {
            if st.failed.is_some() {
                return None;
            }
            if st.queue.is_empty() {
                if st.shutdown {
                    return None;
                }
                let (g, t) = self
                    .work
                    .wait_timeout(st, self.cfg.idle_timeout)
                    .unwrap_or_else(|e| e.into_inner());
                st = g;
                if t.timed_out() && st.queue.is_empty() && !st.shutdown {
                    *idle = true;
                    return None;
                }
                continue;
            }
            if st.queue.len() >= cap || st.shutdown {
                break;
            }
            // While `gpu_depth` batches are on the GPU, the next one keeps growing: it could not
            // start sooner anyway, and a fuller batch costs the GPU less per block. A delivery
            // wakes this wait.
            let busy = lock(&self.inflight).len() >= self.cfg.gpu_depth;
            let age = st.queue[0].at.elapsed();
            if !busy && age >= self.cfg.batch_deadline {
                break;
            }
            let wait = if busy {
                Duration::from_millis(50)
            } else {
                self.cfg.batch_deadline - age
            };
            st = self
                .work
                .wait_timeout(st, wait)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        let n = st.queue.len().min(cap);
        let items: Vec<Item> = st.queue.drain(..n).collect();
        // Recorded before the engine sees the blocks, under the state lock, so batches are in
        // `inflight` in the order they are submitted.
        let (data, slots): (Vec<Vec<u8>>, Vec<Slot>) =
            items.into_iter().map(|it| (it.data, it.slot)).unzip();
        lock(&self.inflight).push_back(slots);
        Some(data)
    }

    /// Hands a finished batch's frames to their writers.
    fn deliver(&self, frames: &dyn FrameSource) {
        let Some(slots) = lock(&self.inflight).pop_front() else {
            // Settled already by a failure (a writer that gave up on a stalled GPU).
            return;
        };
        if slots.len() != frames.len() {
            let why = format!(
                "a GPU batch returned {} frames for {} blocks",
                frames.len(),
                slots.len()
            );
            lock(&self.inflight).push_front(slots);
            self.fail(why);
            return;
        }
        let mut freed = 0;
        for (k, slot) in slots.into_iter().enumerate() {
            freed += slot.len;
            slot.req.resolve(slot.i, Some(frames.frame(k).to_vec()));
        }
        self.batches.fetch_add(1, Ordering::Relaxed);
        self.blocks
            .fetch_add(frames.len() as u64, Ordering::Relaxed);
        self.progress.fetch_add(1, Ordering::Relaxed);
        let mut st = lock(&self.state);
        st.bytes = st.bytes.saturating_sub(freed);
        drop(st);
        self.space.notify_all();
        // The GPU has room for the next batch.
        self.work.notify_all();
    }
}

/// The GPU batch compressor of one store. See the module docs.
pub(crate) struct GpuEncoder {
    shared: Arc<Shared>,
    thread: Mutex<Option<(JoinHandle<()>, mpsc::Receiver<()>)>>,
}

impl GpuEncoder {
    /// Starts the batcher thread. The GPU is opened (by `factory`) when the first block arrives.
    pub(crate) fn new(cfg: GpuConfig, factory: EngineFactory) -> GpuEncoder {
        let shared = Arc::new(Shared {
            cfg,
            state: Mutex::new(State {
                queue: VecDeque::new(),
                bytes: 0,
                shutdown: false,
                failed: None,
            }),
            work: Condvar::new(),
            space: Condvar::new(),
            inflight: Mutex::new(VecDeque::new()),
            progress: AtomicU64::new(0),
            batches: AtomicU64::new(0),
            blocks: AtomicU64::new(0),
            fallback: AtomicU64::new(0),
            opens: AtomicU64::new(0),
            adapter: Mutex::new(None),
        });
        let (exited_tx, exited) = mpsc::channel();
        let bg = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("block-store-gpu-zstd".into())
            .spawn(move || {
                let r = catch_unwind(AssertUnwindSafe(|| batcher(&bg, &factory)));
                if r.is_err() {
                    bg.fail("the GPU batcher thread panicked".into());
                }
                // Anything still held (a shutdown with blocks queued after the last batch).
                bg.fail_quietly();
                let _ = exited_tx.send(());
            });
        let thread = match spawned {
            Ok(h) => Some((h, exited)),
            Err(e) => {
                shared.fail(format!("could not start the GPU batcher thread: {e}"));
                None
            }
        };
        GpuEncoder {
            shared,
            thread: Mutex::new(thread),
        }
    }

    pub(crate) fn level(&self) -> GpuLevel {
        self.shared.cfg.level
    }

    pub(crate) fn verify(&self) -> bool {
        self.shared.cfg.verify
    }

    /// Turns the GPU off (a frame failed verification).
    pub(crate) fn disable(&self, why: String) {
        self.shared.fail(why);
    }

    /// Whether the GPU has been turned off.
    pub(crate) fn failed(&self) -> Option<String> {
        lock(&self.shared.state).failed.clone()
    }

    /// Compresses `blocks` (each 1 to 64 KiB) on the GPU, batched with every other writer's.
    /// Per block: its zstd frame, or `None` when the caller must compress it on the CPU.
    pub(crate) fn compress(&self, blocks: &[&[u8]]) -> Vec<Option<Vec<u8>>> {
        let n = blocks.len();
        if n == 0 {
            return Vec::new();
        }
        let req = Arc::new(Request {
            state: Mutex::new(RequestState {
                out: vec![None; n],
                left: n,
            }),
            done: Condvar::new(),
        });
        // Copied outside the queue lock: the blocks are borrowed, the batcher needs them owned.
        let mut items = blocks.iter().enumerate().map(|(i, b)| Item {
            data: b.to_vec(),
            slot: Slot {
                req: Arc::clone(&req),
                i,
                len: b.len(),
            },
            at: Instant::now(),
        });
        let max = self.shared.cfg.max_inflight_bytes;
        let mut pending = items.next();
        while let Some(item) = pending.take() {
            let mut st = lock(&self.shared.state);
            if st.failed.is_some() || st.shutdown {
                drop(st);
                // Settled here, never queued.
                req.resolve(item.slot.i, None);
                for it in items.by_ref() {
                    req.resolve(it.slot.i, None);
                }
                break;
            }
            if st.bytes > 0 && st.bytes + item.data.len() > max {
                let st = self
                    .shared
                    .space
                    .wait_timeout(st, Duration::from_millis(100))
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
                drop(st);
                pending = Some(item);
                continue;
            }
            // Queue as many as fit under one hold of the lock.
            let mut queued = 0;
            let mut next = Some(item);
            while let Some(mut it) = next.take() {
                if st.bytes > 0 && st.bytes + it.data.len() > max {
                    next = Some(it);
                    break;
                }
                st.bytes += it.data.len();
                it.at = Instant::now();
                st.queue.push_back(it);
                queued += 1;
                next = items.next();
            }
            drop(st);
            if queued > 0 {
                self.shared.work.notify_one();
            }
            pending = next;
        }
        self.wait(&req);
        let out: Vec<Option<Vec<u8>>> = std::mem::take(&mut lock(&req.state).out)
            .into_iter()
            .map(|r| r.flatten())
            .collect();
        let cpu = out.iter().filter(|r| r.is_none()).count() as u64;
        self.shared.fallback.fetch_add(cpu, Ordering::Relaxed);
        out
    }

    /// Waits for every block of `req`. If no batch completes for `stall_timeout`, the GPU is
    /// turned off and the blocks still pending are settled for the CPU.
    fn wait(&self, req: &Request) {
        let stall = self.shared.cfg.stall_timeout;
        let mut seen = self.shared.progress.load(Ordering::Relaxed);
        let mut since = Instant::now();
        let mut st = lock(&req.state);
        while st.left > 0 {
            let (g, _) = req
                .done
                .wait_timeout(st, Duration::from_millis(250).min(stall))
                .unwrap_or_else(|e| e.into_inner());
            st = g;
            if st.left == 0 {
                break;
            }
            let now = self.shared.progress.load(Ordering::Relaxed);
            if now != seen {
                seen = now;
                since = Instant::now();
            } else if since.elapsed() >= stall {
                drop(st);
                self.shared.fail(format!(
                    "no GPU batch completed in {}s",
                    stall.as_secs_f32()
                ));
                st = lock(&req.state);
                // The failure settled every queued or submitted block; settle any stragglers.
                for o in st.out.iter_mut().filter(|o| o.is_none()) {
                    *o = Some(None);
                }
                st.left = 0;
            }
        }
    }

    /// Compresses what is queued, then stops the batcher thread. Later calls to
    /// [`GpuEncoder::compress`] answer "CPU" for every block. Waits at most `stall_timeout`
    /// for the thread; a thread stuck in the GPU driver is left behind (and its blocks settled
    /// for the CPU).
    pub(crate) fn shutdown(&self) {
        let Some((handle, exited)) = lock(&self.thread).take() else {
            return;
        };
        lock(&self.shared.state).shutdown = true;
        self.shared.work.notify_all();
        self.shared.space.notify_all();
        match exited.recv_timeout(self.shared.cfg.stall_timeout) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = handle.join();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.shared
                    .fail("the GPU batcher did not stop at shutdown".into());
            }
        }
    }

    pub(crate) fn stats(&self) -> GpuStats {
        let s = &self.shared;
        GpuStats {
            preset: s.cfg.level.name(),
            batches: s.batches.load(Ordering::Relaxed),
            blocks: s.blocks.load(Ordering::Relaxed),
            cpu_fallback_blocks: s.fallback.load(Ordering::Relaxed),
            opens: s.opens.load(Ordering::Relaxed),
            failed: lock(&s.state).failed.clone(),
            adapter: lock(&s.adapter).clone(),
        }
    }
}

impl Drop for GpuEncoder {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Shared {
    /// Settles whatever is still held for the CPU without turning the GPU off (the batcher
    /// thread is exiting at shutdown).
    fn fail_quietly(&self) {
        let queued: Vec<Item> = lock(&self.state).queue.drain(..).collect();
        let inflight: Vec<Vec<Slot>> = lock(&self.inflight).drain(..).collect();
        let mut freed = 0;
        for slot in queued
            .into_iter()
            .map(|it| it.slot)
            .chain(inflight.into_iter().flatten())
        {
            freed += slot.len;
            slot.req.resolve(slot.i, None);
        }
        let mut st = lock(&self.state);
        st.bytes = st.bytes.saturating_sub(freed);
        st.shutdown = true;
        drop(st);
        self.space.notify_all();
    }
}

/// The batcher thread: opens the engine when blocks arrive, streams batches through it, drops
/// it when idle, and stops at shutdown or on the first failure.
fn batcher(shared: &Shared, factory: &EngineFactory) {
    let mut engine: Option<Box<dyn Engine>> = None;
    loop {
        {
            let mut st = lock(&shared.state);
            while st.queue.is_empty() && !st.shutdown && st.failed.is_none() {
                st = shared.work.wait(st).unwrap_or_else(|e| e.into_inner());
            }
            if st.failed.is_some() || (st.queue.is_empty() && st.shutdown) {
                return;
            }
        }
        if engine.is_none() {
            let opened = catch_unwind(AssertUnwindSafe(|| factory(&shared.cfg)))
                .unwrap_or_else(|_| Err("opening the GPU panicked".into()));
            match opened {
                Ok(e) => {
                    let what = e.describe();
                    tracing::info!(
                        preset = shared.cfg.level.name(),
                        device = %what,
                        "GPU zstd compressor ready"
                    );
                    *lock(&shared.adapter) = Some(what);
                    shared.opens.fetch_add(1, Ordering::Relaxed);
                    engine = Some(e);
                }
                Err(e) => {
                    shared.fail(format!("the GPU could not be opened: {e}"));
                    return;
                }
            }
        }
        let mut idle = false;
        let ran = {
            let eng = engine.as_deref().expect("opened above");
            let mut fill = |cap: usize| shared.fill(cap, &mut idle);
            let mut deliver = |frames: &dyn FrameSource| shared.deliver(frames);
            catch_unwind(AssertUnwindSafe(|| eng.run(&mut fill, &mut deliver)))
                .unwrap_or_else(|_| Err("a GPU batch panicked".into()))
        };
        if let Err(e) = ran {
            shared.fail(format!("a GPU batch failed: {e}"));
            return;
        }
        if !lock(&shared.inflight).is_empty() {
            shared.fail("the GPU run ended with batches undelivered".into());
            return;
        }
        if idle {
            tracing::debug!("GPU zstd compressor idle; releasing the GPU");
            engine = None;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A CPU stand-in for the GPU: zstd level 1 per block, counting batches and runs.
    #[derive(Default)]
    pub(crate) struct Fake {
        pub batches: AtomicUsize,
        pub blocks: AtomicUsize,
        pub runs: AtomicUsize,
        pub opens: AtomicUsize,
        /// Fail opening the engine.
        pub fail_open: std::sync::atomic::AtomicBool,
        /// Fail the run at this batch (1-based); 0 never.
        pub fail_batch: AtomicUsize,
        /// Sleep this long per batch, ms.
        pub delay_ms: AtomicUsize,
        /// Hang forever at this batch (1-based); 0 never.
        pub hang_batch: AtomicUsize,
    }

    struct FakeEngine(Arc<Fake>);

    struct Frames(Vec<Vec<u8>>);
    impl FrameSource for Frames {
        fn len(&self) -> usize {
            self.0.len()
        }
        fn frame(&self, k: usize) -> &[u8] {
            &self.0[k]
        }
    }

    impl Engine for FakeEngine {
        fn describe(&self) -> String {
            "fake".into()
        }
        fn run(&self, fill: &mut Fill<'_>, deliver: &mut Deliver<'_>) -> Result<(), String> {
            self.0.runs.fetch_add(1, Ordering::SeqCst);
            while let Some(blocks) = fill(64) {
                let n = self.0.batches.fetch_add(1, Ordering::SeqCst) + 1;
                if self.0.hang_batch.load(Ordering::SeqCst) == n {
                    loop {
                        std::thread::sleep(Duration::from_secs(3600));
                    }
                }
                if self.0.fail_batch.load(Ordering::SeqCst) == n {
                    return Err("injected batch failure".into());
                }
                let ms = self.0.delay_ms.load(Ordering::SeqCst);
                if ms > 0 {
                    std::thread::sleep(Duration::from_millis(ms as u64));
                }
                self.0.blocks.fetch_add(blocks.len(), Ordering::SeqCst);
                let frames = blocks
                    .iter()
                    .map(|b| zstd::bulk::compress(b, 1).unwrap())
                    .collect();
                deliver(&Frames(frames));
            }
            Ok(())
        }
    }

    pub(crate) fn fake_factory(fake: Arc<Fake>) -> EngineFactory {
        Box::new(move |_cfg: &GpuConfig| {
            if fake.fail_open.load(Ordering::SeqCst) {
                return Err("injected: no adapter".into());
            }
            fake.opens.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeEngine(Arc::clone(&fake))) as Box<dyn Engine>)
        })
    }

    pub(crate) fn test_cfg() -> GpuConfig {
        GpuConfig {
            batch_deadline: Duration::from_millis(20),
            idle_timeout: Duration::from_secs(5),
            ..GpuConfig::default()
        }
    }

    fn block(seed: u8, len: usize) -> Vec<u8> {
        (0..len).map(|i| (i / 7) as u8 ^ seed).collect()
    }

    fn check(blocks: &[Vec<u8>], out: &[Option<Vec<u8>>]) {
        assert_eq!(blocks.len(), out.len());
        for (b, f) in blocks.iter().zip(out) {
            let f = f.as_ref().expect("a GPU frame");
            assert_eq!(&zstd::bulk::decompress(f, b.len()).unwrap(), b);
        }
    }

    #[test]
    fn many_writers_share_few_batches() {
        let fake = Arc::new(Fake::default());
        let enc = GpuEncoder::new(test_cfg(), fake_factory(Arc::clone(&fake)));
        let writers = 16;
        let start = std::sync::Barrier::new(writers as usize);
        std::thread::scope(|s| {
            for w in 0..writers {
                let (enc, start) = (&enc, &start);
                s.spawn(move || {
                    let blocks: Vec<Vec<u8>> = (0..4).map(|i| block(w * 4 + i, 4096)).collect();
                    let refs: Vec<&[u8]> = blocks.iter().map(Vec::as_slice).collect();
                    start.wait();
                    check(&blocks, &enc.compress(&refs));
                });
            }
        });
        // 64 blocks from 16 writers; a batch holds 64, and the deadline gathers them.
        let batches = fake.batches.load(Ordering::SeqCst);
        assert!(batches <= 4, "{batches} batches");
        assert_eq!(fake.blocks.load(Ordering::SeqCst), 64);
        let st = enc.stats();
        assert_eq!(st.blocks, 64);
        assert_eq!(st.batches, batches as u64);
        assert_eq!(st.cpu_fallback_blocks, 0);
        assert_eq!(st.opens, 1);
    }

    #[test]
    fn a_gpu_that_cannot_open_falls_back_once() {
        let fake = Arc::new(Fake::default());
        fake.fail_open.store(true, Ordering::SeqCst);
        let enc = GpuEncoder::new(test_cfg(), fake_factory(Arc::clone(&fake)));
        let b = block(1, 1000);
        assert_eq!(enc.compress(&[&b, &b]), vec![None, None]);
        assert_eq!(enc.compress(&[&b]), vec![None]);
        let st = enc.stats();
        assert!(st.failed.unwrap().contains("injected: no adapter"));
        assert_eq!(st.cpu_fallback_blocks, 3);
        assert_eq!(st.blocks, 0);
    }

    #[test]
    fn a_failed_batch_settles_its_blocks_for_the_cpu() {
        let fake = Arc::new(Fake::default());
        fake.fail_batch.store(2, Ordering::SeqCst);
        let enc = GpuEncoder::new(test_cfg(), fake_factory(Arc::clone(&fake)));
        let blocks: Vec<Vec<u8>> = (0..200).map(|i| block(i as u8, 2048)).collect();
        let refs: Vec<&[u8]> = blocks.iter().map(Vec::as_slice).collect();
        let out = enc.compress(&refs);
        // The first batch (64 blocks) made it; everything after went to the CPU.
        assert_eq!(out.iter().filter(|o| o.is_some()).count(), 64);
        check(&blocks[..64], &out[..64]);
        assert!(enc.failed().unwrap().contains("injected batch failure"));
        assert!(enc.compress(&refs[..3]).iter().all(Option::is_none));
    }

    #[test]
    fn backpressure_bounds_the_bytes_in_flight() {
        let fake = Arc::new(Fake::default());
        fake.delay_ms.store(5, Ordering::SeqCst);
        let cfg = GpuConfig {
            max_inflight_bytes: 4 * GPU_MAX_BLOCK,
            ..test_cfg()
        };
        let enc = GpuEncoder::new(cfg, fake_factory(Arc::clone(&fake)));
        let blocks: Vec<Vec<u8>> = (0..40).map(|i| block(i as u8, GPU_MAX_BLOCK)).collect();
        let refs: Vec<&[u8]> = blocks.iter().map(Vec::as_slice).collect();
        let peak = AtomicUsize::new(0);
        let done = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                while !done.load(Ordering::SeqCst) {
                    let b = lock(&enc.shared.state).bytes;
                    peak.fetch_max(b, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_micros(200));
                }
            });
            check(&blocks, &enc.compress(&refs));
            done.store(true, Ordering::SeqCst);
        });
        assert!(peak.load(Ordering::SeqCst) <= 4 * GPU_MAX_BLOCK);
        assert!(fake.batches.load(Ordering::SeqCst) >= 10);
        assert_eq!(lock(&enc.shared.state).bytes, 0);
    }

    #[test]
    fn shutdown_with_blocks_in_flight_settles_every_writer() {
        let fake = Arc::new(Fake::default());
        fake.delay_ms.store(30, Ordering::SeqCst);
        let enc = GpuEncoder::new(test_cfg(), fake_factory(Arc::clone(&fake)));
        let results = Mutex::new(Vec::new());
        std::thread::scope(|s| {
            for w in 0..8u8 {
                let (enc, results) = (&enc, &results);
                s.spawn(move || {
                    let blocks: Vec<Vec<u8>> = (0..100).map(|i| block(w ^ i, 1500)).collect();
                    let refs: Vec<&[u8]> = blocks.iter().map(Vec::as_slice).collect();
                    let out = enc.compress(&refs);
                    lock(results).push((blocks, out));
                });
            }
            std::thread::sleep(Duration::from_millis(40));
            enc.shutdown();
        });
        let results = results.into_inner().unwrap();
        assert_eq!(results.len(), 8);
        for (blocks, out) in &results {
            for (b, f) in blocks.iter().zip(out) {
                if let Some(f) = f {
                    assert_eq!(&zstd::bulk::decompress(f, b.len()).unwrap(), b);
                }
            }
        }
        // Shutdown compresses what was queued: nothing was turned off.
        assert!(enc.failed().is_none());
        assert!(enc.compress(&[&block(0, 10)]).iter().all(Option::is_none));
    }

    #[test]
    fn a_stalled_gpu_is_given_up_on() {
        let fake = Arc::new(Fake::default());
        fake.hang_batch.store(1, Ordering::SeqCst);
        let cfg = GpuConfig {
            stall_timeout: Duration::from_millis(300),
            ..test_cfg()
        };
        let enc = GpuEncoder::new(cfg, fake_factory(Arc::clone(&fake)));
        let b = block(3, 5000);
        let t = Instant::now();
        assert_eq!(enc.compress(&[&b]), vec![None]);
        assert!(t.elapsed() < Duration::from_secs(5));
        assert!(enc.failed().unwrap().contains("no GPU batch completed"));
        // Shutdown does not wait forever for the stuck thread.
        enc.shutdown();
    }

    #[test]
    fn an_idle_engine_is_released_and_reopened() {
        let fake = Arc::new(Fake::default());
        let cfg = GpuConfig {
            idle_timeout: Duration::from_millis(50),
            ..test_cfg()
        };
        let enc = GpuEncoder::new(cfg, fake_factory(Arc::clone(&fake)));
        let b = block(9, 3000);
        check(std::slice::from_ref(&b), &enc.compress(&[&b]));
        std::thread::sleep(Duration::from_millis(300));
        check(std::slice::from_ref(&b), &enc.compress(&[&b]));
        assert_eq!(fake.opens.load(Ordering::SeqCst), 2);
        assert_eq!(enc.stats().opens, 2);
    }

    #[test]
    fn levels_round_trip_their_names() {
        for l in GpuLevel::ALL {
            assert_eq!(GpuLevel::from_name(l.name()), Some(l));
        }
        assert_eq!(GpuLevel::from_name("opt16"), None);
        assert!(GpuConfig::default().validate(64 * 1024).is_ok());
        assert!(GpuConfig::default().validate(128 * 1024).is_err());
    }
}

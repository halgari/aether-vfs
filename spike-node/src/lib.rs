//! Stage-4 task 5 spike: measure what an N-API round trip per read costs.
//!
//! Shape of the experiment
//! -----------------------
//! `registerProvider(fn)` is called **from whichever JS thread will service
//! provider calls** — the main thread, or a `worker_threads` Worker. It builds
//! an N-API threadsafe function bound to *that isolate's* event loop and parks
//! it in a process-global registry. Rust `static`s are shared by every isolate
//! that loads the same addon, so a Worker can register and a director thread
//! spawned from the main thread can call it. `bridgeCount()` exists to prove
//! that sharing empirically rather than assume it.
//!
//! A director thread's read is:
//!
//! 1. publish `(slot, generation, dst pointer, capacity)` into its own slot,
//! 2. `napi_call_threadsafe_function` with `(slot, generation, offset, len)`,
//! 3. park on a condvar until the JS side calls `complete(...)`, which copies
//!    the returned `Buffer` straight into `dst` **on the JS thread** — one
//!    copy, the trade spec §8b already makes.
//!
//! The generation counter is what makes the timeout in `probeBlockingRead`
//! sound: a call that lands after the waiter gave up sees a stale generation
//! and is dropped instead of writing through a pointer nobody owns any more.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use napi::bindgen_prelude::{AsyncTask, Buffer};
use napi::threadsafe_function::{
    ErrorStrategy, ThreadSafeCallContext, ThreadsafeFunction, ThreadsafeFunctionCallMode,
};
use napi::{Env, JsFunction, Result, Status, Task};
use napi_derive::napi;

use vfs_embed::{
    BlockCache, CacheConfig, Capabilities, CachingProvider, DirEntry, Handle, Provider, Stat, VPath,
    KIND_FILE, ST_IO_ERROR,
};

// ---------------------------------------------------------------------------
// Slots: one per director thread, each holding at most one outstanding call.
// ---------------------------------------------------------------------------

struct SlotState {
    generation: u64,
    done: bool,
    code: i32,
    len: usize,
    /// `*mut u8` of the caller's destination buffer, as a `usize` so the state
    /// stays `Send`. Zero means "no call in flight".
    dst: usize,
    cap: usize,
}

struct Slot {
    state: Mutex<SlotState>,
    settled: Condvar,
}

static SLOTS: OnceLock<RwLock<Vec<Arc<Slot>>>> = OnceLock::new();

fn slots() -> &'static RwLock<Vec<Arc<Slot>>> {
    SLOTS.get_or_init(|| RwLock::new(Vec::new()))
}

fn new_slot() -> (u32, Arc<Slot>) {
    let slot = Arc::new(Slot {
        state: Mutex::new(SlotState {
            generation: 0,
            done: false,
            code: 0,
            len: 0,
            dst: 0,
            cap: 0,
        }),
        settled: Condvar::new(),
    });
    let mut g = slots().write().expect("slot registry poisoned");
    g.push(Arc::clone(&slot));
    ((g.len() - 1) as u32, slot)
}

// ---------------------------------------------------------------------------
// Bridges: one per registered JS provider callback, i.e. one per serviced loop.
// ---------------------------------------------------------------------------

struct ReadReq {
    slot: u32,
    generation: u64,
    offset: u64,
    len: u32,
}

struct Bridge {
    tsfn: ThreadsafeFunction<ReadReq, ErrorStrategy::Fatal>,
    calls: AtomicU64,
}

static BRIDGES: OnceLock<RwLock<Vec<Arc<Bridge>>>> = OnceLock::new();

fn bridges() -> &'static RwLock<Vec<Arc<Bridge>>> {
    BRIDGES.get_or_init(|| RwLock::new(Vec::new()))
}

/// Register a provider callback from the calling JS thread. Returns the bridge
/// id. Signature seen by JS: `(slot, generation, offset, len) => void`, and the
/// callback is expected to hand the result back through `complete`.
#[napi(js_name = "registerProvider")]
pub fn register_provider(callback: JsFunction) -> Result<u32> {
    let tsfn: ThreadsafeFunction<ReadReq, ErrorStrategy::Fatal> = callback
        .create_threadsafe_function(0, |ctx: ThreadSafeCallContext<ReadReq>| {
            Ok(vec![
                ctx.value.slot as f64,
                ctx.value.generation as f64,
                ctx.value.offset as f64,
                ctx.value.len as f64,
            ])
        })?;
    let mut g = bridges().write().expect("bridge registry poisoned");
    g.push(Arc::new(Bridge {
        tsfn,
        calls: AtomicU64::new(0),
    }));
    Ok((g.len() - 1) as u32)
}

/// How many bridges the process has. Called from the main thread after a Worker
/// has registered, a non-zero answer proves Rust statics are shared across
/// isolates — the load-bearing assumption of the whole worker configuration.
#[napi(js_name = "bridgeCount")]
pub fn bridge_count() -> u32 {
    bridges().read().expect("bridge registry poisoned").len() as u32
}

/// Hand a read result back. Called on the provider's own JS thread. `data`
/// present means "copy these bytes into the waiting director thread's buffer".
#[napi(js_name = "complete")]
pub fn complete(slot: u32, generation: f64, code: i32, data: Option<Buffer>) -> Result<()> {
    let slot = {
        let g = slots().read().expect("slot registry poisoned");
        match g.get(slot as usize) {
            Some(s) => Arc::clone(s),
            None => return Ok(()),
        }
    };
    let generation = generation as u64;
    let mut st = slot.state.lock().expect("slot poisoned");
    // Stale completion: the waiter timed out (see `probeBlockingRead`) and its
    // buffer is gone. Dropping it here is what keeps the timeout sound.
    if st.dst == 0 || st.generation != generation || st.done {
        return Ok(());
    }
    let copied = match data.as_deref() {
        Some(bytes) => {
            let n = bytes.len().min(st.cap);
            // SAFETY: `dst`/`cap` were published by a director thread that is
            // parked on `slot.settled` and cannot return until `done` is set
            // below, so the buffer is live and exclusively ours for this call.
            // The generation check above rejects any call whose waiter left.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), st.dst as *mut u8, n);
            }
            n
        }
        None => 0,
    };
    st.len = copied;
    st.code = code;
    st.done = true;
    drop(st);
    slot.settled.notify_one();
    Ok(())
}

/// The blocking round trip. Returns `Ok(bytes)`, or `Err(code)`; `Err(None)`
/// shape is folded into `ST_IO_ERROR` because a spike does not need a richer
/// error channel than the spec's own.
fn round_trip(
    bridge: &Bridge,
    slot_id: u32,
    slot: &Slot,
    offset: u64,
    buf: &mut [u8],
    timeout: Option<Duration>,
) -> std::result::Result<usize, i32> {
    let generation = {
        let mut st = slot.state.lock().expect("slot poisoned");
        st.generation += 1;
        st.done = false;
        st.code = 0;
        st.len = 0;
        st.dst = buf.as_mut_ptr() as usize;
        st.cap = buf.len();
        st.generation
    };
    bridge.calls.fetch_add(1, Ordering::Relaxed);
    let status = bridge.tsfn.call(
        ReadReq {
            slot: slot_id,
            generation,
            offset,
            len: buf.len() as u32,
        },
        ThreadsafeFunctionCallMode::Blocking,
    );
    if status != Status::Ok {
        let mut st = slot.state.lock().expect("slot poisoned");
        st.dst = 0;
        return Err(ST_IO_ERROR);
    }
    let mut st = slot.state.lock().expect("slot poisoned");
    match timeout {
        None => {
            while !st.done {
                st = slot.settled.wait(st).expect("slot poisoned");
            }
        }
        Some(d) => {
            let deadline = Instant::now() + d;
            while !st.done {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                let (guard, _) = slot.settled.wait_timeout(st, left).expect("slot poisoned");
                st = guard;
            }
        }
    }
    let out = if st.done {
        if st.code == 0 {
            Ok(st.len)
        } else {
            Err(st.code)
        }
    } else {
        Err(i32::MIN) // sentinel: never settled
    };
    // Retire the buffer either way; a late `complete` will see `dst == 0`.
    st.dst = 0;
    st.cap = 0;
    out
}

// ---------------------------------------------------------------------------
// Per-thread wiring: which bridge this director thread talks to, and its slot.
// ---------------------------------------------------------------------------

struct ThreadWiring {
    bridge: Arc<Bridge>,
    slot_id: u32,
    slot: Arc<Slot>,
}

thread_local! {
    static WIRING: std::cell::RefCell<Option<ThreadWiring>> =
        const { std::cell::RefCell::new(None) };
}

fn wire_this_thread(bridge: Arc<Bridge>) {
    let (slot_id, slot) = new_slot();
    WIRING.with(|w| {
        *w.borrow_mut() = Some(ThreadWiring {
            bridge,
            slot_id,
            slot,
        })
    });
}

// ---------------------------------------------------------------------------
// The provider under test.
// ---------------------------------------------------------------------------

/// A read-only provider whose `read_at` is a JS call. `getattr`/`open`/`close`
/// are answered in Rust from a fixed descriptor: they happen once per file and
/// the unresolved question is specifically the *per-read* cost, so putting them
/// through the bridge too would only add noise to the number being measured.
struct JsProvider {
    size: u64,
}

impl Provider for JsProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            immutable: true,
            slow: true,
            ..Capabilities::read_only()
        }
    }
    fn getattr(&self, _p: VPath) -> std::result::Result<Option<Stat>, i32> {
        Ok(Some(Stat {
            kind: KIND_FILE,
            size: self.size,
            mtime: 0,
        }))
    }
    fn readdir(&self, _p: VPath) -> std::result::Result<Vec<DirEntry>, i32> {
        Ok(Vec::new())
    }
    fn open(&self, _p: VPath, _flags: u32) -> std::result::Result<(Handle, u64, bool), i32> {
        Ok((1, self.size, false))
    }
    fn close(&self, _h: Handle) -> std::result::Result<(), i32> {
        Ok(())
    }
    fn read_at(
        &self,
        _h: Handle,
        offset: u64,
        buf: &mut [u8],
    ) -> std::result::Result<usize, i32> {
        WIRING.with(|w| {
            let b = w.borrow();
            let wiring = b.as_ref().ok_or(ST_IO_ERROR)?;
            round_trip(
                &wiring.bridge,
                wiring.slot_id,
                &wiring.slot,
                offset,
                buf,
                None,
            )
        })
    }
}

// ---------------------------------------------------------------------------
// Benchmarks. Both run on a libuv worker thread via `AsyncTask`, so the main
// event loop stays free to service provider calls — which is exactly the
// contract §8b states, expressed in code.
// ---------------------------------------------------------------------------

#[napi(object)]
pub struct BenchResult {
    pub label: String,
    pub threads: u32,
    pub elapsed_ms: f64,
    /// `read_at` calls made against the top of the stack.
    pub reads: f64,
    /// Calls that actually crossed into JS.
    pub js_calls: f64,
    pub bytes: f64,
    pub mib_per_sec: f64,
    pub p50_us: f64,
    pub p99_us: f64,
    pub max_us: f64,
    pub cache_hits: f64,
    pub cache_misses: f64,
    /// Reads whose returned bytes were not the JS side's 0xAB payload, or whose
    /// length was short. A fast number with this non-zero would mean the
    /// boundary was not actually being crossed — the premise this spike is
    /// least allowed to assume.
    pub bad_payload_reads: f64,
    pub error: Option<String>,
}

fn percentile(sorted: &[u64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (((sorted.len() - 1) as f64) * q).round() as usize;
    sorted[idx] as f64 / 1000.0
}

#[napi(object)]
pub struct NopOpts {
    pub bridges: Vec<u32>,
    pub threads: u32,
    pub iters: f64,
    pub label: String,
}

pub struct NopTask {
    opts: NopOpts,
}

impl Task for NopTask {
    type Output = BenchResult;
    type JsValue = BenchResult;

    fn compute(&mut self) -> Result<Self::Output> {
        let iters = self.opts.iters as u64;
        let threads = self.opts.threads.max(1) as usize;
        let picked = pick_bridges(&self.opts.bridges)?;
        let start = Instant::now();
        let mut handles = Vec::new();
        for t in 0..threads {
            let bridge = Arc::clone(&picked[t % picked.len()]);
            handles.push(std::thread::spawn(move || {
                wire_this_thread(bridge);
                let mut lat = Vec::with_capacity(iters as usize);
                let mut sink = [0u8; 8];
                let mut fail = 0u64;
                WIRING.with(|w| {
                    let b = w.borrow();
                    let wiring = b.as_ref().expect("wired");
                    for _ in 0..iters {
                        let t0 = Instant::now();
                        let r = round_trip(
                            &wiring.bridge,
                            wiring.slot_id,
                            &wiring.slot,
                            0,
                            &mut sink[..0],
                            None,
                        );
                        lat.push(t0.elapsed().as_nanos() as u64);
                        if r.is_err() {
                            fail += 1;
                        }
                    }
                });
                (lat, fail)
            }));
        }
        let mut lat = Vec::new();
        let mut fail = 0u64;
        for h in handles {
            let (l, f) = h.join().map_err(|_| thread_panic())?;
            lat.extend_from_slice(&l);
            fail += f;
        }
        let elapsed = start.elapsed();
        lat.sort_unstable();
        Ok(BenchResult {
            label: self.opts.label.clone(),
            threads: threads as u32,
            elapsed_ms: elapsed.as_secs_f64() * 1000.0,
            reads: lat.len() as f64,
            js_calls: lat.len() as f64,
            bytes: 0.0,
            mib_per_sec: 0.0,
            p50_us: percentile(&lat, 0.50),
            p99_us: percentile(&lat, 0.99),
            max_us: lat.last().copied().unwrap_or(0) as f64 / 1000.0,
            cache_hits: 0.0,
            cache_misses: 0.0,
            bad_payload_reads: 0.0,
            error: (fail > 0).then(|| format!("{fail} round trips failed")),
        })
    }

    fn resolve(&mut self, _env: Env, out: Self::Output) -> Result<Self::JsValue> {
        Ok(out)
    }
}

#[napi(js_name = "benchNop", ts_return_type = "Promise<BenchResult>")]
pub fn bench_nop(opts: NopOpts) -> AsyncTask<NopTask> {
    AsyncTask::new(NopTask { opts })
}

#[napi(object)]
pub struct ReadOpts {
    pub bridges: Vec<u32>,
    pub threads: u32,
    pub file_size: f64,
    pub read_size: u32,
    pub cached: bool,
    pub block_size: f64,
    pub label: String,
}

pub struct ReadTask {
    opts: ReadOpts,
}

impl Task for ReadTask {
    type Output = BenchResult;
    type JsValue = BenchResult;

    fn compute(&mut self) -> Result<Self::Output> {
        let threads = self.opts.threads.max(1) as usize;
        let file_size = self.opts.file_size as u64;
        let read_size = self.opts.read_size.max(1) as usize;
        let picked = pick_bridges(&self.opts.bridges)?;

        let leaf: Arc<dyn Provider> = Arc::new(JsProvider { size: file_size });
        let cache = Arc::new(BlockCache::new(CacheConfig {
            block_size: self.opts.block_size as u64,
            // Big enough that the whole file stays resident: the question is
            // what `cached` costs per read, not what eviction costs.
            ram_budget: file_size * (threads as u64 + 1) + (16 << 20),
            disk_dir: None,
        }));
        let top: Arc<dyn Provider> = if self.opts.cached {
            Arc::new(CachingProvider::new(
                Arc::clone(&leaf),
                Arc::clone(&cache),
                7,
            ))
        } else {
            Arc::clone(&leaf)
        };
        let js_before = picked
            .iter()
            .map(|b| b.calls.load(Ordering::Relaxed))
            .sum::<u64>();

        // Each thread reads `file_size` bytes sequentially, but starts at its
        // own stagger offset and wraps. Without the stagger N threads march in
        // lockstep over the same block and the cached number measures a miss
        // stampede rather than concurrency.
        let start = Instant::now();
        let mut handles = Vec::new();
        for t in 0..threads {
            let bridge = Arc::clone(&picked[t % picked.len()]);
            let top = Arc::clone(&top);
            handles.push(std::thread::spawn(move || {
                wire_this_thread(bridge);
                let h = match top.open(VPath::at_default("bench.bin"), 0) {
                    Ok((h, _, _)) => h,
                    Err(c) => return (Vec::new(), 0u64, 1u64, c, 0u64),
                };
                let mut buf = vec![0u8; read_size];
                let mut lat = Vec::new();
                let mut bytes = 0u64;
                let mut fail = 0u64;
                let mut bad = 0u64;
                let mut last = 0i32;
                let mut off = (t as u64) * (file_size / threads as u64);
                let mut done = 0u64;
                while done < file_size {
                    let want = read_size.min((file_size - off) as usize);
                    let t0 = Instant::now();
                    match top.read_at(h, off, &mut buf[..want]) {
                        Ok(0) => break,
                        Ok(n) => {
                            bytes += n as u64;
                            done += n as u64;
                            off += n as u64;
                            if off >= file_size {
                                off = 0;
                            }
                        }
                        Err(c) => {
                            fail += 1;
                            last = c;
                            break;
                        }
                    }
                    lat.push(t0.elapsed().as_nanos() as u64);
                    // Outside the timer: cheap proof the JS payload really
                    // arrived. First/middle/last of the range that was just
                    // filled; a memcmp of the whole 64 KiB would be a tenth of
                    // the latency being measured.
                    let n = buf.len().min(want);
                    if n != want
                        || buf[0] != 0xAB
                        || buf[n / 2] != 0xAB
                        || buf[n - 1] != 0xAB
                    {
                        bad += 1;
                    }
                }
                let _ = top.close(h);
                (lat, bytes, fail, last, bad)
            }));
        }
        let mut lat = Vec::new();
        let mut bytes = 0u64;
        let mut fail = 0u64;
        let mut bad = 0u64;
        let mut last = 0i32;
        for h in handles {
            let (l, b, f, c, bd) = h.join().map_err(|_| thread_panic())?;
            lat.extend_from_slice(&l);
            bytes += b;
            fail += f;
            bad += bd;
            if f > 0 {
                last = c;
            }
        }
        let elapsed = start.elapsed();
        lat.sort_unstable();
        let js_calls = picked
            .iter()
            .map(|b| b.calls.load(Ordering::Relaxed))
            .sum::<u64>()
            - js_before;
        let stats = cache.stats();
        Ok(BenchResult {
            label: self.opts.label.clone(),
            threads: threads as u32,
            elapsed_ms: elapsed.as_secs_f64() * 1000.0,
            reads: lat.len() as f64,
            js_calls: js_calls as f64,
            bytes: bytes as f64,
            mib_per_sec: (bytes as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64(),
            p50_us: percentile(&lat, 0.50),
            p99_us: percentile(&lat, 0.99),
            max_us: lat.last().copied().unwrap_or(0) as f64 / 1000.0,
            cache_hits: stats.hits as f64,
            cache_misses: stats.misses as f64,
            bad_payload_reads: bad as f64,
            error: (fail > 0).then(|| format!("{fail} reads failed, last status {last}")),
        })
    }

    fn resolve(&mut self, _env: Env, out: Self::Output) -> Result<Self::JsValue> {
        Ok(out)
    }
}

#[napi(js_name = "benchRead", ts_return_type = "Promise<BenchResult>")]
pub fn bench_read(opts: ReadOpts) -> AsyncTask<ReadTask> {
    AsyncTask::new(ReadTask { opts })
}

fn pick_bridges(ids: &[u32]) -> Result<Vec<Arc<Bridge>>> {
    let g = bridges().read().expect("bridge registry poisoned");
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        match g.get(*id as usize) {
            Some(b) => out.push(Arc::clone(b)),
            None => {
                return Err(napi::Error::from_reason(format!(
                    "no bridge {id}; only {} registered",
                    g.len()
                )))
            }
        }
    }
    if out.is_empty() {
        return Err(napi::Error::from_reason("no bridges given"));
    }
    Ok(out)
}

fn thread_panic() -> napi::Error {
    napi::Error::from_reason("a bench thread panicked")
}

// ---------------------------------------------------------------------------
// The deadlock probe. Deliberately a *synchronous* export: it blocks whatever
// JS thread calls it, which is the rule §8b forbids breaking.
// ---------------------------------------------------------------------------

#[napi(object)]
pub struct ProbeResult {
    pub settled: bool,
    pub waited_ms: f64,
    pub code: i32,
    pub bytes: f64,
}

/// Perform one blocking provider round trip **on the calling JS thread**.
///
/// If `bridge` is serviced by the calling thread's own loop, that loop cannot
/// run the callback while parked here, and the wait can only end in the
/// timeout. If `bridge` is serviced by a different loop (another Worker), the
/// call settles normally. Running it both ways is the experiment.
#[napi(js_name = "probeBlockingRead")]
pub fn probe_blocking_read(bridge: u32, timeout_ms: f64, len: u32) -> Result<ProbeResult> {
    let bridge = pick_bridges(&[bridge])?.remove(0);
    let (slot_id, slot) = new_slot();
    let mut buf = vec![0u8; len as usize];
    let t0 = Instant::now();
    let r = round_trip(
        &bridge,
        slot_id,
        &slot,
        0,
        &mut buf,
        Some(Duration::from_millis(timeout_ms as u64)),
    );
    let waited = t0.elapsed();
    Ok(match r {
        Ok(n) => ProbeResult {
            settled: true,
            waited_ms: waited.as_secs_f64() * 1000.0,
            code: 0,
            bytes: n as f64,
        },
        Err(c) => ProbeResult {
            settled: false,
            waited_ms: waited.as_secs_f64() * 1000.0,
            code: c,
            bytes: 0.0,
        },
    })
}

// ---------------------------------------------------------------------------
// Zero-copy probe: is a SharedArrayBuffer's backing store a stable address
// that a Rust thread in another isolate can read? Spec §8 defers the zero-copy
// question; this only answers "is the door open", it builds nothing.
// ---------------------------------------------------------------------------

static PINNED: OnceLock<RwLock<Vec<(usize, usize)>>> = OnceLock::new();

fn pinned() -> &'static RwLock<Vec<(usize, usize)>> {
    PINNED.get_or_init(|| RwLock::new(Vec::new()))
}

/// Record the address and length of a typed array's backing store.
#[napi(js_name = "pinSharedBuffer")]
pub fn pin_shared_buffer(view: napi::bindgen_prelude::Uint8Array) -> u32 {
    let bytes: &[u8] = view.as_ref();
    let mut g = pinned().write().expect("pin registry poisoned");
    g.push((bytes.as_ptr() as usize, bytes.len()));
    (g.len() - 1) as u32
}

#[napi(js_name = "pinnedPointer")]
pub fn pinned_pointer(id: u32) -> String {
    let g = pinned().read().expect("pin registry poisoned");
    match g.get(id as usize) {
        Some((p, l)) => format!("0x{p:x}+{l}"),
        None => "none".to_string(),
    }
}

/// Read the pinned region from a plain Rust thread — no N-API call involved,
/// nothing holding the isolate. Returns how many bytes equal `expect`.
#[napi(js_name = "countPinnedFromRustThread")]
pub fn count_pinned_from_rust_thread(id: u32, expect: u32) -> Result<f64> {
    let (ptr, len) = {
        let g = pinned().read().expect("pin registry poisoned");
        *g.get(id as usize)
            .ok_or_else(|| napi::Error::from_reason("no such pin"))?
    };
    let want = expect as u8;
    let h = std::thread::spawn(move || {
        let mut n = 0u64;
        for i in 0..len {
            // SAFETY: probe only. A SharedArrayBuffer's backing store is not
            // relocatable and lives as long as any isolate holds the SAB, which
            // the caller guarantees for the duration of this call. Volatile so
            // the read is not hoisted; this is deliberately a race in the
            // formal sense and is why the spike only *observes* the door.
            let b = unsafe { std::ptr::read_volatile((ptr as *const u8).add(i)) };
            if b == want {
                n += 1;
            }
        }
        n
    });
    Ok(h.join().map_err(|_| thread_panic())? as f64)
}

/// Does the addon load in a Worker at all, and is this a fresh isolate? Each
/// isolate gets its own `Env`, but the count below lives in a Rust `static`
/// shared by all of them.
static ISOLATE_LOADS: AtomicU64 = AtomicU64::new(0);

#[napi(js_name = "noteIsolateLoad")]
pub fn note_isolate_load() -> f64 {
    ISOLATE_LOADS.fetch_add(1, Ordering::Relaxed) as f64 + 1.0
}

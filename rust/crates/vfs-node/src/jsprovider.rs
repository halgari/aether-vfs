//! **A JavaScript object as a first-class provider.** Spec §8b's threading
//! contract, as corrected by the measurements in §8c.
//!
//! A director thread reaching a JS provider does this:
//!
//! 1. builds a [`CallReq`] and parks an [`Arc<Call>`] in the process-global
//!    call table under a fresh id,
//! 2. `napi_call_threadsafe_function` — which queues the call on the event loop
//!    the provider was *registered from*,
//! 3. blocks on that call's condvar until the JS side calls [`complete_call`].
//!
//! The JS side runs the host's method, awaits it if it returned a thenable, and
//! hands the result back through `completeCall(callId, result)`. The dispatcher
//! that does this is `makeDispatch` in `index.cjs`; it is the one piece of the
//! path written in JS, because a threadsafe function needs a `JsFunction` and
//! because the `try`/`catch`/`then` that keeps rule 3 (no throw crosses
//! uncaught) is natural there and awkward here.
//!
//! ## What is decided here rather than in JS, and why
//!
//! * **`capabilities` is read once, at registration, and never crosses the
//!   bridge.** The trait says it is constant for the provider's lifetime, and
//!   there is a sharper reason: `mount` composes the graph on the *calling*
//!   thread, so a `capabilities()` that crossed the bridge would deadlock at
//!   mount time for a main-thread-serviced provider — the exact failure this
//!   module exists to prevent, triggered by the act of installing the provider.
//! * **Method presence is a bitmask read at registration.** A provider without
//!   `mkdir` gets `ST_NOT_SUPPORTED` without a round trip, which is what the
//!   trait's own defaults do for a Rust provider.
//! * **The error mapping is enforced on this side.** `index.cjs` classifies a
//!   throw, but [`decode`] is the authority: a `hostError` is `ST_IO_ERROR`
//!   whatever status JS sent, and a status that is not a recognised `ST_*`
//!   becomes `ST_IO_ERROR` too. So a host cannot invent a status code by
//!   throwing `{ vfsStatus: 12345 }`.
//!
//! ## The deadlock guard identifies a *loop*, not a thread role
//!
//! Task 5 measured all four combinations. `main → main-loop` never settles;
//! `main → worker` settles in 47 µs; `worker A → worker A's own loop` never
//! settles; `worker A → worker B` settles in 32 µs. So the invariant is loop
//! identity: **a provider call must not be serviced by the loop that is blocked
//! waiting for it.** `registerProvider` runs *on* the servicing loop's thread,
//! so [`Bridge::owner`] is `std::thread::current().id()` captured there, and
//! every call compares against it. A JS event loop is one OS thread for its
//! whole life — that is what makes the comparison sound — while director threads
//! and libuv pool threads are different OS threads and never false-positive.
//!
//! **What the guard does not catch, stated rather than hidden:** a *cycle*
//! through two loops. Provider A on loop 1 whose method synchronously drives a
//! read that reaches provider B on loop 2, whose method drives a read back into
//! A, deadlocks — and every individual call passes the guard, because in each one
//! the caller's thread is not the callee's owner. Detecting that needs a wait-for
//! graph across bridges, not a single comparison. The stall counter is what
//! notices it (`stallWarnMs`, default 5 s, logs "has not settled" and increments
//! `stalledCalls`), so it is diagnosable but not refused. No host has yet had a
//! reason to build such a cycle, and adding the machinery before there is one
//! would be guessing at its shape.
//!
//! ## One copy, no `unsafe`
//!
//! Task 5's spike memcpy'd the returned `Buffer` straight into the parked
//! director thread's destination on the JS thread. This module copies into an
//! owned `Vec` instead and the director thread copies out, which costs one extra
//! memcpy of at most the read size and buys `#![deny(unsafe_code)]` for the
//! whole crate. At 4 KiB that copy is tens of nanoseconds against a measured
//! 1.7–2.0 µs round trip; the spike's own numbers are what say the trade is not
//! worth arguing about.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use napi::bindgen_prelude::Buffer;
use napi::threadsafe_function::{
    ErrorStrategy, ThreadSafeCallContext, ThreadsafeFunction, ThreadsafeFunctionCallMode,
};
use napi::{Error, JsFunction, JsObject, JsUnknown, Result, Status, ValueType};
use napi_derive::napi;

use vfs_embed::{
    Access, Capabilities, DirEntry, Handle, Provider as VfsProvider, SetAttr, Stat, VPath,
    KIND_DIR, KIND_FILE, KIND_TOMBSTONE, ST_IO_ERROR, ST_NOT_SUPPORTED,
};

// ---------------------------------------------------------------------------
// Operations. One integer per provider method, shared with `index.cjs` through
// `providerOps()` so the two sides cannot drift silently.
// ---------------------------------------------------------------------------

pub(crate) const OP_GETATTR: u32 = 1;
pub(crate) const OP_READDIR: u32 = 2;
pub(crate) const OP_OPEN: u32 = 3;
pub(crate) const OP_CLOSE: u32 = 4;
pub(crate) const OP_READ_AT: u32 = 5;
pub(crate) const OP_READ_NEXT: u32 = 6;
pub(crate) const OP_WRITE_AT: u32 = 7;
pub(crate) const OP_SET_LEN: u32 = 8;
pub(crate) const OP_FLUSH: u32 = 9;
pub(crate) const OP_MKDIR: u32 = 10;
pub(crate) const OP_REMOVE: u32 = 11;
pub(crate) const OP_RENAME: u32 = 12;
pub(crate) const OP_SET_ATTR: u32 = 13;

/// Every op, with the JS method name it dispatches to. The order is the order
/// `providerOps()` reports and the order registration validation lists misses.
const OPS: &[(u32, &str)] = &[
    (OP_GETATTR, "getattr"),
    (OP_READDIR, "readdir"),
    (OP_OPEN, "open"),
    (OP_CLOSE, "close"),
    (OP_READ_AT, "readAt"),
    (OP_READ_NEXT, "readNext"),
    (OP_WRITE_AT, "writeAt"),
    (OP_SET_LEN, "setLen"),
    (OP_FLUSH, "flush"),
    (OP_MKDIR, "mkdir"),
    (OP_REMOVE, "remove"),
    (OP_RENAME, "rename"),
    (OP_SET_ATTR, "setAttr"),
];

fn op_name(op: u32) -> &'static str {
    OPS.iter()
        .find(|(o, _)| *o == op)
        .map(|(_, n)| *n)
        .unwrap_or("<unknown op>")
}

/// The op integers and their JS method names, so `index.cjs` does not hard-code
/// a second copy of the table. A dispatcher and a Rust caller that disagree
/// about which number means `readAt` would produce a provider that answers the
/// wrong question, which is exactly the class of failure this project keeps
/// finding.
#[napi(js_name = "providerOps")]
pub fn provider_ops() -> HashMap<String, u32> {
    OPS.iter().map(|(o, n)| ((*n).to_string(), *o)).collect()
}

/// The `OPEN_*` flag bits an `open(root, path, flags)` call receives. A JS
/// provider that means to support creation has to test `flags & OPEN_CREATE`,
/// and guessing the bit values is not something a host should have to do.
#[napi(js_name = "openFlags")]
pub fn open_flags() -> HashMap<String, u32> {
    [
        ("OPEN_READ", vfs_embed::OPEN_READ),
        ("OPEN_WRITE", vfs_embed::OPEN_WRITE),
        ("OPEN_CREATE", vfs_embed::OPEN_CREATE),
        ("OPEN_TRUNC", vfs_embed::OPEN_TRUNC),
        ("OPEN_APPEND", vfs_embed::OPEN_APPEND),
        ("OPEN_EXCL", vfs_embed::OPEN_EXCL),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

/// The `KIND_*` constants a `getattr`/`readdir` result uses, from the same place
/// Rust reads them.
#[napi(js_name = "kinds")]
pub fn kinds() -> HashMap<String, u32> {
    [
        ("KIND_FILE", KIND_FILE as u32),
        ("KIND_DIR", KIND_DIR as u32),
        ("KIND_TOMBSTONE", KIND_TOMBSTONE as u32),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

// ---------------------------------------------------------------------------
// The wire types.
// ---------------------------------------------------------------------------

/// A request as it leaves Rust. `Send`, because the threadsafe function moves it
/// to the servicing loop's thread; the `Buffer` is built there, in the callback.
#[derive(Default)]
struct CallReq {
    call_id: u64,
    op: u32,
    root: u32,
    path: String,
    root2: u32,
    path2: Option<String>,
    handle: u64,
    offset: u64,
    len: u32,
    flags: u32,
    data: Option<Vec<u8>>,
    mtime: Option<i64>,
    size: Option<u64>,
}

/// A request as JS sees it: one object argument to the dispatcher.
///
/// `f64` for handle/offset/size rather than `BigInt`, deliberately. 2^53 bytes
/// is nine petabytes and a `BigInt` would make the ordinary `offset + n`
/// arithmetic in a JS provider throw against a plain number — the same reasoning
/// as `RejectedWrite::count`.
#[napi(object)]
pub struct CallRequest {
    pub call_id: f64,
    /// One of `providerOps()`.
    pub op: u32,
    pub root: u32,
    pub path: String,
    /// `rename` only: the destination root.
    pub root2: Option<u32>,
    /// `rename` only: the destination path.
    pub path2: Option<String>,
    pub handle: f64,
    pub offset: f64,
    pub len: u32,
    pub flags: u32,
    /// `writeAt` only: the bytes to write.
    pub data: Option<Buffer>,
    /// `setAttr` only.
    pub mtime: Option<f64>,
    /// `setAttr` only.
    pub size: Option<f64>,
}

#[napi(object)]
pub struct JsStat {
    /// One of `kinds()`.
    pub kind: u32,
    pub size: f64,
    pub mtime: Option<f64>,
}

#[napi(object)]
pub struct JsDirEntry {
    pub name: String,
    pub kind: u32,
    pub size: f64,
    pub mtime: Option<f64>,
}

#[napi(object)]
pub struct JsOpen {
    pub handle: f64,
    pub size: f64,
    pub is_dir: Option<bool>,
}

/// What `completeCall` carries back. Exactly one of the payload fields is
/// populated on a successful call, chosen by the op; [`decode`] does not need to
/// know which op it was, because the shapes do not overlap.
#[napi(object)]
pub struct CallResult {
    /// An `ST_*` status. 0 is success.
    pub status: i32,
    /// The host method threw or its promise rejected. Set by `index.cjs`; what
    /// it buys is that a `VfsError(ST_OK)` cannot be mistaken for a success.
    pub threw: Option<bool>,
    /// Present when the throw was *not* a `VfsError`: message and stack. Its
    /// presence forces `ST_IO_ERROR` here regardless of `status`, which is spec
    /// §8b rule 3 enforced on the Rust side of the boundary.
    pub host_error: Option<String>,
    pub bytes: Option<Buffer>,
    pub number: Option<f64>,
    pub stat: Option<JsStat>,
    pub entries: Option<Vec<JsDirEntry>>,
    pub open: Option<JsOpen>,
}

/// What a director thread got back.
enum Out {
    Empty,
    Bytes(Vec<u8>),
    Number(f64),
    Stat(Stat),
    Entries(Vec<DirEntry>),
    Open(Handle, u64, bool),
}

// ---------------------------------------------------------------------------
// The call table. One entry per outstanding call, keyed by a monotonic id.
// ---------------------------------------------------------------------------

struct CallState {
    done: bool,
    status: i32,
    out: Out,
}

struct Call {
    state: Mutex<CallState>,
    settled: Condvar,
    /// Which bridge issued it — `release_provider` needs to find its waiters.
    bridge: u32,
    op: u32,
}

type CallTable = Mutex<HashMap<u64, Arc<Call>>>;

static CALLS: OnceLock<CallTable> = OnceLock::new();
static NEXT_CALL_ID: AtomicU64 = AtomicU64::new(1);

fn calls() -> &'static CallTable {
    CALLS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A call id that has left the table is a call whose waiter is gone. That is
/// what makes an abandoned call safe: a late `completeCall` finds nothing and
/// drops, rather than writing into a result nobody will read. Ids are never
/// reused (`u64`, monotonic), so a stale id cannot collide with a live call —
/// the same guarantee task 5's generation counter provided, expressed as key
/// lifetime instead.
fn next_call_id() -> u64 {
    NEXT_CALL_ID.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Bridges. One per registered JS provider, i.e. one per serviced loop.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Stats {
    calls: AtomicU64,
    settled: AtomicU64,
    vfs_errors: AtomicU64,
    host_errors: AtomicU64,
    stalled: AtomicU64,
    abandoned: AtomicU64,
    self_call_refusals: AtomicU64,
    dispatch_failures: AtomicU64,
    last_host_error: Mutex<Option<String>>,
    last_diagnostic: Mutex<Option<String>>,
}

struct Bridge {
    /// The provider-registry handle this bridge backs. `u32::MAX` until
    /// registration finishes interning the provider.
    provider_handle: AtomicU32,
    /// `None` after `releaseProvider`. An `RwLock` rather than a bare field
    /// because `ThreadsafeFunction::abort` consumes `self`, and the read is
    /// uncontended — tens of nanoseconds against a ~2 µs round trip.
    tsfn: RwLock<Option<ThreadsafeFunction<CallReq, ErrorStrategy::Fatal>>>,
    /// The thread of the event loop that services this provider — see the
    /// module docs. Captured in `registerProvider`, which runs on it.
    owner: ThreadId,
    owner_label: String,
    caps: Capabilities,
    /// Bit `op` set means the host object has that method.
    methods: u32,
    call_timeout: Option<Duration>,
    stall_warn: Duration,
    stats: Stats,
}

type BridgeTable = RwLock<HashMap<u32, Arc<Bridge>>>;

static BRIDGES: OnceLock<BridgeTable> = OnceLock::new();

fn bridges() -> &'static BridgeTable {
    BRIDGES.get_or_init(|| RwLock::new(HashMap::new()))
}

fn bridge_by_handle(handle: u32) -> Option<Arc<Bridge>> {
    bridges().read().ok()?.get(&handle).map(Arc::clone)
}

fn has_method(methods: u32, op: u32) -> bool {
    methods & (1u32 << op) != 0
}

// ---------------------------------------------------------------------------
// The diagnosis channel. A bridge failure that happens on the very thread that
// is about to return to JS can carry a full explanation instead of a status
// number; `crate::status_err` picks it up. Thread-local because the only case
// that reaches JS is the one where the caller *is* the failing thread — which
// is precisely the deadlock guard.
// ---------------------------------------------------------------------------

thread_local! {
    static DIAGNOSIS: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn set_diagnosis(msg: &str) {
    DIAGNOSIS.with(|d| *d.borrow_mut() = Some(msg.to_string()));
}

/// Take any diagnosis this thread recorded since [`clear_diagnosis`].
pub(crate) fn take_diagnosis() -> Option<String> {
    DIAGNOSIS.with(|d| d.borrow_mut().take())
}

/// Drop a stale diagnosis. Called at the top of every JS entry point that can
/// drive a provider, so a refusal from an earlier call cannot be reported
/// against a later, unrelated failure.
pub(crate) fn clear_diagnosis() {
    DIAGNOSIS.with(|d| *d.borrow_mut() = None);
}

impl Bridge {
    fn note(&self, msg: String) {
        eprintln!("{msg}");
        set_diagnosis(&msg);
        if let Ok(mut g) = self.stats.last_diagnostic.lock() {
            *g = Some(msg);
        }
    }

    fn handle(&self) -> u32 {
        self.provider_handle.load(Ordering::Relaxed)
    }

    /// The deadlock guard. Spec §8b rule 1, as corrected in §8c.
    fn refuse_self_call(&self, op: u32) -> i32 {
        self.stats.self_call_refusals.fetch_add(1, Ordering::Relaxed);
        self.note(format!(
            "aethervfs: refusing a provider call that would deadlock. Provider {} \
             is serviced by {}, and `{}` was invoked from that same thread, so \
             the loop would have to run the callback while parked waiting for \
             it — the call could never settle. A blocking provider call must \
             never be issued on the loop that services the provider; any other \
             loop, including the host's main thread, is safe. Register the \
             provider with providerWorker() and drive the session from a \
             different thread. (spec §8b rule 1, corrected by measurement in §8c)",
            self.handle(),
            self.owner_label,
            op_name(op),
        ));
        ST_IO_ERROR
    }

    fn note_stall(&self, op: u32, waited: Duration) {
        self.stats.stalled.fetch_add(1, Ordering::Relaxed);
        self.note(format!(
            "aethervfs: provider {} has not settled `{}` after {:.0} ms. One \
             director thread is parked on it; the session and every other \
             director thread are unaffected. Counted in \
             provider.stats().stalledCalls. Set callTimeoutMs on the provider to \
             abandon such a call instead of waiting for it.",
            self.handle(),
            op_name(op),
            waited.as_secs_f64() * 1000.0,
        ));
    }

    /// One blocking round trip. `Err(status)` for every failure, because that is
    /// all the `Provider` trait can carry; the readable form went to stderr, to
    /// `stats().lastDiagnostic`, and — when the caller is the thread that failed
    /// — to the thread-local diagnosis.
    fn dispatch(&self, mut req: CallReq) -> std::result::Result<Out, i32> {
        let op = req.op;
        if !has_method(self.methods, op) {
            return Err(ST_NOT_SUPPORTED);
        }

        // Rule 1, before anything is queued. Checked here rather than at
        // registration because it is a property of the *caller*, and the same
        // provider is legal from every other thread in the process.
        if self.owner == std::thread::current().id() {
            return Err(self.refuse_self_call(op));
        }

        let call_id = next_call_id();
        req.call_id = call_id;
        let call = Arc::new(Call {
            state: Mutex::new(CallState {
                done: false,
                status: 0,
                out: Out::Empty,
            }),
            settled: Condvar::new(),
            bridge: self.handle(),
            op,
        });
        match calls().lock() {
            Ok(mut g) => {
                g.insert(call_id, Arc::clone(&call));
            }
            Err(_) => return Err(ST_IO_ERROR),
        }
        self.stats.calls.fetch_add(1, Ordering::Relaxed);

        let status = match self.tsfn.read() {
            Ok(g) => match &*g {
                Some(t) => t.call(req, ThreadsafeFunctionCallMode::Blocking),
                None => Status::Closing,
            },
            Err(_) => Status::GenericFailure,
        };
        if status != Status::Ok {
            forget_call(call_id);
            self.stats.dispatch_failures.fetch_add(1, Ordering::Relaxed);
            self.note(format!(
                "aethervfs: could not queue `{}` on provider {} ({status:?}). The \
                 provider's loop is gone — releaseProvider() was called, or the \
                 worker servicing it exited.",
                op_name(op),
                self.handle(),
            ));
            return Err(ST_IO_ERROR);
        }

        let out = self.park(&call, op);
        forget_call(call_id);
        out
    }

    /// Block until the call settles, the stall threshold passes, or the
    /// `callTimeoutMs` deadline expires. Rule 2: this may take as long as the
    /// host takes.
    fn park(&self, call: &Call, op: u32) -> std::result::Result<Out, i32> {
        let start = Instant::now();
        let mut warned = false;
        let mut st = match call.state.lock() {
            Ok(g) => g,
            Err(_) => return Err(ST_IO_ERROR),
        };
        loop {
            if st.done {
                break;
            }
            let elapsed = start.elapsed();
            if self.call_timeout.is_some_and(|t| elapsed >= t) {
                break;
            }
            if !warned && elapsed >= self.stall_warn {
                warned = true;
                // Report outside the lock: `note_stall` writes to stderr and takes
                // the stats mutex, and holding this call's state lock across that
                // would block the completion that may be arriving right now.
                drop(st);
                self.note_stall(op, elapsed);
                st = match call.state.lock() {
                    Ok(g) => g,
                    Err(_) => return Err(ST_IO_ERROR),
                };
                continue;
            }
            // The next moment anything can change: whichever threshold is still
            // ahead of us. With neither, wait indefinitely — a released bridge
            // wakes its waiters explicitly, so this cannot be a lost wakeup.
            let next = match (self.call_timeout, warned) {
                (Some(t), false) => Some((t - elapsed).min(self.stall_warn - elapsed)),
                (Some(t), true) => Some(t - elapsed),
                (None, false) => Some(self.stall_warn - elapsed),
                (None, true) => None,
            };
            st = match next {
                Some(d) => match call.settled.wait_timeout(st, d) {
                    Ok((g, _)) => g,
                    Err(_) => return Err(ST_IO_ERROR),
                },
                None => match call.settled.wait(st) {
                    Ok(g) => g,
                    Err(_) => return Err(ST_IO_ERROR),
                },
            };
        }

        if !st.done {
            let waited = start.elapsed();
            drop(st);
            self.stats.abandoned.fetch_add(1, Ordering::Relaxed);
            self.note(format!(
                "aethervfs: abandoned `{}` on provider {} after {:.0} ms without \
                 it settling (callTimeoutMs). Counted in \
                 provider.stats().abandonedCalls; a completion arriving later is \
                 dropped, and the director thread is released with ST_IO_ERROR \
                 rather than held forever.",
                op_name(op),
                self.handle(),
                waited.as_secs_f64() * 1000.0,
            ));
            return Err(ST_IO_ERROR);
        }

        self.stats.settled.fetch_add(1, Ordering::Relaxed);
        if st.status == 0 {
            Ok(std::mem::replace(&mut st.out, Out::Empty))
        } else {
            Err(st.status)
        }
    }
}

fn forget_call(call_id: u64) {
    if let Ok(mut g) = calls().lock() {
        g.remove(&call_id);
    }
}

// ---------------------------------------------------------------------------
// Completion, and the error mapping.
// ---------------------------------------------------------------------------

/// Hand a provider call's result back. Called on the provider's own JS thread by
/// the dispatcher in `index.cjs`; a `callId` with no waiter is dropped.
#[napi(js_name = "completeCall")]
pub fn complete_call(call_id: f64, result: CallResult) -> Result<()> {
    let call = match calls().lock() {
        Ok(g) => g.get(&(call_id as u64)).map(Arc::clone),
        Err(_) => return Ok(()),
    };
    // No waiter: abandoned, or a duplicate completion. Both are dropped rather
    // than reported, because the failure was already counted where it happened.
    let Some(call) = call else { return Ok(()) };
    let bridge = bridge_by_handle(call.bridge);
    let (status, out) = decode(result, call.op, bridge.as_deref());
    let mut st = match call.state.lock() {
        Ok(g) => g,
        Err(_) => return Ok(()),
    };
    if st.done {
        return Ok(());
    }
    st.status = status;
    st.out = out;
    st.done = true;
    drop(st);
    call.settled.notify_all();
    Ok(())
}

/// Is this a status the workspace defines? A host that throws
/// `{ vfsStatus: 12345 }` must not be able to inject an unknown code into the
/// director, so anything unrecognised becomes `ST_IO_ERROR`.
fn is_known_status(status: i32) -> bool {
    crate::status_name(status) != crate::UNKNOWN_STATUS
}

/// Spec §8b rule 3, enforced here rather than trusted from JS.
fn decode(r: CallResult, op: u32, bridge: Option<&Bridge>) -> (i32, Out) {
    if let Some(stack) = r.host_error {
        eprintln!(
            "aethervfs: provider threw on `{}` — mapped to ST_IO_ERROR. The host \
             process is unaffected; the call fails and the stack follows.\n{}",
            op_name(op),
            stack
        );
        if let Some(b) = bridge {
            b.stats.host_errors.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut g) = b.stats.last_host_error.lock() {
                *g = Some(stack);
            }
        }
        return (ST_IO_ERROR, Out::Empty);
    }

    if r.threw == Some(true) || r.status != 0 {
        let mapped = if r.status != vfs_embed::ST_OK && is_known_status(r.status) {
            r.status
        } else {
            eprintln!(
                "aethervfs: provider signalled status {} on `{}`, which is not a \
                 recognised ST_* code — mapped to ST_IO_ERROR.",
                r.status,
                op_name(op)
            );
            ST_IO_ERROR
        };
        if let Some(b) = bridge {
            b.stats.vfs_errors.fetch_add(1, Ordering::Relaxed);
        }
        return (mapped, Out::Empty);
    }

    // Success. The payload shapes do not overlap, so the op is not needed to
    // pick one; the caller checks it got the shape its op expects.
    let out = if let Some(b) = r.bytes {
        Out::Bytes(b.to_vec())
    } else if let Some(e) = r.entries {
        Out::Entries(
            e.into_iter()
                .map(|d| DirEntry {
                    name: d.name,
                    stat: Stat {
                        kind: d.kind as u8,
                        size: d.size as u64,
                        mtime: d.mtime.unwrap_or(0.0) as i64,
                    },
                })
                .collect(),
        )
    } else if let Some(o) = r.open {
        Out::Open(o.handle as Handle, o.size as u64, o.is_dir.unwrap_or(false))
    } else if let Some(s) = r.stat {
        Out::Stat(Stat {
            kind: s.kind as u8,
            size: s.size as u64,
            mtime: s.mtime.unwrap_or(0.0) as i64,
        })
    } else if let Some(n) = r.number {
        Out::Number(n)
    } else {
        Out::Empty
    };
    (0, out)
}

// ---------------------------------------------------------------------------
// The provider.
// ---------------------------------------------------------------------------

struct JsProvider {
    bridge: Arc<Bridge>,
}

impl JsProvider {
    fn req(&self, op: u32) -> CallReq {
        CallReq {
            op,
            ..Default::default()
        }
    }

    /// A result of the wrong shape is the host's bug, and reporting it as
    /// `ST_IO_ERROR` with a message beats silently answering "empty".
    fn wrong_shape<T>(&self, op: u32, wanted: &str) -> std::result::Result<T, i32> {
        self.bridge.note(format!(
            "aethervfs: provider {} returned nothing usable from `{}` — {} was \
             expected. Mapped to ST_IO_ERROR.",
            self.bridge.handle(),
            op_name(op),
            wanted,
        ));
        Err(ST_IO_ERROR)
    }
}

impl VfsProvider for JsProvider {
    fn capabilities(&self) -> Capabilities {
        self.bridge.caps
    }

    fn getattr(&self, p: VPath) -> std::result::Result<Option<Stat>, i32> {
        let mut req = self.req(OP_GETATTR);
        req.root = p.root.0;
        req.path = p.rel.to_string();
        match self.bridge.dispatch(req)? {
            Out::Stat(s) => Ok(Some(s)),
            // `null`/`undefined` from `getattr` is "not here", which is the
            // trait's `Ok(None)` and not an error.
            _ => Ok(None),
        }
    }

    fn readdir(&self, p: VPath) -> std::result::Result<Vec<DirEntry>, i32> {
        let mut req = self.req(OP_READDIR);
        req.root = p.root.0;
        req.path = p.rel.to_string();
        match self.bridge.dispatch(req)? {
            Out::Entries(e) => Ok(e),
            _ => Ok(Vec::new()),
        }
    }

    fn open(&self, p: VPath, flags: u32) -> std::result::Result<(Handle, u64, bool), i32> {
        let mut req = self.req(OP_OPEN);
        req.root = p.root.0;
        req.path = p.rel.to_string();
        req.flags = flags;
        match self.bridge.dispatch(req)? {
            Out::Open(h, size, is_dir) => Ok((h, size, is_dir)),
            _ => self.wrong_shape(OP_OPEN, "{ handle, size, isDir? }"),
        }
    }

    fn close(&self, h: Handle) -> std::result::Result<(), i32> {
        let mut req = self.req(OP_CLOSE);
        req.handle = h;
        self.bridge.dispatch(req)?;
        Ok(())
    }

    fn read_at(
        &self,
        h: Handle,
        offset: u64,
        buf: &mut [u8],
    ) -> std::result::Result<usize, i32> {
        let mut req = self.req(OP_READ_AT);
        req.handle = h;
        req.offset = offset;
        req.len = buf.len() as u32;
        match self.bridge.dispatch(req)? {
            Out::Bytes(b) => {
                let n = b.len().min(buf.len());
                buf[..n].copy_from_slice(&b[..n]);
                Ok(n)
            }
            _ => self.wrong_shape(OP_READ_AT, "a Buffer or Uint8Array"),
        }
    }

    fn read_next(&self, h: Handle, buf: &mut [u8]) -> std::result::Result<usize, i32> {
        let mut req = self.req(OP_READ_NEXT);
        req.handle = h;
        req.len = buf.len() as u32;
        match self.bridge.dispatch(req)? {
            Out::Bytes(b) => {
                let n = b.len().min(buf.len());
                buf[..n].copy_from_slice(&b[..n]);
                Ok(n)
            }
            _ => self.wrong_shape(OP_READ_NEXT, "a Buffer or Uint8Array"),
        }
    }

    fn write_at(&self, h: Handle, offset: u64, buf: &[u8]) -> std::result::Result<usize, i32> {
        let mut req = self.req(OP_WRITE_AT);
        req.handle = h;
        req.offset = offset;
        req.len = buf.len() as u32;
        req.data = Some(buf.to_vec());
        match self.bridge.dispatch(req)? {
            Out::Number(n) => Ok((n as usize).min(buf.len())),
            _ => self.wrong_shape(OP_WRITE_AT, "the number of bytes written"),
        }
    }

    fn set_len(&self, h: Handle, len: u64) -> std::result::Result<(), i32> {
        let mut req = self.req(OP_SET_LEN);
        req.handle = h;
        req.size = Some(len);
        self.bridge.dispatch(req)?;
        Ok(())
    }

    fn flush(&self, h: Handle) -> std::result::Result<(), i32> {
        let mut req = self.req(OP_FLUSH);
        req.handle = h;
        self.bridge.dispatch(req)?;
        Ok(())
    }

    fn mkdir(&self, p: VPath) -> std::result::Result<(), i32> {
        let mut req = self.req(OP_MKDIR);
        req.root = p.root.0;
        req.path = p.rel.to_string();
        self.bridge.dispatch(req)?;
        Ok(())
    }

    fn remove(&self, p: VPath) -> std::result::Result<(), i32> {
        let mut req = self.req(OP_REMOVE);
        req.root = p.root.0;
        req.path = p.rel.to_string();
        self.bridge.dispatch(req)?;
        Ok(())
    }

    fn rename(&self, from: VPath, to: VPath) -> std::result::Result<(), i32> {
        let mut req = self.req(OP_RENAME);
        req.root = from.root.0;
        req.path = from.rel.to_string();
        req.root2 = to.root.0;
        req.path2 = Some(to.rel.to_string());
        self.bridge.dispatch(req)?;
        Ok(())
    }

    fn set_attr(&self, p: VPath, attr: SetAttr) -> std::result::Result<(), i32> {
        let mut req = self.req(OP_SET_ATTR);
        req.root = p.root.0;
        req.path = p.rel.to_string();
        req.mtime = attr.mtime;
        req.size = attr.size;
        self.bridge.dispatch(req)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Registration.
// ---------------------------------------------------------------------------

#[napi(object)]
pub struct ProviderOptions {
    /// Abandon a call that has not settled after this long: the director thread
    /// is released with `ST_IO_ERROR` and `stats().abandonedCalls` counts it.
    ///
    /// Unset is the default and is the contract in spec §8b — *"a provider that
    /// never settles hangs one director thread, not the session"*. The hang is
    /// still diagnosable without this: `stallWarnMs` counts and logs it. Set it
    /// when a host would rather lose a read than a thread.
    pub call_timeout_ms: Option<f64>,
    /// Count and log a call still outstanding after this long. Default 5000.
    /// Fires at most once per call.
    pub stall_warn_ms: Option<f64>,
}

fn opt_unknown(obj: &JsObject, key: &str) -> Result<Option<JsUnknown>> {
    let v: JsUnknown = obj.get_named_property(key)?;
    Ok(match v.get_type()? {
        ValueType::Undefined | ValueType::Null => None,
        _ => Some(v),
    })
}

fn read_caps(obj: &JsObject) -> Result<Capabilities> {
    let Some(v) = opt_unknown(obj, "capabilities")? else {
        return Ok(Capabilities::read_only());
    };
    if v.get_type()? != ValueType::Object {
        return Err(Error::from_reason(
            "registerProvider: `capabilities` must be an object, e.g. \
             { access: 'read', immutable: true, slow: true, preferredBlock: 65536 }",
        ));
    }
    let c = v.coerce_to_object()?;

    let access = match opt_unknown(&c, "access")? {
        None => Access::Read,
        Some(a) => {
            let raw = a.coerce_to_string()?.into_utf8()?.into_owned()?;
            let norm: String = raw
                .chars()
                .filter(|ch| ch.is_ascii_alphanumeric())
                .map(|ch| ch.to_ascii_lowercase())
                .collect();
            match norm.as_str() {
                "read" => Access::Read,
                "readwrite" | "rw" => Access::ReadWrite,
                "seqread" | "sequential" => Access::SeqRead,
                _ => {
                    return Err(Error::from_reason(format!(
                        "registerProvider: capabilities.access was {raw:?}; expected \
                         'read', 'readwrite' or 'seqread'"
                    )))
                }
            }
        }
    };

    let bool_of = |key: &str| -> Result<bool> {
        match opt_unknown(&c, key)? {
            None => Ok(false),
            Some(v) => v.coerce_to_bool()?.get_value(),
        }
    };
    let immutable = bool_of("immutable")?;
    let slow = bool_of("slow")?;
    let preferred_block = match opt_unknown(&c, "preferredBlock")? {
        None => None,
        Some(v) => Some(v.coerce_to_number()?.get_uint32()?),
    };

    let caps = Capabilities {
        access,
        immutable,
        slow,
        preferred_block,
    };
    caps.validate().map_err(|e| {
        Error::from_reason(format!(
            "registerProvider: contradictory capabilities — {e}. Checked here, at \
             construction, so the session never starts with a graph that cannot \
             mean what it says."
        ))
    })?;
    Ok(caps)
}

/// Which provider methods the object actually has.
///
/// `napi_get_named_property` is a JS `[[Get]]`, so it walks the prototype chain
/// — a `class MyProvider { readAt() {} }` instance reports `readAt`, which
/// `Object.hasOwn` would not.
fn read_methods(obj: &JsObject) -> Result<u32> {
    let mut mask = 0u32;
    for (op, name) in OPS {
        let v: JsUnknown = obj.get_named_property(name)?;
        if v.get_type()? == ValueType::Function {
            mask |= 1u32 << op;
        }
    }
    Ok(mask)
}

/// Spec §8b rule 5, widened to every method the declared access implies.
///
/// The rule the brief names is `ReadWrite` without `writeAt`; the others are the
/// same defect with a different method, and a provider that cannot answer
/// `open` is no more mountable than one that cannot answer `writeAt`.
fn validate_methods(caps: Capabilities, methods: u32) -> Result<()> {
    let mut missing: Vec<&str> = Vec::new();
    let mut require = |op: u32| {
        if !has_method(methods, op) {
            missing.push(op_name(op));
        }
    };
    require(OP_GETATTR);
    require(OP_READDIR);
    require(OP_OPEN);
    require(OP_CLOSE);
    match caps.access {
        Access::SeqRead => require(OP_READ_NEXT),
        Access::Read => require(OP_READ_AT),
        Access::ReadWrite => {
            require(OP_READ_AT);
            require(OP_WRITE_AT);
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    Err(Error::from_reason(format!(
        "registerProvider: a provider declaring access {:?} must implement {} — \
         missing. Refused at construction rather than at the first call, because \
         the first call happens inside an injected process, where the symptom is \
         a game that will not start and the cause is nowhere near it. \
         (spec §8b, registration-time validation)",
        caps.access,
        missing.join(", "),
    )))
}

/// Turn a JS object into an `Arc<dyn Provider>` the director mounts anywhere a
/// Rust provider goes. Returns the same process-global integer handle `disk()`
/// returns.
///
/// **Call it on the thread that will service the provider.** That thread's event
/// loop is where every method runs, and it is the one thread that may not drive
/// a session mounting this provider — see the module docs. `index.cjs` wraps
/// this as `registerProvider(obj)` and builds the dispatcher; `providerWorker()`
/// wraps *that* to put the whole thing on a dedicated worker loop, which task 5
/// measured as the only configuration immune to a busy main loop (1449 vs
/// 3.8 MiB/s).
#[napi(js_name = "registerProvider")]
pub fn register_provider(
    obj: JsObject,
    dispatch: JsFunction,
    options: Option<ProviderOptions>,
) -> Result<crate::Provider> {
    let caps = read_caps(&obj)?;
    let methods = read_methods(&obj)?;
    validate_methods(caps, methods)?;

    let o = options.unwrap_or(ProviderOptions {
        call_timeout_ms: None,
        stall_warn_ms: None,
    });
    let call_timeout = o
        .call_timeout_ms
        .filter(|ms| *ms > 0.0)
        .map(|ms| Duration::from_secs_f64(ms / 1000.0));
    let stall_warn = Duration::from_secs_f64(
        o.stall_warn_ms.filter(|ms| *ms > 0.0).unwrap_or(5000.0) / 1000.0,
    );

    let tsfn: ThreadsafeFunction<CallReq, ErrorStrategy::Fatal> = dispatch
        .create_threadsafe_function(0, |ctx: ThreadSafeCallContext<CallReq>| {
            let v = ctx.value;
            Ok(vec![CallRequest {
                call_id: v.call_id as f64,
                op: v.op,
                root: v.root,
                path: v.path,
                root2: (v.op == OP_RENAME).then_some(v.root2),
                path2: v.path2,
                handle: v.handle as f64,
                offset: v.offset as f64,
                len: v.len,
                flags: v.flags,
                data: v.data.map(Buffer::from),
                mtime: v.mtime.map(|m| m as f64),
                size: v.size.map(|s| s as f64),
            }])
        })?;

    let current = std::thread::current();
    let bridge = Arc::new(Bridge {
        provider_handle: AtomicU32::new(u32::MAX),
        tsfn: RwLock::new(Some(tsfn)),
        owner: current.id(),
        owner_label: match current.name() {
            Some(n) => format!("{:?} (\"{n}\")", current.id()),
            None => format!("{:?}", current.id()),
        },
        caps,
        methods,
        call_timeout,
        stall_warn,
        stats: Stats::default(),
    });

    let handle = crate::intern_provider(Arc::new(JsProvider {
        bridge: Arc::clone(&bridge),
    }))?;
    crate::primitives::note_leaf(handle, "js");
    bridge.provider_handle.store(handle, Ordering::Relaxed);
    bridges()
        .write()
        .map_err(|_| Error::from_reason("bridge registry poisoned"))?
        .insert(handle, bridge);
    Ok(crate::Provider::wrap(handle))
}

/// Release the loop this provider is serviced by, so the worker holding it can
/// exit. Calls afterwards fail with `ST_IO_ERROR` rather than hanging, and any
/// director thread already parked on it is woken.
///
/// The registry entry itself stays — a handle is process-global and outlives the
/// wrapper object by design (see [`crate::intern_provider`]) — so a later
/// `Provider.fromHandle` still resolves, and reports `released: true` from
/// `stats()`.
#[napi(js_name = "releaseProvider")]
pub fn release_provider(handle: u32) -> Result<()> {
    let Some(b) = bridge_by_handle(handle) else {
        return Err(Error::from_reason(format!(
            "releaseProvider({handle}): not a JS-backed provider. Only providers \
             made by registerProvider() have a loop to release."
        )));
    };
    let taken = b.tsfn.write().ok().and_then(|mut g| g.take());
    if let Some(t) = taken {
        let _ = t.abort();
    }
    // A director thread parked on this bridge would otherwise wait for a loop
    // that will never run again. Wake them with a status instead.
    let waiting: Vec<Arc<Call>> = match calls().lock() {
        Ok(g) => g
            .values()
            .filter(|c| c.bridge == handle)
            .map(Arc::clone)
            .collect(),
        Err(_) => Vec::new(),
    };
    for c in waiting {
        if let Ok(mut st) = c.state.lock() {
            if !st.done {
                st.done = true;
                st.status = ST_IO_ERROR;
                st.out = Out::Empty;
                drop(st);
                c.settled.notify_all();
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Stats — where a stalled, abandoned or refused call is counted.
// ---------------------------------------------------------------------------

#[napi(object)]
pub struct ProviderStats {
    pub handle: u32,
    /// `'read'`, `'readwrite'` or `'seqread'`.
    pub access: String,
    pub immutable: bool,
    pub slow: bool,
    pub preferred_block: Option<u32>,
    /// The provider methods the object was found to have at registration.
    pub methods: Vec<String>,
    /// The event loop that services this provider, as the deadlock guard names
    /// it.
    pub owner_thread: String,
    pub released: bool,
    pub call_timeout_ms: Option<f64>,
    pub stall_warn_ms: f64,
    /// Calls handed to the bridge.
    pub calls: f64,
    /// Calls that settled, successfully or with a status.
    pub settled_calls: f64,
    /// Calls that came back as a `VfsError`, or as a status the host set.
    pub vfs_errors: f64,
    /// Calls where the host threw something that was not a `VfsError`. Each one
    /// was logged with its stack and mapped to `ST_IO_ERROR`.
    pub host_errors: f64,
    /// Calls still outstanding when `stallWarnMs` passed. **This is where a
    /// provider that never settles is counted**, and it does not require
    /// `callTimeoutMs` to be set.
    pub stalled_calls: f64,
    /// Calls given up on because `callTimeoutMs` expired.
    pub abandoned_calls: f64,
    /// Calls refused by the deadlock guard: issued on the loop that services
    /// this provider.
    pub self_call_refusals: f64,
    /// Calls that could not be queued at all — a released or dead loop.
    pub dispatch_failures: f64,
    pub last_host_error: Option<String>,
    /// The last full-sentence explanation this bridge produced, whatever kind.
    pub last_diagnostic: Option<String>,
}

fn access_name(a: Access) -> &'static str {
    match a {
        Access::SeqRead => "seqread",
        Access::Read => "read",
        Access::ReadWrite => "readwrite",
    }
}

/// `None` when `handle` is not a JS-backed provider — `disk()` and the Rust
/// primitives have no bridge and nothing to report.
pub(crate) fn stats_for(handle: u32) -> Option<ProviderStats> {
    let b = bridge_by_handle(handle)?;
    let s = &b.stats;
    Some(ProviderStats {
        handle,
        access: access_name(b.caps.access).to_string(),
        immutable: b.caps.immutable,
        slow: b.caps.slow,
        preferred_block: b.caps.preferred_block,
        methods: OPS
            .iter()
            .filter(|(op, _)| has_method(b.methods, *op))
            .map(|(_, n)| (*n).to_string())
            .collect(),
        owner_thread: b.owner_label.clone(),
        released: b.tsfn.read().map(|g| g.is_none()).unwrap_or(true),
        call_timeout_ms: b.call_timeout.map(|d| d.as_secs_f64() * 1000.0),
        stall_warn_ms: b.stall_warn.as_secs_f64() * 1000.0,
        calls: s.calls.load(Ordering::Relaxed) as f64,
        settled_calls: s.settled.load(Ordering::Relaxed) as f64,
        vfs_errors: s.vfs_errors.load(Ordering::Relaxed) as f64,
        host_errors: s.host_errors.load(Ordering::Relaxed) as f64,
        stalled_calls: s.stalled.load(Ordering::Relaxed) as f64,
        abandoned_calls: s.abandoned.load(Ordering::Relaxed) as f64,
        self_call_refusals: s.self_call_refusals.load(Ordering::Relaxed) as f64,
        dispatch_failures: s.dispatch_failures.load(Ordering::Relaxed) as f64,
        last_host_error: s.last_host_error.lock().ok().and_then(|g| g.clone()),
        last_diagnostic: s.last_diagnostic.lock().ok().and_then(|g| g.clone()),
    })
}

/// How many provider calls are outstanding across the whole process. A number
/// that never returns to zero after a quiet moment is the signature of a
/// provider that does not settle.
#[napi(js_name = "outstandingProviderCalls")]
pub fn outstanding_provider_calls() -> f64 {
    calls().lock().map(|g| g.len()).unwrap_or(0) as f64
}

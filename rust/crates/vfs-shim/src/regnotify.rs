//! Registry change notifications on keys the overlay serves: `NtNotifyChangeKey` and
//! `NtNotifyChangeMultipleKeys` (registry overlay spec section 3.4, ruling R1).
//!
//! **Which calls are served here.** Every notification on a key the overlay serves: a synthetic
//! handle, and a real (pass-through) handle on a virtualised path, touched by the overlay or
//! not. Each becomes an *overlay waiter* that completes when the overlay changes at the key (or
//! below it, with `WatchTree`). No real notification is registered for those keys, so a change
//! another process makes to the real registry does not complete them; spec 3.4 accepts that,
//! because nothing else writes those keys during a game. A handle the overlay does not serve
//! (not a key, or a key outside `\Registry\Machine` and `\Registry\User`) gets the real call.
//!
//! Why a real handle on an untouched virtualised path does not get the real call as well: every
//! write the game makes through it goes to the overlay, so the real notification would never
//! fire for the game's own writes. A "shadow" waiter beside the real notification, completing
//! whichever fires first, is not safe: the real completion writes the caller's I/O status block
//! and queues its APC asynchronously, and cannot be withdrawn, so it would land a second time
//! after the caller had already seen the notification complete and reused (or freed) that
//! memory. An overlay-only waiter has one completion, always.
//!
//! **The notifier.** One thread, started when the first waiter is registered and gone once none
//! is left, wakes every [`POLL`] (R1), and asks the director `REG_CHANGED` for each waiter. It
//! skips the round trips when the registry generation the director publishes has not moved since
//! a poll that got every answer: no write anywhere means no change for anyone. A poll the
//! director does not answer is counted, and its waiters keep waiting (they are never completed
//! spuriously). The thread holds the shim's re-entrancy guard for its whole life, so its own
//! calls never re-enter the hooks, and no lock is held across a director request.
//!
//! **Completion, as Windows and Wine complete a key notification.**
//! - Asynchronous: the call returns `STATUS_PENDING` and resets the caller's event. On a change
//!   the I/O status block gets `STATUS_NOTIFY_ENUM_DIR`, the event is signalled, and the APC (if
//!   any) is queued to the calling thread as `ApcRoutine(ApcContext, IoStatusBlock, 0)`.
//! - Synchronous: the caller blocks (not alertably, as Wine's `NtNotifyChangeMultipleKeys`
//!   waits) until the overlay changes, and gets `STATUS_SUCCESS`.
//! - Closing the key handle completes its waiters with `STATUS_NOTIFY_CLEANUP` (event, status
//!   block, APC), as Windows does; a synchronous caller returns that status.
//! - The status block of an asynchronous call with an event and no APC is **not written**.
//!   Wine never writes it, and Wine's own `RegNotifyChangeKeyValue` passes a status block on its
//!   stack and returns before the notification fires: writing it later would corrupt whatever
//!   that thread's stack holds by then. A caller that can only learn the outcome from the block
//!   (an APC, or no event) gets it written.
//! - `CompletionFilter` is coarse: any overlay change at the key (or below, with `WatchTree`)
//!   completes the waiter. `Buffer`/`BufferSize` are not filled, as on Windows.
//! - `NtNotifyChangeMultipleKeys` with subordinate keys (`Count > 0`) on a served key is
//!   `STATUS_NOT_SUPPORTED`; `Count = 0` is `NtNotifyChangeKey`.
#![allow(unsafe_code)]

use core::ffi::c_void;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use vfs_registry::Lookup;
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};

use crate::hookstats::{note_reg_notify, RegNotify};
use crate::ntdef::{
    STATUS_ACCESS_DENIED, STATUS_INSUFFICIENT_RESOURCES, STATUS_INVALID_HANDLE, STATUS_KEY_DELETED,
    STATUS_NOTIFY_CLEANUP, STATUS_NOTIFY_ENUM_DIR, STATUS_NOT_SUPPORTED, STATUS_PENDING,
    STATUS_SUCCESS, STATUS_UNSUCCESSFUL,
};
use crate::regclient;
use crate::regkeys::{self, Real, KEY_NOTIFY};

/// How often the notifier asks the director (ruling R1).
pub const POLL: Duration = Duration::from_millis(250);

/// Pending waiters at most; a registration past it fails with `STATUS_INSUFFICIENT_RESOURCES`.
const MAX_WAITERS: usize = 4096;

/// What a notification hook does with a call.
pub enum Notify {
    /// Not a key the overlay serves: the real call.
    Pass,
    /// Answered here.
    Done(NTSTATUS),
}

/// The caller's arguments, as the hooks receive them.
pub struct Args {
    pub event: HANDLE,
    pub apc: *const c_void,
    pub apc_ctx: *const c_void,
    pub iosb: *mut c_void,
    pub subtree: bool,
    pub asynchronous: bool,
    /// `NtNotifyChangeMultipleKeys`' subordinate key count (0 for `NtNotifyChangeKey`).
    pub count: u32,
}

/// How a waiter tells its caller.
enum Done {
    Async {
        event: isize,
        apc: usize,
        apc_ctx: usize,
        /// The caller's status block, or 0 when it is not to be written.
        iosb: usize,
        /// A handle to the calling thread for the APC (owned), or 0.
        thread: isize,
    },
    /// A caller blocked in the hook on `slot.event`.
    Sync(Arc<SyncSlot>),
}

struct SyncSlot {
    /// The shim's own event the caller waits on (owned by the waiting thread).
    event: isize,
    status: AtomicI32,
}

struct Waiter {
    id: u64,
    handle: isize,
    path: String,
    subtree: bool,
    /// The overlay version the waiter watches changes after. `None` until the director answers.
    since: Option<u64>,
    done: Done,
}

struct State {
    waiters: Vec<Waiter>,
    /// The notifier thread is alive (or being started).
    running: bool,
    next_id: u64,
}

static STATE: Mutex<State> = Mutex::new(State {
    waiters: Vec::new(),
    running: false,
    next_id: 1,
});

/// Pending waiters, readable without the lock (`cleanup` runs on every key handle close).
static PENDING: AtomicUsize = AtomicUsize::new(0);

/// Pending waiters. For tests and diagnostics.
pub fn pending() -> usize {
    PENDING.load(Ordering::Relaxed)
}

/// The ntdll calls a completion makes, none of them hooked.
struct NtApi {
    set_event: unsafe extern "system" fn(HANDLE, *mut i32) -> NTSTATUS,
    reset_event: unsafe extern "system" fn(HANDLE, *mut i32) -> NTSTATUS,
    queue_apc: unsafe extern "system" fn(HANDLE, *const c_void, usize, usize, usize) -> NTSTATUS,
}

fn nt() -> Option<&'static NtApi> {
    static API: OnceLock<Option<NtApi>> = OnceLock::new();
    API.get_or_init(|| {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
        // SAFETY: ntdll is always loaded; each export is cast to its documented signature.
        unsafe {
            let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
            if ntdll.is_null() {
                return None;
            }
            let get = |n: &core::ffi::CStr| GetProcAddress(ntdll, n.as_ptr().cast());
            Some(NtApi {
                set_event: core::mem::transmute::<
                    unsafe extern "system" fn() -> isize,
                    unsafe extern "system" fn(HANDLE, *mut i32) -> NTSTATUS,
                >(get(c"NtSetEvent")?),
                reset_event: core::mem::transmute::<
                    unsafe extern "system" fn() -> isize,
                    unsafe extern "system" fn(HANDLE, *mut i32) -> NTSTATUS,
                >(get(c"NtResetEvent")?),
                queue_apc: core::mem::transmute::<
                    unsafe extern "system" fn() -> isize,
                    unsafe extern "system" fn(
                        HANDLE,
                        *const c_void,
                        usize,
                        usize,
                        usize,
                    ) -> NTSTATUS,
                >(get(c"NtQueueApcThread")?),
            })
        }
    })
    .as_ref()
}

/// A notification on key handle `h`. See the module docs.
///
/// # Safety
/// The arguments are the caller's NT arguments.
pub unsafe fn notify(real: &Real, h: isize, a: &Args) -> Notify {
    let (path, access, deleted) = if regkeys::is_synthetic(h) {
        match regkeys::synthetic(h) {
            Some(k) => (k.path, k.access, k.deleted),
            None => return Notify::Done(STATUS_INVALID_HANDLE),
        }
    } else {
        match regkeys::resolve_handle(real, h) {
            Some(r) => (r.path, r.access, r.deleted),
            None => return Notify::Pass,
        }
    };
    if a.count > 0 {
        return Notify::Done(STATUS_NOT_SUPPORTED);
    }
    if access & KEY_NOTIFY == 0 {
        return Notify::Done(STATUS_ACCESS_DENIED);
    }
    if deleted || matches!(regclient::lookup(&path), Ok((Lookup::Tombstoned, _))) {
        return Notify::Done(STATUS_KEY_DELETED);
    }
    let Some(api) = nt() else {
        return Notify::Done(STATUS_UNSUCCESSFUL);
    };
    // The version to watch from. A director that does not answer leaves it for the notifier's
    // first poll to fill in (nothing can change in the overlay while it cannot be reached).
    let since = match regclient::changed(&path, a.subtree, u64::MAX) {
        Ok((_, v)) => Some(v),
        Err(_) => {
            note_reg_notify(RegNotify::PollError);
            None
        }
    };
    if !a.asynchronous {
        return Notify::Done(wait_sync(real, h, path, a, since));
    }
    // Registration resets the caller's event (Windows and Wine both do), which also checks it is
    // an event handle.
    if !a.event.is_null() {
        let st = (api.reset_event)(a.event, core::ptr::null_mut());
        if st < 0 {
            return Notify::Done(st);
        }
    }
    let apc = a.apc as usize;
    let thread = if apc != 0 { current_thread() } else { 0 };
    let iosb = if apc != 0 || a.event.is_null() {
        a.iosb as usize
    } else {
        0
    };
    let done = Done::Async {
        event: a.event as isize,
        apc,
        apc_ctx: a.apc_ctx as usize,
        iosb,
        thread,
    };
    match register(h, path, a.subtree, since, done) {
        Ok(()) => Notify::Done(STATUS_PENDING),
        Err((st, done)) => {
            release(real, done);
            Notify::Done(st)
        }
    }
}

/// A synchronous notification: block until the waiter completes, and answer its status.
unsafe fn wait_sync(real: &Real, h: isize, path: String, a: &Args, since: Option<u64>) -> NTSTATUS {
    use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};
    let event = CreateEventW(core::ptr::null(), 1, 0, core::ptr::null()) as isize;
    if event == 0 {
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    let slot = Arc::new(SyncSlot {
        event,
        status: AtomicI32::new(STATUS_UNSUCCESSFUL),
    });
    if let Err((st, _)) = register(h, path, a.subtree, since, Done::Sync(slot.clone())) {
        regkeys::close_real(real, event);
        return st;
    }
    WaitForSingleObject(event as HANDLE, INFINITE);
    regkeys::close_real(real, event);
    let st = slot.status.load(Ordering::Acquire);
    // The caller is blocked in this call, so its status block is live.
    write_iosb(a.iosb as usize, st);
    st
}

/// A handle to the calling thread an APC can be queued to, or 0.
fn current_thread() -> isize {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThreadId, OpenThread, THREAD_SET_CONTEXT,
    };
    // SAFETY: opens this thread by id; `NtOpenThread` is not hooked.
    unsafe { OpenThread(THREAD_SET_CONTEXT, 0, GetCurrentThreadId()) as isize }
}

/// Add a waiter, and start the notifier if it is not running. On failure the waiter's `Done`
/// comes back for the caller to release.
fn register(
    h: isize,
    path: String,
    subtree: bool,
    since: Option<u64>,
    done: Done,
) -> Result<(), (NTSTATUS, Done)> {
    let start = {
        let Ok(mut s) = STATE.lock() else {
            return Err((STATUS_UNSUCCESSFUL, done));
        };
        if s.waiters.len() >= MAX_WAITERS {
            return Err((STATUS_INSUFFICIENT_RESOURCES, done));
        }
        let id = s.next_id;
        s.next_id += 1;
        s.waiters.push(Waiter {
            id,
            handle: h,
            path,
            subtree,
            since,
            done,
        });
        PENDING.store(s.waiters.len(), Ordering::Relaxed);
        !std::mem::replace(&mut s.running, true)
    };
    note_reg_notify(RegNotify::Registered);
    if start && !spawn_notifier() {
        // No thread: the waiters stay (a close still ends them), and the next registration
        // tries again.
        note_reg_notify(RegNotify::PollError);
        if let Ok(mut s) = STATE.lock() {
            s.running = false;
        }
    }
    Ok(())
}

/// Start the notifier thread, the way the shim starts its other threads (`std::thread`, which
/// only creates the thread: safe from a hook, and under the loader lock, where the new thread
/// simply starts once the lock is released).
fn spawn_notifier() -> bool {
    std::thread::Builder::new()
        .name("vfs-reg-notify".into())
        .spawn(notifier)
        .is_ok()
}

fn notifier() {
    // Its registry and file calls are the shim's own: straight to ntdll.
    let _guard = crate::hook::ShimIoGuard::enter();
    // SAFETY: the trampolines, for closing the handles completions own.
    let real = unsafe { crate::hook::reg_real() };
    // The generation of the last poll that got every answer.
    let mut seen: Option<u64> = None;
    loop {
        std::thread::sleep(POLL);
        // Read before the waiters, so a waiter registered after the snapshot is one whose
        // registration came after this read: any write after that moves the generation again.
        let generation = regclient::generation();
        let polls: Vec<(u64, String, bool, Option<u64>)> = {
            let Ok(mut s) = STATE.lock() else {
                return;
            };
            if s.waiters.is_empty() {
                s.running = false;
                return;
            }
            s.waiters
                .iter()
                .map(|w| (w.id, w.path.clone(), w.subtree, w.since))
                .collect()
        };
        if generation != 0 && seen == Some(generation) && polls.iter().all(|p| p.3.is_some()) {
            continue;
        }
        let mut answered = true;
        let mut answers = Vec::with_capacity(polls.len());
        for (id, path, subtree, since) in &polls {
            match regclient::changed(path, *subtree, since.unwrap_or(u64::MAX)) {
                Ok((changed, version)) => answers.push((*id, since.is_some() && changed, version)),
                Err(_) => {
                    note_reg_notify(RegNotify::PollError);
                    answered = false;
                }
            }
        }
        seen = answered.then_some(generation);
        let fired = {
            let Ok(mut s) = STATE.lock() else {
                return;
            };
            let mut fired = Vec::new();
            for (id, fire, version) in answers {
                // A waiter may have been ended by a close meanwhile.
                let Some(i) = s.waiters.iter().position(|w| w.id == id) else {
                    continue;
                };
                if fire {
                    fired.push(s.waiters.remove(i));
                } else if s.waiters[i].since.is_none() {
                    s.waiters[i].since = Some(version);
                }
            }
            PENDING.store(s.waiters.len(), Ordering::Relaxed);
            fired
        };
        for w in fired {
            // SAFETY: the waiter's caller registered these handles and addresses for this.
            unsafe { complete(&real, w.done, STATUS_NOTIFY_ENUM_DIR) };
            note_reg_notify(RegNotify::Completed);
        }
    }
}

/// End every waiter on key handle `h`, which is being closed: `STATUS_NOTIFY_CLEANUP`.
pub fn cleanup(h: isize) {
    if PENDING.load(Ordering::Relaxed) == 0 {
        return;
    }
    let gone: Vec<Waiter> = {
        let Some(mut s) = regkeys::lock_for_close(&STATE) else {
            return;
        };
        let mut gone = Vec::new();
        let mut i = 0;
        while i < s.waiters.len() {
            if s.waiters[i].handle == h {
                gone.push(s.waiters.remove(i));
            } else {
                i += 1;
            }
        }
        PENDING.store(s.waiters.len(), Ordering::Relaxed);
        gone
    };
    if gone.is_empty() {
        return;
    }
    // SAFETY: the trampolines, for closing the handles completions own.
    let real = unsafe { crate::hook::reg_real() };
    for w in gone {
        // SAFETY: as in the notifier.
        unsafe { complete(&real, w.done, STATUS_NOTIFY_CLEANUP) };
        note_reg_notify(RegNotify::CleanedUp);
    }
}

/// Tell a waiter's caller: status block, then event, then APC, as Windows completes it. A
/// synchronous caller gets `STATUS_SUCCESS` for a change and the cleanup status for a close.
unsafe fn complete(real: &Real, done: Done, status: NTSTATUS) {
    let Some(api) = nt() else {
        return;
    };
    match done {
        Done::Sync(slot) => {
            let st = if status == STATUS_NOTIFY_ENUM_DIR {
                STATUS_SUCCESS
            } else {
                status
            };
            slot.status.store(st, Ordering::Release);
            // The waiting thread closes the event once woken; this is the last use of it here.
            (api.set_event)(slot.event as HANDLE, core::ptr::null_mut());
        }
        Done::Async {
            event,
            apc,
            apc_ctx,
            iosb,
            thread,
        } => {
            write_iosb(iosb, status);
            if event != 0 {
                (api.set_event)(event as HANDLE, core::ptr::null_mut());
            }
            if apc != 0 && thread != 0 {
                (api.queue_apc)(thread as HANDLE, apc as *const c_void, apc_ctx, iosb, 0);
            }
            if thread != 0 {
                regkeys::close_real(real, thread);
            }
        }
    }
}

/// Release what a waiter that was never registered owns.
unsafe fn release(real: &Real, done: Done) {
    if let Done::Async { thread, .. } = done {
        if thread != 0 {
            regkeys::close_real(real, thread);
        }
    }
}

/// `IO_STATUS_BLOCK { Status, Information = 0 }` at `iosb` (0: none).
unsafe fn write_iosb(iosb: usize, status: NTSTATUS) {
    if iosb == 0 {
        return;
    }
    core::ptr::write_volatile(iosb as *mut i32, status);
    core::ptr::write_volatile((iosb + core::mem::size_of::<usize>()) as *mut usize, 0);
}

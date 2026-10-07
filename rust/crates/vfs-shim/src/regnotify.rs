//! Registry change notifications on keys the overlay serves: `NtNotifyChangeKey` and
//! `NtNotifyChangeMultipleKeys` (registry overlay spec section 3.4, ruling R1).
//!
//! **Scope: overlay changes only, on every key the overlay serves.** This is a deliberate scope
//! choice. Every notification on a key the overlay serves (a synthetic handle, or a real
//! pass-through handle on a virtualised path, touched by the overlay or not) becomes an *overlay
//! waiter*, completed when the overlay changes at the key (or below it, with `WatchTree`). No
//! real notification is registered for those keys, so a change another process makes to the
//! real registry does not complete them; spec 3.4 accepts that, because nothing else writes
//! those keys during a game. What it buys: the game's own writes, which all go to the overlay,
//! always wake its watchers, whichever handle they were made through, and every waiter has
//! exactly one completion. A handle the overlay does not serve (not a key, or a key outside
//! `\Registry\Machine` and `\Registry\User`) gets the real call.
//!
//! Passing the caller's own arguments to the real call as well (a "shadow" waiter beside it,
//! first completion wins) is not safe: the real completion writes the caller's status block and
//! queues its APC asynchronously and cannot be withdrawn, so it would land a second time after
//! the caller had already seen the notification complete. A safe way to also watch the real
//! registry, left as a possible follow-up: one shim-owned real notification per served handle,
//! made with a duplicated event and a status block from a shim pool (never the caller's), whose
//! event the notifier checks on each poll and turns into an ordinary overlay-style completion.
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

use crate::hookstats::{RegNotify, note_reg_notify};
use crate::ntdef::{
    STATUS_ACCESS_DENIED, STATUS_INSUFFICIENT_RESOURCES, STATUS_INVALID_HANDLE, STATUS_KEY_DELETED,
    STATUS_NOT_SUPPORTED, STATUS_NOTIFY_CLEANUP, STATUS_NOTIFY_ENUM_DIR, STATUS_PENDING,
    STATUS_SUCCESS, STATUS_UNSUCCESSFUL,
};
use crate::regclient;
use crate::regkeys::{self, KEY_NOTIFY, Real};

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
        /// The shim's own duplicate of the caller's event (owned), or 0.
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
    let regkeys::KeyRef {
        path,
        access,
        deleted,
        ..
    } = match regkeys::key_handle(real, h, regkeys::Mode::Read) {
        regkeys::KeyHandle::Key(k) => k,
        regkeys::KeyHandle::Invalid => return Notify::Done(STATUS_INVALID_HANDLE),
        regkeys::KeyHandle::NotOurs | regkeys::KeyHandle::Unresolvable => return Notify::Pass,
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
    // The waiter owns a duplicate of the caller's event, never the caller's handle value: the
    // caller may close its handle before the notification completes, and the value may by then
    // name some other object.
    let event = if a.event.is_null() {
        0
    } else {
        match duplicate(real, a.event as isize, 0, DUPLICATE_SAME_ACCESS) {
            Ok(e) => e,
            Err(st) => return Notify::Done(st),
        }
    };
    let apc = a.apc as usize;
    // The calling thread, for the APC. A waiter whose APC could never be queued is refused.
    let thread = if apc == 0 {
        0
    } else {
        match duplicate(real, CURRENT_THREAD, THREAD_SET_CONTEXT, 0) {
            Ok(t) => t,
            Err(st) => {
                if event != 0 {
                    regkeys::close_real(real, event);
                }
                return Notify::Done(st);
            }
        }
    };
    let iosb = if apc != 0 || a.event.is_null() {
        a.iosb as usize
    } else {
        0
    };
    let done = Done::Async {
        event,
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

/// `NtCurrentThread()`.
const CURRENT_THREAD: isize = -2;
/// `NtCurrentProcess()`.
const CURRENT_PROCESS: isize = -1;
/// `THREAD_SET_CONTEXT`: the right `NtQueueApcThread` needs.
const THREAD_SET_CONTEXT: u32 = 0x0010;
const DUPLICATE_SAME_ACCESS: u32 = 0x2;
/// `STATUS_POSSIBLE_DEADLOCK`: a synchronous notification on a thread holding the loader lock.
const STATUS_POSSIBLE_DEADLOCK: NTSTATUS = 0xC000_0194u32 as i32;

/// A handle of this process duplicated into it through the unhooked `NtDuplicateObject`.
unsafe fn duplicate(real: &Real, h: isize, access: u32, options: u32) -> Result<isize, NTSTATUS> {
    let Some(dup) = real.dup else {
        return Err(STATUS_UNSUCCESSFUL);
    };
    let mut out: HANDLE = core::ptr::null_mut();
    let st = dup(
        CURRENT_PROCESS as HANDLE,
        h as HANDLE,
        CURRENT_PROCESS as HANDLE,
        &mut out,
        access,
        0,
        options,
    );
    if st < 0 { Err(st) } else { Ok(out as isize) }
}

/// Whether this thread holds the loader lock (`PEB->LoaderLock`). A synchronous wait there
/// could never end if the notifier has to be started: a new thread cannot run until the lock is
/// released.
fn holds_loader_lock() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        type Locked = unsafe extern "system" fn(*mut c_void) -> u32;
        static F: OnceLock<Option<Locked>> = OnceLock::new();
        let f = F.get_or_init(|| {
            use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
            // SAFETY: an ntdll export cast to its documented signature.
            unsafe {
                let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
                GetProcAddress(ntdll, c"RtlIsCriticalSectionLockedByThread".as_ptr().cast()).map(
                    |p| core::mem::transmute::<unsafe extern "system" fn() -> isize, Locked>(p),
                )
            }
        });
        let Some(f) = f else {
            return false;
        };
        // SAFETY: x64 `gs:[0x60]` is the PEB, whose `LoaderLock` (offset 0x110) is the loader's
        // critical section; both are valid for the life of the process.
        unsafe {
            let peb: *const u8;
            core::arch::asm!("mov {}, gs:[0x60]", out(reg) peb, options(nostack, readonly));
            let lock = *(peb.add(0x110) as *const *mut c_void);
            !lock.is_null() && f(lock) != 0
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    false
}

/// A synchronous notification: block until the waiter completes, and answer its status.
unsafe fn wait_sync(real: &Real, h: isize, path: String, a: &Args, since: Option<u64>) -> NTSTATUS {
    use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
    if holds_loader_lock() {
        return STATUS_POSSIBLE_DEADLOCK;
    }
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
    crate::ntbuf::iosb_set(a.iosb, st, 0);
    st
}

/// Add a waiter, and start the notifier if it is not running. The notifier is started under the
/// table lock (creating a thread takes no lock of ours), so no other registration can slip in
/// while it is being started: if it cannot be started, this waiter is taken out again and the
/// call fails, and no waiter is ever left with no thread to complete it. On failure the
/// waiter's `Done` comes back for the caller to release.
fn register(
    h: isize,
    path: String,
    subtree: bool,
    since: Option<u64>,
    done: Done,
) -> Result<(), (NTSTATUS, Done)> {
    let Ok(mut s) = STATE.lock() else {
        return Err((STATUS_UNSUCCESSFUL, done));
    };
    if s.waiters.len() >= MAX_WAITERS {
        return Err((STATUS_INSUFFICIENT_RESOURCES, done));
    }
    if !s.running {
        if !spawn_notifier() {
            note_reg_notify(RegNotify::PollError);
            return Err((STATUS_INSUFFICIENT_RESOURCES, done));
        }
        s.running = true;
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
    drop(s);
    note_reg_notify(RegNotify::Registered);
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
            // Counted before the caller can see it, so a count read after the wake includes it.
            note_reg_notify(RegNotify::Completed);
            // SAFETY: the waiter's caller registered these handles and addresses for this.
            unsafe { complete(&real, w.done, STATUS_NOTIFY_ENUM_DIR) };
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
        note_reg_notify(RegNotify::CleanedUp);
        // SAFETY: as in the notifier.
        unsafe { complete(&real, w.done, STATUS_NOTIFY_CLEANUP) };
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
            crate::ntbuf::iosb_set(iosb as *mut c_void, status, 0);
            if event != 0 {
                (api.set_event)(event as HANDLE, core::ptr::null_mut());
            }
            if apc != 0 && thread != 0 {
                (api.queue_apc)(thread as HANDLE, apc as *const c_void, apc_ctx, iosb, 0);
            }
            for owned in [event, thread] {
                if owned != 0 {
                    regkeys::close_real(real, owned);
                }
            }
        }
    }
}

/// Release what a waiter that was never registered owns.
unsafe fn release(real: &Real, done: Done) {
    if let Done::Async { event, thread, .. } = done {
        for owned in [event, thread] {
            if owned != 0 {
                regkeys::close_real(real, owned);
            }
        }
    }
}

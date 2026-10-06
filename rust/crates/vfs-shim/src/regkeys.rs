//! Registry key handles, path resolution, and the open/create decision (registry overlay spec,
//! sections 2.2, 3.1, 3.2 and 6).
//!
//! **Two handle tables.**
//! - *Synthetic* key handles carry [`REG_TAG`] (2^46), between `zipserve`'s section tag (2^45)
//!   and `fuse_synth`'s file tag (2^47), and are never kernel objects. Each holds the key's
//!   canonical path, the access the caller was granted, and the shim's own private handle to the
//!   real key (opened read-only through the unhooked `NtOpenKeyEx`) when one exists. Queries on
//!   them merge that real key with the overlay node (Task 10).
//! - *Pass-through* handles are the real handles the caller got back, recorded with their
//!   canonical path and requested access so writes through them can be caught later (Task 11).
//!
//! Both tables hold exactly the live handles: `NtClose` drops the record (and closes a synthetic
//! handle's private real handle), `NtDuplicateObject` with `DUPLICATE_CLOSE_SOURCE` drops the
//! source, and a duplicate of a tracked handle is tracked.
//!
//! **Writes never reach the real registry** (section 6). Nothing here calls the real
//! `NtCreateKey`: a create of a key that already exists for real is answered with a
//! pass-through *open* of that key, and a create of a key that does not is sent to the
//! director as `REG_CREATE_KEY`. When the director cannot be asked, a read falls back to the
//! real key and a create that would need the director fails with `STATUS_UNSUCCESSFUL`.
//!
//! Nothing in this module runs when the registry overlay is off: the hooks check
//! [`crate::regclient::enabled`] first and go straight to the trampoline.
#![allow(unsafe_code)]

use core::ffi::c_void;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, TryLockError};

use vfs_protocol::{ST_BAD_REQUEST, ST_EXISTS};
use vfs_registry::path::{self, PathError};
use vfs_registry::Lookup;
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};

use crate::ntdef::{
    NtCloseFn, NtDuplicateObjectFn, NtEnumerateKeyFn, NtEnumerateValueKeyFn, NtOpenKeyExFn,
    NtQueryKeyFn, NtQueryMultipleValueKeyFn, NtQueryObjectFn, NtQueryValueKeyFn, ObjectAttributes,
    UnicodeString, DUPLICATE_CLOSE_SOURCE, DUPLICATE_SAME_ACCESS, DUPLICATE_SAME_ATTRIBUTES,
    KEY_NAME_INFORMATION, OBJECT_BASIC_INFORMATION, OBJECT_HANDLE_FLAG_INFORMATION,
    OBJECT_TYPE_INFORMATION, OBJ_CASE_INSENSITIVE, REG_CREATED_NEW_KEY, REG_OPENED_EXISTING_KEY,
    REG_OPTION_BACKUP_RESTORE, REG_OPTION_CREATE_LINK, REG_OPTION_OPEN_LINK, REG_OPTION_VOLATILE,
    STATUS_ACCESS_DENIED, STATUS_BUFFER_OVERFLOW, STATUS_BUFFER_TOO_SMALL,
    STATUS_INFO_LENGTH_MISMATCH, STATUS_INVALID_HANDLE, STATUS_INVALID_PARAMETER,
    STATUS_NOT_SUPPORTED, STATUS_OBJECT_NAME_INVALID, STATUS_OBJECT_NAME_NOT_FOUND,
    STATUS_OBJECT_PATH_NOT_FOUND, STATUS_SUCCESS, STATUS_UNSUCCESSFUL,
};

/// Tag bit of a synthetic key handle. Real kernel handles never reach this magnitude; the sign
/// bit stays clear, so the value is never a pseudo-handle.
pub const REG_TAG: usize = 1 << 46;
/// Slot bits below the tag (shifted left by 2, so handles stay multiples of 4 as kernel handles
/// are). Kept clear of 2^45 so a key handle never reads as a synthetic section.
const SLOT_MASK: usize = (1 << 42) - 1;

/// `STATUS_ACCESS_VIOLATION`, for a NULL `KeyHandle` out-pointer.
const STATUS_ACCESS_VIOLATION: NTSTATUS = 0xC000_0005u32 as i32;

// Key access rights.
pub const KEY_QUERY_VALUE: u32 = 0x0001;
pub const KEY_SET_VALUE: u32 = 0x0002;
pub const KEY_CREATE_SUB_KEY: u32 = 0x0004;
pub const KEY_ENUMERATE_SUB_KEYS: u32 = 0x0008;
pub const KEY_NOTIFY: u32 = 0x0010;
pub const KEY_CREATE_LINK: u32 = 0x0020;
pub const KEY_WOW64_64KEY: u32 = 0x0100;
pub const KEY_WOW64_32KEY: u32 = 0x0200;
pub const WOW64_MASK: u32 = KEY_WOW64_64KEY | KEY_WOW64_32KEY;
const DELETE: u32 = 0x0001_0000;
const READ_CONTROL: u32 = 0x0002_0000;
const WRITE_DAC: u32 = 0x0004_0000;
const WRITE_OWNER: u32 = 0x0008_0000;
const MAXIMUM_ALLOWED: u32 = 0x0200_0000;
const GENERIC_ALL: u32 = 0x1000_0000;
const GENERIC_EXECUTE: u32 = 0x2000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const GENERIC_READ: u32 = 0x8000_0000;
pub const KEY_READ: u32 = READ_CONTROL | KEY_QUERY_VALUE | KEY_ENUMERATE_SUB_KEYS | KEY_NOTIFY;
pub const KEY_WRITE: u32 = READ_CONTROL | KEY_SET_VALUE | KEY_CREATE_SUB_KEY;
pub const KEY_ALL_ACCESS: u32 = 0x000F_003F;

/// A synthetic key handle's record.
#[derive(Clone, Debug)]
pub struct SynthKey {
    /// Canonical path (spec 2.2).
    pub path: String,
    /// The access the caller was granted, generic rights mapped to key rights
    /// ([`map_generic`]); WOW64 flags are not access and are dropped.
    pub access: u32,
    /// The shim's private read-only handle to the real key, when there is one to merge.
    /// `None` for a key created here (no real counterpart may show through).
    pub real: Option<isize>,
    /// The access exactly as the caller asked for it, WOW64 flags included: what a private real
    /// handle for this key is opened with again ([`open_private`]), for a duplicate.
    pub requested: u32,
    /// Handle attributes (`OBJ_INHERIT`), as `NtQueryObject` reports them.
    pub attributes: u32,
}

/// `OBJ_INHERIT`: the only handle attribute a key handle keeps.
pub const OBJ_INHERIT: u32 = 0x2;

/// A pass-through (real) key handle's record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyRec {
    /// Canonical path (spec 2.2).
    pub path: String,
    /// Requested access, generic rights mapped ([`map_generic`]). For `MAXIMUM_ALLOWED` this is
    /// [`KEY_ALL_ACCESS`], an upper bound on what the kernel granted.
    pub access: u32,
}

static SYNTH: Mutex<BTreeMap<isize, SynthKey>> = Mutex::new(BTreeMap::new());
static PASS: Mutex<BTreeMap<isize, KeyRec>> = Mutex::new(BTreeMap::new());
static NEXT_SLOT: AtomicUsize = AtomicUsize::new(1);

/// Whether `h` is a synthetic key handle.
pub fn is_synthetic(h: isize) -> bool {
    h > 0 && (h as usize) & REG_TAG != 0
}

/// Lock a table for a removal on the close path. Never blocks for good: a thread killed while
/// holding a `std::sync::Mutex` leaves it locked and not poisoned (see `close_hook_body`), so
/// this spins a bounded number of times and then gives up. A record lost that way belongs to a
/// handle that is going away.
pub(crate) fn lock_for_close<T>(m: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    for _ in 0..10_000 {
        match m.try_lock() {
            Ok(g) => return Some(g),
            Err(TryLockError::Poisoned(_)) => break,
            Err(TryLockError::WouldBlock) => std::thread::yield_now(),
        }
    }
    crate::hookstats::note_reg_close_lock_given_up();
    None
}

/// Register a synthetic key handle and return its value.
pub fn insert_synthetic(rec: SynthKey) -> Option<isize> {
    let slot = NEXT_SLOT.fetch_add(1, Ordering::Relaxed) & SLOT_MASK;
    let h = (REG_TAG | (slot << 2)) as isize;
    SYNTH.lock().ok()?.insert(h, rec);
    Some(h)
}

/// The record of a synthetic key handle.
pub fn synthetic(h: isize) -> Option<SynthKey> {
    if !is_synthetic(h) {
        return None;
    }
    SYNTH.lock().ok()?.get(&h).cloned()
}

/// Remove a synthetic key handle's record (the caller closes its private real handle).
fn remove_synthetic(h: isize) -> Option<SynthKey> {
    lock_for_close(&SYNTH)?.remove(&h)
}

/// Record a pass-through key handle.
pub fn track(h: isize, rec: KeyRec) {
    if let Ok(mut t) = PASS.lock() {
        t.insert(h, rec);
    }
}

/// The record of a pass-through key handle.
pub fn tracked(h: isize) -> Option<KeyRec> {
    PASS.lock().ok()?.get(&h).cloned()
}

fn untrack(h: isize) {
    if let Some(mut t) = lock_for_close(&PASS) {
        t.remove(&h);
    }
    forget_not_ours(h);
}

/// The canonical path of a key handle from either table.
pub fn path_of(h: isize) -> Option<String> {
    if is_synthetic(h) {
        return synthetic(h).map(|k| k.path);
    }
    tracked(h).map(|r| r.path)
}

/// Live (synthetic, pass-through) key handles.
pub fn counts() -> (usize, usize) {
    (
        SYNTH.lock().map(|t| t.len()).unwrap_or(0),
        PASS.lock().map(|t| t.len()).unwrap_or(0),
    )
}

/// Generic rights mapped to key rights (the registry's generic mapping); `MAXIMUM_ALLOWED`
/// reads as everything. WOW64 flags are dropped: they select a view, they grant nothing.
pub fn map_generic(access: u32) -> u32 {
    let mut a = access & !(GENERIC_ALL | GENERIC_EXECUTE | GENERIC_WRITE | GENERIC_READ);
    a &= !(MAXIMUM_ALLOWED | WOW64_MASK);
    if access & (GENERIC_READ | GENERIC_EXECUTE) != 0 {
        a |= KEY_READ;
    }
    if access & GENERIC_WRITE != 0 {
        a |= KEY_WRITE;
    }
    if access & (GENERIC_ALL | MAXIMUM_ALLOWED) != 0 {
        a |= KEY_ALL_ACCESS;
    }
    a
}

/// The access includes a right that changes the key (spec 3.2 "Desired access").
pub fn wants_write(access: u32) -> bool {
    map_generic(access)
        & (KEY_SET_VALUE | KEY_CREATE_SUB_KEY | KEY_CREATE_LINK | DELETE | WRITE_DAC | WRITE_OWNER)
        != 0
}

/// String form (`S-1-5-21-...`) of a binary SID.
pub fn sid_string(sid: &[u8]) -> Option<String> {
    let (&rev, rest) = sid.split_first()?;
    let (&n, rest) = rest.split_first()?;
    let auth: &[u8; 6] = rest.get(..6)?.try_into().ok()?;
    let subs = rest.get(6..6 + 4 * n as usize)?;
    let auth = auth.iter().fold(0u64, |a, b| (a << 8) | *b as u64);
    let mut s = format!("S-{rev}-{auth}");
    for c in subs.as_chunks::<4>().0 {
        s.push_str(&format!("-{}", u32::from_le_bytes(*c)));
    }
    Some(s)
}

/// The process user's SID as a string, read once from the process token.
pub fn user_sid() -> Option<&'static str> {
    static SID: OnceLock<Option<String>> = OnceLock::new();
    SID.get_or_init(read_user_sid).as_deref()
}

fn read_user_sid() -> Option<String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{
        GetLengthSid, GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    // SAFETY: plain token queries into owned buffers; the SID pointer points into `buf`, and its
    // length is checked against `buf` before it is read.
    unsafe {
        let mut token: HANDLE = core::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let mut buf = vec![0u64; 64]; // 512 bytes, 8-aligned for TOKEN_USER
        let mut need = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr().cast(),
            (buf.len() * 8) as u32,
            &mut need,
        );
        CloseHandle(token);
        if ok == 0 {
            return None;
        }
        let tu = &*(buf.as_ptr() as *const TOKEN_USER);
        let sid = tu.User.Sid;
        let base = buf.as_ptr() as usize;
        let len = GetLengthSid(sid) as usize;
        let off = (sid as usize).checked_sub(base)?;
        if off + len > buf.len() * 8 {
            return None;
        }
        sid_string(core::slice::from_raw_parts(sid as *const u8, len))
    }
}

/// Where a key name resolved to.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolved {
    /// A canonical path.
    Path(String),
    /// An absolute name that is not a registry path the overlay knows about: pass it through.
    NotOurs,
    /// A relative name with an empty, `.` or `..` component.
    Invalid,
}

/// The canonical path of `name` relative to `base` (a canonical path, or `None` for an absolute
/// name). An empty relative name is the base key itself.
pub fn compose(base: Option<&str>, name: &str, sid: Option<&str>) -> Resolved {
    match base {
        None => match path::canonical(name, sid) {
            Ok(p) => Resolved::Path(p),
            Err(PathError::NotRegistry) => Resolved::NotOurs,
            Err(PathError::BadComponent) => Resolved::Invalid,
        },
        Some(b) if name.is_empty() => Resolved::Path(b.to_string()),
        Some(b) => match path::join(b, name.strip_suffix('\\').unwrap_or(name)) {
            Ok(p) => Resolved::Path(p),
            Err(_) => Resolved::Invalid,
        },
    }
}

/// The unhooked entry points the key logic calls (each hook's trampoline).
pub struct Real {
    pub open_ex: Option<NtOpenKeyExFn>,
    pub query: Option<NtQueryKeyFn>,
    pub close: Option<NtCloseFn>,
    pub dup: Option<NtDuplicateObjectFn>,
    pub enum_key: Option<NtEnumerateKeyFn>,
    pub query_value: Option<NtQueryValueKeyFn>,
    pub enum_value: Option<NtEnumerateValueKeyFn>,
    pub query_multiple: Option<NtQueryMultipleValueKeyFn>,
    /// The unhooked `NtQueryObject`, for the access a real handle was granted.
    pub query_object: Option<NtQueryObjectFn>,
}

/// An absolute `OBJECT_ATTRIBUTES` the shim builds for its own opens. Boxed: the attributes
/// point at the string, which points at the buffer.
struct AbsName {
    buf: Vec<u16>,
    us: UnicodeString,
    oa: ObjectAttributes,
}

impl AbsName {
    /// `path` (canonical) as the absolute NT name, with `template`'s attributes, security
    /// descriptor and QoS when given.
    fn new(canonical: &str, template: Option<&ObjectAttributes>) -> Box<AbsName> {
        let buf: Vec<u16> = path::to_nt(canonical, user_sid()).encode_utf16().collect();
        let bytes = (buf.len() * 2).min(u16::MAX as usize & !1) as u16;
        let mut b = Box::new(AbsName {
            buf,
            us: UnicodeString {
                length: bytes,
                maximum_length: bytes,
                buffer: core::ptr::null_mut(),
            },
            oa: ObjectAttributes {
                length: core::mem::size_of::<ObjectAttributes>() as u32,
                root_directory: core::ptr::null_mut(),
                object_name: core::ptr::null(),
                attributes: OBJ_CASE_INSENSITIVE,
                security_descriptor: core::ptr::null(),
                security_qos: core::ptr::null(),
            },
        });
        b.us.buffer = b.buf.as_mut_ptr();
        b.oa.object_name = &b.us;
        if let Some(t) = template {
            b.oa.attributes = t.attributes | OBJ_CASE_INSENSITIVE;
            b.oa.security_descriptor = t.security_descriptor;
            b.oa.security_qos = t.security_qos;
        }
        b
    }
}

/// The name `NtQueryKey(KeyNameInformation)` reports for a real key handle.
unsafe fn real_key_name(real: &Real, h: isize) -> Option<String> {
    let q = real.query?;
    let mut buf = vec![0u32; 256];
    for _ in 0..3 {
        let mut need = 0u32;
        let st = q(
            h as HANDLE,
            KEY_NAME_INFORMATION,
            buf.as_mut_ptr().cast(),
            (buf.len() * 4) as u32,
            &mut need,
        );
        if (st == STATUS_BUFFER_OVERFLOW || st == STATUS_BUFFER_TOO_SMALL)
            && need as usize > buf.len() * 4
        {
            buf = vec![0u32; (need as usize).div_ceil(4)];
            continue;
        }
        if st < 0 {
            return None;
        }
        let n = buf[0] as usize;
        if !n.is_multiple_of(2) || 4 + n > buf.len() * 4 {
            return None;
        }
        let units =
            core::slice::from_raw_parts((buf.as_ptr() as *const u8).add(4) as *const u16, n / 2);
        return Some(String::from_utf16_lossy(units));
    }
    None
}

/// Handles [`resolve_handle`] found are not keys the overlay serves (not a key, or a key outside
/// `\Registry\Machine` and `\Registry\User`), so asking again costs one lookup here instead of
/// a syscall. Bounded: it starts over when full. An entry goes with its handle's `NtClose` (or
/// `DUPLICATE_CLOSE_SOURCE`), so a recycled handle value never inherits it.
static NOT_OURS: Mutex<BTreeSet<isize>> = Mutex::new(BTreeSet::new());
static NOT_OURS_COUNT: AtomicUsize = AtomicUsize::new(0);
const MAX_NOT_OURS: usize = 1024;

fn note_not_ours(h: isize) {
    if let Ok(mut t) = NOT_OURS.lock() {
        if t.len() >= MAX_NOT_OURS {
            t.clear();
        }
        t.insert(h);
        NOT_OURS_COUNT.store(t.len(), Ordering::Relaxed);
    }
}

fn is_not_ours(h: isize) -> bool {
    NOT_OURS_COUNT.load(Ordering::Relaxed) != 0 && NOT_OURS.lock().is_ok_and(|t| t.contains(&h))
}

/// Handles remembered as not ours. For tests and diagnostics.
pub fn not_ours_count() -> usize {
    NOT_OURS.lock().map_or(0, |t| t.len())
}

/// Forget that `h` was not ours: its handle value is being closed.
fn forget_not_ours(h: isize) {
    if NOT_OURS_COUNT.load(Ordering::Relaxed) == 0 {
        return;
    }
    if let Some(mut t) = lock_for_close(&NOT_OURS) {
        t.remove(&h);
        NOT_OURS_COUNT.store(t.len(), Ordering::Relaxed);
    }
}

/// The access a real handle was granted, from `NtQueryObject(ObjectBasicInformation)`.
unsafe fn granted_access(real: &Real, h: isize) -> Option<u32> {
    let q = real.query_object?;
    let mut buf = [0u32; 14]; // OBJECT_BASIC_INFORMATION, 56 bytes
    let mut need = 0u32;
    let st = q(
        h as HANDLE,
        OBJECT_BASIC_INFORMATION,
        buf.as_mut_ptr().cast(),
        56,
        &mut need,
    );
    // Attributes@0, GrantedAccess@4.
    (st >= 0).then_some(buf[1])
}

/// The record of a real key handle the overlay serves: from the pass-through table, or, for a
/// handle neither table holds (opened before the hooks, or handed in from elsewhere), resolved
/// once from the real key's name and the access the kernel granted it, and recorded as
/// pass-through from then on. `None` for a synthetic handle, a handle that is not a key, or a
/// key outside the virtualised hives (remembered, so the next ask is cheap).
///
/// # Safety
/// `h` is a caller's handle; it is only passed to the real `NtQueryKey` and `NtQueryObject`.
pub unsafe fn resolve_handle(real: &Real, h: isize) -> Option<KeyRec> {
    if is_synthetic(h) || h <= 0 {
        return None;
    }
    if let Some(r) = tracked(h) {
        return Some(r);
    }
    if is_not_ours(h) {
        return None;
    }
    let path = real_key_name(real, h)
        .and_then(|nt| path::canonical(&nt, user_sid()).ok())
        .filter(|p| path::is_virtualised(p));
    let Some(path) = path else {
        note_not_ours(h);
        return None;
    };
    let rec = KeyRec {
        path,
        // Not readable (no `NtQueryObject` trampoline): the kernel still checks every real call.
        access: granted_access(real, h).unwrap_or(KEY_ALL_ACCESS),
    };
    track(h, rec.clone());
    Some(rec)
}

/// The canonical path of a root key handle: from the tables, else from the real key's name.
/// `Err(())` when the handle is not something this can name.
unsafe fn root_path(real: &Real, root: isize) -> Result<String, ()> {
    if is_synthetic(root) {
        return synthetic(root).map(|k| k.path).ok_or(());
    }
    if let Some(r) = tracked(root) {
        return Ok(r.path);
    }
    let nt = real_key_name(real, root).ok_or(())?;
    path::canonical(&nt, user_sid()).map_err(|_| ())
}

/// The shim's private read-only handle to the real key at `path`, opened `KEY_READ` with the
/// WOW64 flags of `access` (the caller's access). A key that refuses `KEY_READ` is tried again
/// with only the read rights the caller itself asked for, which it may grant. `Err` is the
/// open's status.
pub(crate) unsafe fn open_private(
    real: &Real,
    canonical: &str,
    access: u32,
) -> Result<isize, NTSTATUS> {
    let Some(open) = real.open_ex else {
        return Err(STATUS_UNSUCCESSFUL);
    };
    let name = AbsName::new(canonical, None);
    let wow64 = access & WOW64_MASK;
    let try_open = |rights: u32| {
        let mut h: HANDLE = core::ptr::null_mut();
        let st = open(&mut h, rights | wow64, &name.oa, 0);
        if st < 0 {
            Err(st)
        } else {
            Ok(h as isize)
        }
    };
    match try_open(KEY_READ) {
        Err(STATUS_ACCESS_DENIED) => {
            let reads = map_generic(access) & KEY_READ;
            if reads != 0 && reads != KEY_READ {
                try_open(reads)
            } else {
                Err(STATUS_ACCESS_DENIED)
            }
        }
        r => r,
    }
}

fn not_found(st: NTSTATUS) -> bool {
    st == STATUS_OBJECT_NAME_NOT_FOUND || st == STATUS_OBJECT_PATH_NOT_FOUND
}

pub(crate) unsafe fn close_real(real: &Real, h: isize) {
    if let Some(c) = real.close {
        c(h as HANDLE);
    }
}

/// Whether the real key at `path` exists (a key that refuses even a read-only open exists).
unsafe fn real_exists(real: &Real, canonical: &str, wow64: u32) -> bool {
    match open_private(real, canonical, wow64 & WOW64_MASK) {
        Ok(h) => {
            close_real(real, h);
            true
        }
        Err(st) => st == STATUS_ACCESS_DENIED,
    }
}

/// Whether `path` lies below a key created here, which hides every real key under it.
///
/// The overlay keeps every descendant of a created-here node created-here too (a node is
/// created-here when its parent is, and reviving a tombstoned key drops its old subtree), so
/// the nearest ancestor the overlay holds decides: created-here or not. Lookups are cached; a
/// failed one ends the walk as "no" (the real registry is the fallback for reads).
fn below_created(canonical: &str) -> bool {
    let mut cur = path::parent(canonical);
    while let Some(p) = cur {
        match crate::regclient::lookup(p) {
            Ok((Lookup::Present { created }, _)) => return created,
            Ok((Lookup::Tombstoned, _)) => return true,
            Ok((Lookup::Absent, _)) => cur = path::parent(p),
            Err(_) => return false,
        }
    }
    false
}

/// Whether the parent of `path` exists in the merged view, as `NtCreateKey` requires: a node
/// in the overlay, or a real key that is neither tombstoned nor hidden below a key created here.
unsafe fn parent_exists(real: &Real, canonical: &str, wow64: u32) -> Result<bool, NTSTATUS> {
    let Some(parent) = path::parent(canonical) else {
        return Ok(false);
    };
    match crate::regclient::lookup(parent) {
        // An overlay node exists in the merged view whatever the real key is, as `virtual_open`
        // has it.
        Ok((Lookup::Present { .. }, _)) => Ok(true),
        Ok((Lookup::Tombstoned, _)) => Ok(false),
        Ok((Lookup::Absent, _)) => Ok(!below_created(parent) && real_exists(real, parent, wow64)),
        Err(_) => Err(STATUS_UNSUCCESSFUL),
    }
}

/// Which call is being answered.
#[derive(Clone, Copy)]
pub enum Call {
    /// `NtOpenKey` / `NtOpenKeyEx`.
    Open,
    /// `NtCreateKey` with its `CreateOptions`.
    Create { options: u32 },
}

/// What an open or create comes to.
pub struct Outcome {
    pub status: NTSTATUS,
    /// For `NtCreateKey`'s `Disposition`, on success.
    pub disposition: u32,
}

impl Outcome {
    fn fail(status: NTSTATUS) -> Outcome {
        Outcome {
            status,
            disposition: 0,
        }
    }
}

/// `NtOpenKey`, `NtOpenKeyEx` and `NtCreateKey` with the overlay on.
///
/// `pass` makes the real open the caller asked for (the hook's own trampoline for an open; for
/// a create, the open trampoline with the create's open options), with the
/// `OBJECT_ATTRIBUTES` it is given. The decision (spec 3.2):
/// - the path is not under `\Registry\Machine` or `\Registry\User`, or its root cannot be
///   named: pass through (an unnamed root is counted);
/// - the overlay has nothing at or below it: the real handle, recorded as pass-through. A real
///   open refused for write access becomes a synthetic handle over a read-only private open;
/// - otherwise a synthetic handle, or `STATUS_OBJECT_NAME_NOT_FOUND` for an open of a
///   tombstoned key or one hidden below a key created here;
/// - a create of a key with no real counterpart (or a tombstoned one) is `REG_CREATE_KEY`.
///
/// # Safety
/// `out` and `oa` are the caller's NT arguments.
pub unsafe fn open_or_create(
    real: &Real,
    out: *mut HANDLE,
    access: u32,
    oa: *const ObjectAttributes,
    call: Call,
    pass: &mut dyn FnMut(*const ObjectAttributes) -> NTSTATUS,
) -> Outcome {
    let passthrough = |pass: &mut dyn FnMut(*const ObjectAttributes) -> NTSTATUS| Outcome {
        status: pass(oa),
        disposition: REG_OPENED_EXISTING_KEY,
    };
    if oa.is_null() || (*oa).object_name.is_null() {
        return passthrough(pass);
    }
    let oa_ref = &*oa;
    let us = &*oa_ref.object_name;
    let name = if us.buffer.is_null() || us.length == 0 {
        String::new()
    } else {
        String::from_utf16_lossy(core::slice::from_raw_parts(
            us.buffer,
            us.length as usize / 2,
        ))
    };
    let root = oa_ref.root_directory as isize;
    let root_synth = is_synthetic(root);
    let base = if root == 0 {
        None
    } else {
        match root_path(real, root) {
            Ok(p) => Some(p),
            Err(()) if root_synth => return Outcome::fail(STATUS_INVALID_HANDLE),
            Err(()) => {
                crate::hookstats::note_reg_unresolved();
                return passthrough(pass);
            }
        }
    };
    let canonical = match compose(base.as_deref(), &name, user_sid()) {
        Resolved::Path(p) => p,
        Resolved::NotOurs => return passthrough(pass),
        Resolved::Invalid if root_synth => return Outcome::fail(STATUS_OBJECT_NAME_INVALID),
        Resolved::Invalid => {
            crate::hookstats::note_reg_unresolved();
            return passthrough(pass);
        }
    };
    if !path::is_virtualised(&canonical) {
        // A synthetic key is always virtualised, and so is everything below it.
        return passthrough(pass);
    }
    if out.is_null() {
        return Outcome::fail(STATUS_ACCESS_VIOLATION);
    }
    let k = Key {
        real,
        out,
        access,
        oa,
        root_synth,
        canonical,
    };
    match call {
        Call::Open => k.open(pass),
        Call::Create { options } => k.create(options, pass),
    }
}

/// One resolved open or create.
struct Key<'a> {
    real: &'a Real,
    out: *mut HANDLE,
    access: u32,
    oa: *const ObjectAttributes,
    root_synth: bool,
    canonical: String,
}

impl Key<'_> {
    fn wow64(&self) -> u32 {
        self.access & WOW64_MASK
    }

    /// The real open through `pass`. A synthetic root is not a kernel handle, so the real call
    /// gets the absolute name instead. A success is recorded as pass-through.
    unsafe fn real_open(
        &self,
        pass: &mut dyn FnMut(*const ObjectAttributes) -> NTSTATUS,
    ) -> NTSTATUS {
        let st = if self.root_synth {
            let abs = AbsName::new(&self.canonical, Some(&*self.oa));
            pass(&abs.oa)
        } else {
            pass(self.oa)
        };
        if st >= 0 {
            track(
                *self.out as isize,
                KeyRec {
                    path: self.canonical.clone(),
                    access: map_generic(self.access),
                },
            );
        }
        st
    }

    /// Hand the caller a synthetic handle over `real_key`.
    unsafe fn synthetic(&self, real_key: Option<isize>, disposition: u32) -> Outcome {
        match insert_synthetic(SynthKey {
            path: self.canonical.clone(),
            access: map_generic(self.access),
            real: real_key,
            requested: self.access,
            attributes: (*self.oa).attributes & OBJ_INHERIT,
        }) {
            Some(h) => {
                *self.out = h as HANDLE;
                Outcome {
                    status: STATUS_SUCCESS,
                    disposition,
                }
            }
            None => {
                if let Some(r) = real_key {
                    close_real(self.real, r);
                }
                Outcome::fail(STATUS_UNSUCCESSFUL)
            }
        }
    }

    /// The real open, with the write-access fallback of spec 3.2: refused for write access, the
    /// key is opened read-only and the caller gets a synthetic handle with the access it asked
    /// for (its writes go to the overlay).
    unsafe fn pass_or_write_fallback(
        &self,
        pass: &mut dyn FnMut(*const ObjectAttributes) -> NTSTATUS,
    ) -> NTSTATUS {
        let st = self.real_open(pass);
        if st != STATUS_ACCESS_DENIED || !wants_write(self.access) {
            return st;
        }
        match open_private(self.real, &self.canonical, self.access) {
            Ok(r) => self.synthetic(Some(r), REG_OPENED_EXISTING_KEY).status,
            Err(_) => st,
        }
    }

    /// A key the overlay touches: synthetic, merged with the real key when it has one. `Err`
    /// when it does not exist in the merged view (or the real key refuses even a read).
    unsafe fn virtual_open(&self, created: Option<bool>) -> Result<Outcome, NTSTATUS> {
        let real_key = match created {
            // Created here: no real key may show through.
            Some(true) => None,
            // Overlays a real key, which may be gone; the node exists either way.
            Some(false) => match open_private(self.real, &self.canonical, self.access) {
                Ok(r) => Some(r),
                Err(st) if st == STATUS_ACCESS_DENIED => return Err(st),
                Err(_) => None,
            },
            // Only something below it is in the overlay: the real key must exist.
            None => {
                if below_created(&self.canonical) {
                    return Err(STATUS_OBJECT_NAME_NOT_FOUND);
                }
                Some(open_private(self.real, &self.canonical, self.access)?)
            }
        };
        Ok(self.synthetic(real_key, REG_OPENED_EXISTING_KEY))
    }

    unsafe fn open(&self, pass: &mut dyn FnMut(*const ObjectAttributes) -> NTSTATUS) -> Outcome {
        let ok = |status| Outcome {
            status,
            disposition: REG_OPENED_EXISTING_KEY,
        };
        match crate::regclient::lookup(&self.canonical) {
            // The director did not answer (counted by `regclient`): the real key alone.
            Err(_) => ok(self.real_open(pass)),
            Ok((Lookup::Tombstoned, _)) => Outcome::fail(STATUS_OBJECT_NAME_NOT_FOUND),
            Ok((Lookup::Absent, false)) => {
                if below_created(&self.canonical) {
                    return Outcome::fail(STATUS_OBJECT_NAME_NOT_FOUND);
                }
                ok(self.pass_or_write_fallback(pass))
            }
            Ok((Lookup::Absent, true)) => self.virtual_open(None).unwrap_or_else(Outcome::fail),
            Ok((Lookup::Present { created }, _)) => self
                .virtual_open(Some(created))
                .unwrap_or_else(Outcome::fail),
        }
    }

    unsafe fn create(
        &self,
        options: u32,
        pass: &mut dyn FnMut(*const ObjectAttributes) -> NTSTATUS,
    ) -> Outcome {
        let existing = |status| Outcome {
            status,
            disposition: REG_OPENED_EXISTING_KEY,
        };
        match crate::regclient::lookup(&self.canonical) {
            // The director did not answer: only a pass-through open of a key that exists for
            // real is possible; anything that would need `REG_CREATE_KEY` fails.
            Err(_) => {
                let st = self.real_open(pass);
                if not_found(st) {
                    Outcome::fail(STATUS_UNSUCCESSFUL)
                } else {
                    existing(st)
                }
            }
            Ok((Lookup::Tombstoned, _)) => self.overlay_create(options),
            Ok((Lookup::Absent, false)) => {
                if below_created(&self.canonical) {
                    return self.overlay_create(options);
                }
                let st = self.pass_or_write_fallback(pass);
                if not_found(st) {
                    self.overlay_create(options)
                } else {
                    existing(st)
                }
            }
            Ok((Lookup::Absent, true)) => match self.virtual_open(None) {
                Ok(o) => o,
                Err(st) if not_found(st) => self.overlay_create(options),
                Err(st) => Outcome::fail(st),
            },
            Ok((Lookup::Present { created }, _)) => self
                .virtual_open(Some(created))
                .unwrap_or_else(Outcome::fail),
        }
    }

    /// `REG_CREATE_KEY`: a key created here, under a parent that exists in the merged view.
    unsafe fn overlay_create(&self, options: u32) -> Outcome {
        if options & REG_OPTION_CREATE_LINK != 0 {
            // Symbolic links are not modelled by the overlay.
            return Outcome::fail(STATUS_NOT_SUPPORTED);
        }
        match parent_exists(self.real, &self.canonical, self.wow64()) {
            Ok(true) => {}
            Ok(false) => return Outcome::fail(STATUS_OBJECT_NAME_NOT_FOUND),
            Err(st) => return Outcome::fail(st),
        }
        let volatile = options & REG_OPTION_VOLATILE != 0;
        match crate::regclient::create_key(&self.canonical, volatile) {
            Ok(()) => self.synthetic(None, REG_CREATED_NEW_KEY),
            // Another thread or process created it first: it exists now.
            Err(ST_EXISTS) => match crate::regclient::lookup(&self.canonical) {
                Ok((Lookup::Present { created }, _)) => self
                    .virtual_open(Some(created))
                    .unwrap_or_else(Outcome::fail),
                _ => Outcome::fail(STATUS_UNSUCCESSFUL),
            },
            Err(ST_BAD_REQUEST) => Outcome::fail(STATUS_INVALID_PARAMETER),
            Err(_) => Outcome::fail(STATUS_UNSUCCESSFUL),
        }
    }
}

/// The open options `NtOpenKeyEx` accepts, out of `NtCreateKey`'s `CreateOptions`.
pub fn open_options_of_create(options: u32) -> u32 {
    options & (REG_OPTION_BACKUP_RESTORE | REG_OPTION_OPEN_LINK)
}

/// `NtClose` of a key handle. `Some(status)` for a synthetic handle (answered here, its
/// private real handle closed); `None` for anything else, whose pass-through record (if any)
/// is dropped before the caller closes it for real.
pub unsafe fn close(real: &Real, h: isize) -> Option<NTSTATUS> {
    // Whatever the handle was, an enumeration snapshot kept for it goes with it.
    crate::regquery::forget(h);
    if is_synthetic(h) {
        return Some(match remove_synthetic(h) {
            Some(k) => {
                if let Some(r) = k.real {
                    close_real(real, r);
                }
                STATUS_SUCCESS
            }
            None => STATUS_INVALID_HANDLE,
        });
    }
    untrack(h);
    None
}

/// The NT name of a synthetic key handle, for `NtQueryObject(ObjectNameInformation)`.
pub fn object_name(h: isize) -> Option<String> {
    synthetic(h).map(|k| path::to_nt(&k.path, user_sid()))
}

/// A real key handle to ask the host about a key's object type when a synthetic key has no
/// private real handle of its own: `\Registry\Machine`, opened read-only once for the process.
unsafe fn type_donor(real: &Real) -> Option<isize> {
    static DONOR: OnceLock<Option<isize>> = OnceLock::new();
    *DONOR.get_or_init(|| open_private(real, r"\Registry\Machine", 0).ok())
}

/// `NtQueryObject` on a synthetic key handle, for every class but the name (which the hook
/// answers with [`object_name`]).
/// - `ObjectTypeInformation`: the host's own answer for a real key handle (the key's private
///   one, else [`type_donor`]), so the layout, the `"Key"` name and the short-buffer rules are
///   the host's exactly.
/// - `ObjectBasicInformation`: the same, with `Attributes` and `GrantedAccess` replaced by this
///   handle's own.
/// - `ObjectHandleFlagInformation`: `Inherit` from the record, `ProtectFromClose` false.
///   `NtSetInformationObject` is not hooked, so neither flag can be changed on a synthetic
///   handle.
/// - anything else: the host's answer for the handle, which is `STATUS_INVALID_HANDLE`.
///
/// # Safety
/// The arguments are the caller's NT arguments; `tramp` is the unhooked `NtQueryObject`.
pub unsafe fn query_object(
    real: &Real,
    tramp: NtQueryObjectFn,
    h: isize,
    class: u32,
    info: *mut c_void,
    length: u32,
    ret_len: *mut u32,
) -> NTSTATUS {
    let Some(rec) = synthetic(h) else {
        return STATUS_INVALID_HANDLE;
    };
    match class {
        OBJECT_HANDLE_FLAG_INFORMATION => {
            if !ret_len.is_null() {
                core::ptr::write_unaligned(ret_len, 2);
            }
            if info.is_null() || length < 2 {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let p = info as *mut u8;
            *p = u8::from(rec.attributes & OBJ_INHERIT != 0);
            *p.add(1) = 0;
            STATUS_SUCCESS
        }
        OBJECT_BASIC_INFORMATION | OBJECT_TYPE_INFORMATION => {
            let Some(donor) = rec.real.or_else(|| type_donor(real)) else {
                return STATUS_UNSUCCESSFUL;
            };
            let st = tramp(donor as HANDLE, class, info, length, ret_len);
            if st >= 0 && class == OBJECT_BASIC_INFORMATION && length >= 8 {
                // `OBJECT_BASIC_INFORMATION` opens with `Attributes`, then `GrantedAccess`.
                let p = info as *mut u32;
                core::ptr::write_unaligned(p, rec.attributes & OBJ_INHERIT);
                core::ptr::write_unaligned(p.add(1), rec.access);
            }
            st
        }
        _ => tramp(h as HANDLE, class, info, length, ret_len),
    }
}

/// `NtCurrentProcess()`.
const CURRENT_PROCESS: isize = -1;

fn is_self(process: HANDLE) -> bool {
    use windows_sys::Win32::System::Threading::{GetCurrentProcessId, GetProcessId};
    let p = process as isize;
    // SAFETY: `GetProcessId` only reads the handle's process id (0 on failure).
    p == CURRENT_PROCESS
        || (p != 0 && unsafe { GetProcessId(process) } == unsafe { GetCurrentProcessId() })
}

/// `NtDuplicateObject` of a key handle. `None` when the source is not a tracked key handle of
/// this process: the caller passes the call through untouched.
///
/// A synthetic source gives a new synthetic handle with its own private real handle; it cannot
/// go to another process (`STATUS_NOT_SUPPORTED`), because no other process can resolve it. A
/// pass-through source is duplicated for real and the duplicate in this process is tracked.
/// `DUPLICATE_CLOSE_SOURCE` closes the source either way, as NT does even when the duplication
/// fails.
///
/// # Safety
/// The arguments are the caller's NT arguments.
#[allow(clippy::too_many_arguments)]
pub unsafe fn duplicate(
    real: &Real,
    src_process: HANDLE,
    src: HANDLE,
    dst_process: HANDLE,
    dst: *mut HANDLE,
    access: u32,
    attributes: u32,
    options: u32,
) -> Option<NTSTATUS> {
    let sh = src as isize;
    let close_source = options & DUPLICATE_CLOSE_SOURCE != 0;
    let new_access = |old: u32| {
        if options & DUPLICATE_SAME_ACCESS != 0 {
            old
        } else {
            map_generic(access)
        }
    };
    if is_synthetic(sh) {
        if !is_self(src_process) {
            return None;
        }
        let rec = synthetic(sh)?;
        let status = if dst.is_null() {
            STATUS_SUCCESS
        } else if !is_self(dst_process) {
            STATUS_NOT_SUPPORTED
        } else {
            // The duplicate owns a fresh private real handle of its own, opened the way the
            // source's was, so each closes independently.
            let mut real_dup = None;
            let mut st = STATUS_SUCCESS;
            if rec.real.is_some() {
                match open_private(real, &rec.path, rec.requested) {
                    Ok(r) => real_dup = Some(r),
                    Err(e) => st = e,
                }
            }
            if st < 0 {
                st
            } else {
                match insert_synthetic(SynthKey {
                    path: rec.path.clone(),
                    access: new_access(rec.access),
                    real: real_dup,
                    requested: rec.requested,
                    attributes: if options & DUPLICATE_SAME_ATTRIBUTES != 0 {
                        rec.attributes
                    } else {
                        attributes & OBJ_INHERIT
                    },
                }) {
                    Some(h) => {
                        *dst = h as HANDLE;
                        STATUS_SUCCESS
                    }
                    None => {
                        if let Some(r) = real_dup {
                            close_real(real, r);
                        }
                        STATUS_UNSUCCESSFUL
                    }
                }
            }
        };
        if close_source {
            close(real, sh);
        }
        return Some(status);
    }
    if !is_self(src_process) {
        return None;
    }
    // NT closes the source even when the duplication fails, so its records go first: the
    // handle value may be reused the moment the call returns.
    let rec = tracked(sh);
    if close_source {
        untrack(sh);
        crate::regquery::forget(sh);
    }
    let rec = rec?;
    let dup = real.dup?;
    let st = dup(
        src_process,
        src,
        dst_process,
        dst,
        access,
        attributes,
        options,
    );
    if st >= 0 && !dst.is_null() && is_self(dst_process) {
        track(
            *dst as isize,
            KeyRec {
                path: rec.path,
                access: new_access(rec.access),
            },
        );
    }
    Some(st)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_key_handles_stay_clear_of_the_other_tags() {
        let h = insert_synthetic(SynthKey {
            path: r"\Registry\Machine\X".into(),
            access: KEY_READ,
            real: None,
            requested: KEY_READ,
            attributes: 0,
        })
        .unwrap();
        assert!(is_synthetic(h));
        assert_eq!(h % 4, 0);
        assert_eq!(
            h as usize & (1 << 45),
            0,
            "would read as a synthetic section"
        );
        assert_eq!(h as usize & (1 << 47), 0, "would read as a synthetic file");
        assert!(!is_synthetic(-1), "a pseudo-handle is not a key handle");
        assert!(!is_synthetic(0x1234));
        assert_eq!(path_of(h).as_deref(), Some(r"\Registry\Machine\X"));
        assert!(remove_synthetic(h).is_some());
        assert!(synthetic(h).is_none());
    }

    #[test]
    fn sid_bytes_render_as_the_string_form() {
        // S-1-5-21-111-222-333-1001
        let mut sid = vec![1u8, 5, 0, 0, 0, 0, 0, 5];
        for s in [21u32, 111, 222, 333, 1001] {
            sid.extend_from_slice(&s.to_le_bytes());
        }
        assert_eq!(
            sid_string(&sid).as_deref(),
            Some("S-1-5-21-111-222-333-1001")
        );
        assert_eq!(sid_string(&sid[..sid.len() - 1]), None, "truncated");
        assert_eq!(
            sid_string(&[1, 0, 0, 0, 0, 0, 0, 18]).as_deref(),
            Some("S-1-18")
        );
    }

    #[test]
    fn names_compose_against_their_root() {
        let sid = Some("S-1-5-21-1-2-3-1000");
        assert_eq!(
            compose(None, r"\REGISTRY\USER\s-1-5-21-1-2-3-1000\Software", sid),
            Resolved::Path(r"\Registry\User\<CurrentUser>\Software".into())
        );
        assert_eq!(compose(None, r"\Device\X", sid), Resolved::NotOurs);
        assert_eq!(compose(None, r"Software\X", sid), Resolved::NotOurs);
        let b = r"\Registry\Machine\Software";
        let base = Some(b);
        assert_eq!(compose(base, "", sid), Resolved::Path(b.into()));
        assert_eq!(
            compose(base, r"A\B\", sid),
            Resolved::Path(r"\Registry\Machine\Software\A\B".into())
        );
        assert_eq!(compose(base, r"A\..\B", sid), Resolved::Invalid);
        assert_eq!(compose(base, r"\A", sid), Resolved::Invalid);
    }

    #[test]
    fn generic_rights_map_to_key_rights() {
        assert_eq!(map_generic(GENERIC_READ), KEY_READ);
        assert_eq!(map_generic(GENERIC_WRITE), KEY_WRITE);
        assert_eq!(map_generic(MAXIMUM_ALLOWED), KEY_ALL_ACCESS);
        assert_eq!(map_generic(KEY_READ | KEY_WOW64_32KEY), KEY_READ);
        assert!(wants_write(KEY_SET_VALUE));
        assert!(wants_write(GENERIC_WRITE));
        assert!(wants_write(DELETE));
        assert!(!wants_write(KEY_READ | KEY_WOW64_64KEY));
    }
}

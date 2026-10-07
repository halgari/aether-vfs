//! The registry query hooks: `NtQueryKey`, `NtEnumerateKey`, `NtQueryValueKey`,
//! `NtEnumerateValueKey` and `NtQueryMultipleValueKey` on keys the overlay touches (registry
//! overlay spec sections 3.2, 3.3, 3.5 and 6).
//!
//! **Which handles are merged.** Decided on every call, never remembered on the handle, because
//! a write through a pass-through handle makes its path virtual (Task 11):
//! - a synthetic handle: always merged (its private real handle, if any, plus the overlay node);
//! - a pass-through handle, including a real key handle opened before the hooks (resolved and
//!   recorded on first sight by `regkeys::resolve_for_read`): straight to the real call while `regclient::lookup` says the overlay has
//!   nothing at or below the path (a cached answer costs no round trip); merged otherwise, with
//!   the caller's own handle as the real key;
//! - a tombstoned path: `STATUS_KEY_DELETED`, as for a handle to a deleted key;
//! - the director cannot be asked: the real key alone (the caller's handle, or a synthetic
//!   handle's private one). `regclient` counts the fallback.
//!
//! **What a merge answers itself, and what it hands to the real key.** Entries that come from
//! the real key unchanged are answered by the real call on the real handle with the real index
//! (an untouched real subkey or value in an enumeration, a value the overlay neither sets nor
//! hides): the host's own layout, at the cost of one call. Everything the overlay changes is
//! written by `vfs_registry::layout` from `vfs_registry::merge`. Classes the layout does not own
//! (`KeyFlags`, `Virtualization`, `HandleTags`, `Trust` and later ones) go to the real key; with
//! none, the layout's answer where it has one, else `STATUS_INVALID_PARAMETER`.
//!
//! **Enumeration.** The real key's subkey names (or value names) are read once per enumeration,
//! at index 0, and kept per handle with the merged entry list. A later index reuses the list
//! while the director's registry generation is unchanged; when it moves, the list is merged
//! again from the kept real names and the new node, so overlay changes show at once, as live
//! changes do on Windows. The kept state is one key list and one value list per handle, dropped
//! on `NtClose`. Value enumeration keeps only names: a real value's data is read by the real
//! call that answers its index.
//!
//! **Access, as Windows checks it.** `KEY_QUERY_VALUE` for every class of `NtQueryKey` but the
//! name, and for the value calls; `KEY_ENUMERATE_SUB_KEYS` for `NtEnumerateKey`. Checked against
//! the access a handle was granted (for a handle opened before the hooks, the kernel's own
//! record of it).
//!
//! **Locks.** No lock is held across a real call or a director request, nor while caller memory
//! is written.
#![allow(unsafe_code)]

use core::ffi::c_void;
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use vfs_registry::layout::{
    self, KEY_VALUE_ENTRY_SIZE, KeyInfoClass, ValueEntry, ValueInfoClass, Written,
};
use vfs_registry::path::{self, fold};
use vfs_registry::{Child, Lookup, MergedKey, Node, RealKey, Value, merge};
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};

use crate::ntdef::{
    KEY_BASIC_INFORMATION, KEY_FULL_INFORMATION, KEY_NAME_INFORMATION, KEY_NODE_INFORMATION,
    KEY_VALUE_BASIC_INFORMATION, KEY_VALUE_PARTIAL_INFORMATION, STATUS_ACCESS_DENIED,
    STATUS_ACCESS_VIOLATION, STATUS_BUFFER_OVERFLOW, STATUS_BUFFER_TOO_SMALL,
    STATUS_INVALID_HANDLE, STATUS_INVALID_PARAMETER, STATUS_KEY_DELETED, STATUS_NO_MORE_ENTRIES,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND, STATUS_SUCCESS,
    STATUS_UNSUCCESSFUL, UnicodeString,
};
use crate::regclient;
use crate::regkeys::{
    self, KEY_ENUMERATE_SUB_KEYS, KEY_QUERY_VALUE, KeyHandle, KeyRef, Mode, Real,
};
use crate::sync::{CloseLock, lock_for_close};

/// The rights a read of a real key needs on the private handle that replaces one lacking them.
const READS: u32 = KEY_QUERY_VALUE | KEY_ENUMERATE_SUB_KEYS;

/// `KeyValueFullInformation`, read from real keys when a merge needs value data.
const KEY_VALUE_FULL_INFORMATION: u32 = 1;

/// Who answers a query on a handle.
enum Target {
    /// The real call on this handle.
    Real(isize),
    /// The key is gone from the merged view.
    Deleted,
    Fail(NTSTATUS),
    Merge(KeyRef),
}

/// Decide who answers. Runs the cached `REG_LOOKUP` for every key handle the overlay serves.
///
/// `right` is the access the query needs (0 for none). It is checked here only where the answer
/// is the real call on a synthetic key's private handle, which was opened with rights the
/// caller may not have; a merge checks it itself, after validating the class.
unsafe fn classify(real: &Real, h: isize, right: u32) -> Target {
    let k = match regkeys::key_handle(real, h, Mode::Read) {
        KeyHandle::Key(k) => k,
        KeyHandle::Invalid => return Target::Fail(STATUS_INVALID_HANDLE),
        // A handle opened before the hooks that is not on a virtualised path (or cannot be
        // resolved: counted as a read fallback).
        KeyHandle::NotOurs | KeyHandle::Unresolvable => return Target::Real(h),
    };
    if k.deleted {
        return Target::Deleted;
    }
    if k.synthetic {
        return match regclient::lookup(&k.path) {
            // The real key alone; a key that exists only in the overlay cannot be read.
            Err(_) if k.access & right != right => Target::Fail(STATUS_ACCESS_DENIED),
            Err(_) => k.real.map_or(Target::Deleted, Target::Real),
            Ok((Lookup::Tombstoned, _)) => Target::Deleted,
            Ok((state, _)) => Target::Merge(merged(k, state)),
        };
    }
    match regclient::lookup(&k.path) {
        // Untouched (the fast path), or the director cannot be asked: the real key.
        Err(_) | Ok((Lookup::Absent, false)) => Target::Real(h),
        Ok((Lookup::Tombstoned, _)) => Target::Deleted,
        Ok((state, _)) => Target::Merge(merged(k, state)),
    }
}

/// The key to merge: one created here has no real counterpart to show through.
fn merged(k: KeyRef, state: Lookup) -> KeyRef {
    let created = state == Lookup::Present { created: true };
    KeyRef {
        real: if created { None } else { k.real },
        ..k
    }
}

// ---- Reading real keys through the unhooked calls ----

/// Call `q(buffer, length, &mut needed)` with a buffer grown until the answer fits. `Ok` holds
/// the bytes of a `STATUS_SUCCESS` answer; any other status is `Err`.
unsafe fn grown(
    mut q: impl FnMut(*mut c_void, u32, *mut u32) -> NTSTATUS,
) -> Result<Vec<u8>, NTSTATUS> {
    let mut buf = vec![0u64; 64];
    for _ in 0..8 {
        let cap = buf.len() * 8;
        let mut need = 0u32;
        let st = q(buf.as_mut_ptr().cast(), cap as u32, &mut need);
        if st == STATUS_BUFFER_OVERFLOW || st == STATUS_BUFFER_TOO_SMALL {
            // Always grow: a host can report a ResultLength the answer does not fit (Node's
            // class padding on Windows), and an overflow is never handed on.
            buf = vec![0u64; (need as usize).max(cap * 2).div_ceil(8)];
            continue;
        }
        if st != STATUS_SUCCESS {
            return Err(st);
        }
        return Ok(core::slice::from_raw_parts(buf.as_ptr() as *const u8, cap).to_vec());
    }
    Err(STATUS_UNSUCCESSFUL)
}

fn rd_u32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

fn rd_u64(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

fn rd_units(b: &[u8], off: usize, len: usize) -> Option<Vec<u16>> {
    let bytes = b.get(off..off.checked_add(len)?)?;
    Some(
        bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect(),
    )
}

fn rd_str(b: &[u8], off: usize, len: usize) -> Option<String> {
    rd_units(b, off, len).map(|u| String::from_utf16_lossy(&u))
}

/// A real key's own fields, from `NtQueryKey`.
struct RealInfo {
    /// The key's own name, as stored (Node class only).
    name: Option<String>,
    last_write: u64,
    class: Option<Vec<u16>>,
    /// `MaxClassLen` (Full class only).
    max_class: u32,
}

/// `KeyNodeInformation` (name, last write, class) or, with `full`, `KeyFullInformation` (last
/// write, class, `MaxClassLen`).
unsafe fn real_info(real: &Real, h: isize, full: bool) -> Result<RealInfo, NTSTATUS> {
    let q = real.query.ok_or(STATUS_UNSUCCESSFUL)?;
    let class = if full {
        KEY_FULL_INFORMATION
    } else {
        KEY_NODE_INFORMATION
    };
    let b = grown(|p, len, need| q(h as HANDLE, class, p, len, need))?;
    let bad = || STATUS_UNSUCCESSFUL;
    let last_write = rd_u64(&b, 0).ok_or_else(bad)?;
    let class_off = rd_u32(&b, 12).ok_or_else(bad)?;
    let class_len = rd_u32(&b, 16).ok_or_else(bad)? as usize;
    let class = if class_len == 0 || class_off == u32::MAX {
        None
    } else {
        Some(rd_units(&b, class_off as usize, class_len).ok_or_else(bad)?)
    };
    if full {
        return Ok(RealInfo {
            name: None,
            last_write,
            class,
            max_class: rd_u32(&b, 28).ok_or_else(bad)?,
        });
    }
    let name_len = rd_u32(&b, 20).ok_or_else(bad)? as usize;
    Ok(RealInfo {
        name: Some(rd_str(&b, 24, name_len).ok_or_else(bad)?),
        last_write,
        class,
        max_class: 0,
    })
}

/// No key holds more entries than this; a real call that never says "no more" stops here.
const MAX_ENTRIES: u32 = 1 << 24;

/// The real key's subkey names, in its own order.
pub(crate) unsafe fn real_subkeys(real: &Real, h: isize) -> Result<Vec<String>, NTSTATUS> {
    let e = real.enum_key.ok_or(STATUS_UNSUCCESSFUL)?;
    let mut out = Vec::new();
    for i in 0..MAX_ENTRIES {
        let b = match grown(|p, len, need| e(h as HANDLE, i, KEY_BASIC_INFORMATION, p, len, need)) {
            Ok(b) => b,
            Err(STATUS_NO_MORE_ENTRIES) => break,
            Err(st) => return Err(st),
        };
        // KEY_BASIC_INFORMATION: NameLength@12 Name@16.
        let n = rd_u32(&b, 12).ok_or(STATUS_UNSUCCESSFUL)? as usize;
        out.push(rd_str(&b, 16, n).ok_or(STATUS_UNSUCCESSFUL)?);
    }
    Ok(out)
}

/// The real key's values in its own order: names and types, and the data with `data`.
pub(crate) unsafe fn real_values(
    real: &Real,
    h: isize,
    data: bool,
) -> Result<Vec<Value>, NTSTATUS> {
    let e = real.enum_value.ok_or(STATUS_UNSUCCESSFUL)?;
    let class = if data {
        KEY_VALUE_FULL_INFORMATION
    } else {
        KEY_VALUE_BASIC_INFORMATION
    };
    let bad = || STATUS_UNSUCCESSFUL;
    let mut out = Vec::new();
    for i in 0..MAX_ENTRIES {
        let b = match grown(|p, len, need| e(h as HANDLE, i, class, p, len, need)) {
            Ok(b) => b,
            Err(STATUS_NO_MORE_ENTRIES) => break,
            Err(st) => return Err(st),
        };
        let ty = rd_u32(&b, 4).ok_or_else(bad)?;
        out.push(if data {
            // KEY_VALUE_FULL_INFORMATION: DataOffset@8 DataLength@12 NameLength@16 Name@20.
            let off = rd_u32(&b, 8).ok_or_else(bad)? as usize;
            let len = rd_u32(&b, 12).ok_or_else(bad)? as usize;
            let n = rd_u32(&b, 16).ok_or_else(bad)? as usize;
            Value {
                name: rd_str(&b, 20, n).ok_or_else(bad)?,
                ty,
                data: if len == 0 {
                    Vec::new()
                } else {
                    b.get(off..off + len).ok_or_else(bad)?.to_vec()
                },
            }
        } else {
            // KEY_VALUE_BASIC_INFORMATION: NameLength@8 Name@12.
            let n = rd_u32(&b, 8).ok_or_else(bad)? as usize;
            Value {
                name: rd_str(&b, 12, n).ok_or_else(bad)?,
                ty,
                data: Vec::new(),
            }
        });
    }
    Ok(out)
}

/// What of the real key a merged answer needs.
#[derive(Clone, Copy)]
struct Need {
    /// `MaxClassLen` (`KeyFullInformation`); otherwise the key's own name is read.
    full: bool,
    /// Subkey names and values (with their data): the counts of Full and Cached.
    lists: bool,
}

impl Need {
    fn of(class: KeyInfoClass) -> Need {
        Need {
            full: class == KeyInfoClass::Full,
            lists: matches!(class, KeyInfoClass::Full | KeyInfoClass::Cached),
        }
    }
}

/// A `RealKey` (as much as `need` asks for) and the real key's own name.
unsafe fn read_real(
    real: &Real,
    h: isize,
    need: Need,
) -> Result<(RealKey, Option<String>), NTSTATUS> {
    let info = real_info(real, h, need.full)?;
    let (subkeys, values) = if need.lists {
        (real_subkeys(real, h)?, real_values(real, h, true)?)
    } else {
        (Vec::new(), Vec::new())
    };
    Ok((
        RealKey {
            subkeys,
            values,
            class: info.class,
            last_write: info.last_write,
            max_subkey_class_len: info.max_class,
        },
        info.name,
    ))
}

/// The key's own name as stored: the real key's (unless created here), else the spelling the
/// parent's overlay node keeps for it, else the last component of the handle's path.
fn stored_leaf(path: &str, created: bool, real_name: Option<String>) -> String {
    if !created {
        if let Some(n) = real_name {
            return n;
        }
    }
    let leaf = path::leaf(path);
    if let Some(parent) = path::parent(path) {
        let f = fold(leaf);
        if let Ok(Some(s)) = regclient::with_key(parent, |n| {
            n.and_then(|n| n.children.get(&f).map(|(s, _)| s.clone()))
        }) {
            return s;
        }
    }
    leaf.to_string()
}

// ---- Writing answers into caller memory ----

/// Run a layout writer over the caller's buffer and report its `ResultLength`.
unsafe fn emit(
    info: *mut c_void,
    len: u32,
    ret: *mut u32,
    f: impl FnOnce(&mut [u8]) -> Written,
) -> NTSTATUS {
    if info.is_null() && len != 0 {
        return STATUS_ACCESS_VIOLATION;
    }
    let mut empty = [0u8; 0];
    let buf: &mut [u8] = if len == 0 {
        &mut empty
    } else {
        core::slice::from_raw_parts_mut(info as *mut u8, len as usize)
    };
    let w = f(buf);
    if !ret.is_null() {
        core::ptr::write_unaligned(ret, w.result_length);
    }
    w.status
}

fn key_class(class: u32) -> Option<KeyInfoClass> {
    Some(match class {
        0 => KeyInfoClass::Basic,
        1 => KeyInfoClass::Node,
        2 => KeyInfoClass::Full,
        3 => KeyInfoClass::Name,
        4 => KeyInfoClass::Cached,
        5 => KeyInfoClass::Flags,
        6 => KeyInfoClass::Virtualization,
        7 => KeyInfoClass::HandleTags,
        _ => return None,
    })
}

fn value_class(class: u32) -> Option<ValueInfoClass> {
    Some(match class {
        0 => ValueInfoClass::Basic,
        1 => ValueInfoClass::Full,
        2 => ValueInfoClass::Partial,
        3 => ValueInfoClass::FullAlign64,
        4 => ValueInfoClass::PartialAlign64,
        _ => return None,
    })
}

/// The overlay node at the key, or `Err` when the director cannot be asked.
fn node_of(p: &str) -> Result<Option<Node>, NTSTATUS> {
    regclient::key(p).map_err(|_| STATUS_UNSUCCESSFUL)
}

/// The caller's `UNICODE_STRING` as a value name: a NULL pointer is `STATUS_ACCESS_VIOLATION`,
/// as is a NULL buffer with a length. An odd length drops its last byte (`ntbuf::value_name_units`).
unsafe fn read_us(us: *const UnicodeString) -> Result<String, NTSTATUS> {
    crate::ntbuf::value_name_units(us)?
        .map(String::from_utf16_lossy)
        .ok_or(STATUS_ACCESS_VIOLATION)
}

// ---- NtQueryKey ----

/// `NtQueryKey` with the overlay on.
///
/// # Safety
/// The arguments are the caller's NT arguments.
pub(crate) unsafe fn query_key(
    real: &Real,
    h: isize,
    class: u32,
    info: *mut c_void,
    len: u32,
    ret: *mut u32,
) -> NTSTATUS {
    let Some(tramp) = real.query else {
        return STATUS_UNSUCCESSFUL;
    };
    let ctx = match classify(
        real,
        h,
        if class == KEY_NAME_INFORMATION {
            0
        } else {
            KEY_QUERY_VALUE
        },
    ) {
        Target::Real(r) => return tramp(r as HANDLE, class, info, len, ret),
        Target::Deleted => return STATUS_KEY_DELETED,
        Target::Fail(st) => return st,
        Target::Merge(c) => c,
    };
    if class != KEY_NAME_INFORMATION {
        if let Err(st) = ctx.check(KEY_QUERY_VALUE) {
            return st;
        }
    }
    let kc = key_class(class);
    match kc {
        Some(KeyInfoClass::Name) => {
            // A pass-through handle names its real key, unless the key it now stands for has no
            // real counterpart (renamed through it, or created here): then its path does.
            if !ctx.synthetic && ctx.real.is_some() {
                return tramp(h as HANDLE, class, info, len, ret);
            }
            let name = path::to_nt(&ctx.path, regkeys::user_sid());
            emit(info, len, ret, |b| {
                layout::write_key_info(KeyInfoClass::Name, &MergedKey::default(), &name, b)
            })
        }
        Some(
            kc @ (KeyInfoClass::Basic
            | KeyInfoClass::Node
            | KeyInfoClass::Full
            | KeyInfoClass::Cached),
        ) => merged_key_info(real, &ctx, kc, info, len, ret),
        // Classes the layout zeroes or does not know: the real key's own answer.
        _ => match ctx.real {
            Some(r) => tramp(r as HANDLE, class, info, len, ret),
            None => match kc {
                Some(kc) => emit(info, len, ret, |b| {
                    layout::write_key_info(kc, &MergedKey::default(), &ctx.path, b)
                }),
                None => STATUS_INVALID_PARAMETER,
            },
        },
    }
}

unsafe fn merged_key_info(
    real: &Real,
    ctx: &KeyRef,
    kc: KeyInfoClass,
    info: *mut c_void,
    len: u32,
    ret: *mut u32,
) -> NTSTATUS {
    let read = match ctx.with_real(real, READS, |rh| read_real(real, rh, Need::of(kc))) {
        Ok(r) => r,
        Err(st) => return st,
    };
    let node = match node_of(&ctx.path) {
        Ok(n) => n,
        // Lost the director since the lookup: the real key alone.
        Err(st) => {
            return match ctx.real {
                Some(r) => real
                    .query
                    .map_or(st, |q| q(r as HANDLE, kc as u32, info, len, ret)),
                None => STATUS_KEY_DELETED,
            };
        }
    };
    let (real_key, real_name) = match read {
        Some((k, n)) => (Some(k), n),
        None => (None, None),
    };
    let Some(merged) = merge(real_key.as_ref(), node.as_ref(), false) else {
        return STATUS_KEY_DELETED;
    };
    let created = node.as_ref().is_some_and(|n| n.created);
    let leaf = stored_leaf(&ctx.path, created, real_name);
    emit(info, len, ret, |b| {
        layout::write_key_info(kc, &merged, &leaf, b)
    })
}

// ---- Enumeration state ----

/// One entry of a merged subkey list.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SubEntry {
    /// As stored: the real spelling for a real subkey, the overlay's for one created here.
    name: String,
    /// Index in the real key's own enumeration, for a real subkey.
    real: Option<u32>,
    /// The overlay has a node for it: its answer is a merge, not the real one.
    touched: bool,
}

/// One entry of a merged value list.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ValEntry {
    Overlay(Value),
    /// Index in the real key's own enumeration.
    Real(u32),
}

/// The merged subkey list (spec 3.3), in `vfs_registry::merge`'s order: the real subkeys in
/// their order minus tombstoned names, then the overlay's own subkeys sorted. A created-here
/// node hides the real ones.
fn sub_entries(real: Option<&[String]>, node: Option<&Node>) -> Vec<SubEntry> {
    let created = node.is_some_and(|n| n.created);
    let mut out = Vec::new();
    let mut real_names = HashSet::new();
    if let Some(r) = real.filter(|_| !created) {
        for (i, s) in r.iter().enumerate() {
            let f = fold(s);
            let child = node.and_then(|n| n.children.get(&f)).map(|(_, c)| *c);
            real_names.insert(f);
            if child == Some(Child::Tombstone) {
                continue;
            }
            out.push(SubEntry {
                name: s.clone(),
                real: Some(i as u32),
                touched: child.is_some(),
            });
        }
    }
    if let Some(n) = node {
        for (f, (spelling, c)) in &n.children {
            if *c == Child::Present && !real_names.contains(f) {
                out.push(SubEntry {
                    name: spelling.clone(),
                    real: None,
                    touched: true,
                });
            }
        }
    }
    out
}

/// The merged value list (spec 3.3): the overlay's values, then the real values not shadowed
/// or tombstoned. A created-here node hides the real ones.
fn val_entries(real: Option<&[String]>, node: Option<&Node>) -> Vec<ValEntry> {
    let Some(n) = node else {
        return real
            .unwrap_or(&[])
            .iter()
            .enumerate()
            .map(|(i, _)| ValEntry::Real(i as u32))
            .collect();
    };
    let mut out: Vec<ValEntry> = n.values.iter().cloned().map(ValEntry::Overlay).collect();
    if let Some(r) = real.filter(|_| !n.created) {
        let hidden: HashSet<String> = n
            .values
            .iter()
            .map(|v| fold(&v.name))
            .chain(n.value_tombstones.iter().cloned())
            .collect();
        out.extend(
            r.iter()
                .enumerate()
                .filter(|(_, name)| !hidden.contains(&fold(name)))
                .map(|(i, _)| ValEntry::Real(i as u32)),
        );
    }
    out
}

/// The kept state of one enumeration: the real names read at index 0 and the merged list.
struct EnumState<E> {
    /// The real key's names; `None` when no real key shows.
    real: Option<Vec<String>>,
    /// The registry generation the list was merged at (0: none published, never reused).
    generation: u64,
    entries: Arc<Vec<E>>,
}

#[derive(Default)]
struct HandleEnums {
    path: String,
    keys: Option<EnumState<SubEntry>>,
    values: Option<EnumState<ValEntry>>,
}

static ENUMS: Mutex<BTreeMap<isize, HandleEnums>> = Mutex::new(BTreeMap::new());
/// Entries in [`ENUMS`], so `NtClose` of a handle that never enumerated skips the lock.
static ENUM_COUNT: AtomicUsize = AtomicUsize::new(0);
/// A bound on [`ENUMS`] beyond the live handles: a state put back after a racing close is
/// otherwise kept until its handle value is closed again.
const MAX_ENUM_STATES: usize = 4096;

/// Handles with kept enumeration state.
pub(crate) fn states() -> usize {
    ENUMS.lock().map_or(0, |t| t.len())
}

/// Drop the enumeration state of a handle being closed.
pub(crate) fn forget(h: isize) {
    if ENUM_COUNT.load(Ordering::Relaxed) == 0 {
        return;
    }
    if let Some(mut t) = lock_for_close(&ENUMS, &CloseLock::REGISTRY) {
        if t.remove(&h).is_some() {
            ENUM_COUNT.store(t.len(), Ordering::Relaxed);
        }
    }
}

/// Which list of a handle.
trait Kind: Sized + Clone {
    fn slot(e: &mut HandleEnums) -> &mut Option<EnumState<Self>>;
    fn entries(real: Option<&[String]>, node: Option<&Node>) -> Vec<Self>;
    unsafe fn read_names(real: &Real, h: isize) -> Result<Vec<String>, NTSTATUS>;
}

impl Kind for SubEntry {
    fn slot(e: &mut HandleEnums) -> &mut Option<EnumState<Self>> {
        &mut e.keys
    }
    fn entries(real: Option<&[String]>, node: Option<&Node>) -> Vec<Self> {
        sub_entries(real, node)
    }
    unsafe fn read_names(real: &Real, h: isize) -> Result<Vec<String>, NTSTATUS> {
        real_subkeys(real, h)
    }
}

impl Kind for ValEntry {
    fn slot(e: &mut HandleEnums) -> &mut Option<EnumState<Self>> {
        &mut e.values
    }
    fn entries(real: Option<&[String]>, node: Option<&Node>) -> Vec<Self> {
        val_entries(real, node)
    }
    unsafe fn read_names(real: &Real, h: isize) -> Result<Vec<String>, NTSTATUS> {
        Ok(real_values(real, h, false)?
            .into_iter()
            .map(|v| v.name)
            .collect())
    }
}

/// Take a handle's kept state of kind `K` out of the table (none if the handle now names
/// another path).
fn take_state<K: Kind>(h: isize, p: &str) -> Option<EnumState<K>> {
    let mut t = ENUMS.lock().ok()?;
    let e = t.get_mut(&h)?;
    if e.path != p {
        t.remove(&h);
        ENUM_COUNT.store(t.len(), Ordering::Relaxed);
        return None;
    }
    K::slot(e).take()
}

/// Put a handle's state back, unless the handle was closed meanwhile.
fn put_state<K: Kind>(ctx: &KeyRef, s: EnumState<K>) {
    if regkeys::path_of(ctx.handle).as_deref() != Some(&ctx.path) {
        return;
    }
    let Ok(mut t) = ENUMS.lock() else {
        return;
    };
    if t.len() >= MAX_ENUM_STATES && !t.contains_key(&ctx.handle) {
        t.clear();
    }
    let e = t.entry(ctx.handle).or_insert_with(|| HandleEnums {
        path: ctx.path.clone(),
        ..HandleEnums::default()
    });
    *K::slot(e) = Some(s);
    ENUM_COUNT.store(t.len(), Ordering::Relaxed);
}

/// The merged list of kind `K` for the enumeration call at `index`. Index 0 (or no kept state)
/// reads the real names afresh; a later index reuses the kept list while the registry
/// generation is unchanged, and merges the kept real names with the current node otherwise.
unsafe fn view<K: Kind>(real: &Real, ctx: &KeyRef, index: u32) -> Result<Arc<Vec<K>>, NTSTATUS> {
    let generation = regclient::generation();
    let kept = take_state::<K>(ctx.handle, &ctx.path)
        .filter(|s| index != 0 && s.real.is_some() == ctx.real.is_some());
    let names = match kept {
        Some(s) if generation != 0 && s.generation == generation => {
            let entries = s.entries.clone();
            put_state(ctx, s);
            return Ok(entries);
        }
        Some(s) => s.real,
        None => ctx.with_real(real, READS, |rh| K::read_names(real, rh))?,
    };
    let node = match node_of(&ctx.path) {
        Ok(n) => n,
        // The director cannot be asked (counted by `regclient`): the real list alone, for this
        // call only, so the merge comes back as soon as the director does.
        Err(_) => return Ok(Arc::new(K::entries(names.as_deref(), None))),
    };
    let entries = Arc::new(K::entries(names.as_deref(), node.as_ref()));
    put_state(
        ctx,
        EnumState {
            real: names,
            generation,
            entries: entries.clone(),
        },
    );
    Ok(entries)
}

// ---- NtEnumerateKey ----

/// `NtEnumerateKey` with the overlay on.
///
/// # Safety
/// The arguments are the caller's NT arguments.
pub(crate) unsafe fn enumerate_key(
    real: &Real,
    h: isize,
    index: u32,
    class: u32,
    info: *mut c_void,
    len: u32,
    ret: *mut u32,
) -> NTSTATUS {
    let Some(tramp) = real.enum_key else {
        return STATUS_UNSUCCESSFUL;
    };
    let ctx = match classify(real, h, KEY_ENUMERATE_SUB_KEYS) {
        Target::Real(r) => return tramp(r as HANDLE, index, class, info, len, ret),
        Target::Deleted => return STATUS_KEY_DELETED,
        Target::Fail(st) => return st,
        Target::Merge(c) => c,
    };
    // Only Basic, Node and Full enumerate (WRK `NtEnumerateKey`).
    let kc = match class {
        0 => KeyInfoClass::Basic,
        1 => KeyInfoClass::Node,
        2 => KeyInfoClass::Full,
        _ => return STATUS_INVALID_PARAMETER,
    };
    if let Err(st) = ctx.check(KEY_ENUMERATE_SUB_KEYS) {
        return st;
    }
    let entries = match view::<SubEntry>(real, &ctx, index) {
        Ok(e) => e,
        Err(st) => return st,
    };
    let Some(e) = entries.get(index as usize) else {
        return STATUS_NO_MORE_ENTRIES;
    };
    let forward = |ri: u32| match ctx.real {
        Some(r) => tramp(r as HANDLE, ri, class, info, len, ret),
        None => STATUS_NO_MORE_ENTRIES,
    };
    if !e.touched {
        // An untouched real subkey: the real answer at its real index. The index is from the
        // snapshot taken at index 0; if the real key itself changes meanwhile, later entries
        // shift, as a live enumeration of the real key would.
        return e.real.map_or(STATUS_NO_MORE_ENTRIES, forward);
    }
    let child = format!("{}\\{}", ctx.path, e.name);
    let Ok(node) = node_of(&child) else {
        return e.real.map_or(STATUS_UNSUCCESSFUL, forward);
    };
    let created = node.as_ref().is_some_and(|n| n.created);
    let need = Need::of(kc);
    let child_real = if created || e.real.is_none() {
        None
    } else {
        match regkeys::open_private(real, &child, ctx.wow64) {
            Ok(ch) => {
                let r = read_real(real, ch, need);
                regkeys::close_real(real, ch);
                match r {
                    Ok((k, _)) => Some(k),
                    Err(STATUS_KEY_DELETED) => None,
                    // Unreadable beyond what the parent's enumeration shows: that answer.
                    Err(_) => return e.real.map_or(STATUS_UNSUCCESSFUL, forward),
                }
            }
            Err(st) if st == STATUS_OBJECT_NAME_NOT_FOUND || st == STATUS_OBJECT_PATH_NOT_FOUND => {
                None
            }
            Err(_) => return e.real.map_or(STATUS_UNSUCCESSFUL, forward),
        }
    };
    let Some(merged) = merge(child_real.as_ref(), node.as_ref(), false) else {
        return e.real.map_or(STATUS_NO_MORE_ENTRIES, forward);
    };
    emit(info, len, ret, |b| {
        layout::write_subkey_info(kc, &e.name, &merged, b)
    })
}

// ---- NtEnumerateValueKey ----

/// `NtEnumerateValueKey` with the overlay on.
///
/// # Safety
/// The arguments are the caller's NT arguments.
pub(crate) unsafe fn enumerate_value_key(
    real: &Real,
    h: isize,
    index: u32,
    class: u32,
    info: *mut c_void,
    len: u32,
    ret: *mut u32,
) -> NTSTATUS {
    let Some(tramp) = real.enum_value else {
        return STATUS_UNSUCCESSFUL;
    };
    let ctx = match classify(real, h, KEY_QUERY_VALUE) {
        Target::Real(r) => return tramp(r as HANDLE, index, class, info, len, ret),
        Target::Deleted => return STATUS_KEY_DELETED,
        Target::Fail(st) => return st,
        Target::Merge(c) => c,
    };
    // The class first, then the access (WRK).
    let Some(vc) = value_class(class) else {
        return STATUS_INVALID_PARAMETER;
    };
    if let Err(st) = ctx.check(KEY_QUERY_VALUE) {
        return st;
    }
    let entries = match view::<ValEntry>(real, &ctx, index) {
        Ok(e) => e,
        Err(st) => return st,
    };
    match entries.get(index as usize) {
        None => STATUS_NO_MORE_ENTRIES,
        Some(ValEntry::Overlay(v)) => emit(info, len, ret, |b| layout::write_value_info(vc, v, b)),
        Some(ValEntry::Real(ri)) => match ctx.real {
            Some(r) => tramp(r as HANDLE, *ri, class, info, len, ret),
            None => STATUS_NO_MORE_ENTRIES,
        },
    }
}

// ---- NtQueryValueKey and NtQueryMultipleValueKey ----

/// Where a value of the merged view comes from.
enum Hit {
    Overlay(Value),
    /// Tombstoned, or the key was created here: not there.
    Hidden,
    /// Whatever the real key has.
    Real,
}

fn hit(n: Option<&Node>, folded: &str) -> Hit {
    let Some(n) = n else {
        return Hit::Real;
    };
    if let Some(v) = n.values.iter().find(|v| fold(&v.name) == folded) {
        return Hit::Overlay(v.clone());
    }
    if n.created || n.value_tombstones.iter().any(|t| t == folded) {
        return Hit::Hidden;
    }
    Hit::Real
}

/// `NtQueryValueKey` with the overlay on.
///
/// # Safety
/// The arguments are the caller's NT arguments.
pub(crate) unsafe fn query_value_key(
    real: &Real,
    h: isize,
    name: *const UnicodeString,
    class: u32,
    info: *mut c_void,
    len: u32,
    ret: *mut u32,
) -> NTSTATUS {
    let Some(tramp) = real.query_value else {
        return STATUS_UNSUCCESSFUL;
    };
    let ctx = match classify(real, h, KEY_QUERY_VALUE) {
        Target::Real(r) => return tramp(r as HANDLE, name, class, info, len, ret),
        Target::Deleted => return STATUS_KEY_DELETED,
        Target::Fail(st) => return st,
        Target::Merge(c) => c,
    };
    // The class first, then the access (WRK).
    let Some(vc) = value_class(class) else {
        return STATUS_INVALID_PARAMETER;
    };
    if let Err(st) = ctx.check(KEY_QUERY_VALUE) {
        return st;
    }
    let vname = match read_us(name) {
        Ok(v) => v,
        Err(st) => return st,
    };
    let f = fold(&vname);
    // The director lost since the lookup: the real key alone (counted by `regclient`).
    let found = regclient::with_key(&ctx.path, |n| hit(n, &f)).unwrap_or(Hit::Real);
    match found {
        Hit::Overlay(v) => emit(info, len, ret, |b| layout::write_value_info(vc, &v, b)),
        Hit::Hidden => STATUS_OBJECT_NAME_NOT_FOUND,
        Hit::Real => match ctx.real {
            Some(r) => tramp(r as HANDLE, name, class, info, len, ret),
            None => STATUS_OBJECT_NAME_NOT_FOUND,
        },
    }
}

/// One real value by name, through `KeyValuePartialInformation`. `Ok(None)`: not there.
unsafe fn real_value(real: &Real, h: isize, name: &str) -> Result<Option<Value>, NTSTATUS> {
    let q = real.query_value.ok_or(STATUS_UNSUCCESSFUL)?;
    let mut w: Vec<u16> = name.encode_utf16().collect();
    let bytes = (w.len() * 2).min(u16::MAX as usize & !1) as u16;
    let us = UnicodeString {
        length: bytes,
        maximum_length: bytes,
        buffer: w.as_mut_ptr(),
    };
    let b = match grown(|p, len, need| {
        q(
            h as HANDLE,
            &us,
            KEY_VALUE_PARTIAL_INFORMATION,
            p,
            len,
            need,
        )
    }) {
        Ok(b) => b,
        Err(STATUS_OBJECT_NAME_NOT_FOUND) => return Ok(None),
        Err(st) => return Err(st),
    };
    // KEY_VALUE_PARTIAL_INFORMATION: Type@4 DataLength@8 Data@12.
    let bad = || STATUS_UNSUCCESSFUL;
    let ty = rd_u32(&b, 4).ok_or_else(bad)?;
    let n = rd_u32(&b, 8).ok_or_else(bad)? as usize;
    Ok(Some(Value {
        name: name.to_string(),
        ty,
        data: b.get(12..12 + n).ok_or_else(bad)?.to_vec(),
    }))
}

/// `NtQueryMultipleValueKey` with the overlay on, following WRK `CmQueryMultipleValueKey` as
/// `layout::write_multiple_values` does.
///
/// # Safety
/// The arguments are the caller's NT arguments.
pub(crate) unsafe fn query_multiple_value_key(
    real: &Real,
    h: isize,
    entries: *mut c_void,
    count: u32,
    buffer: *mut c_void,
    buffer_len: *mut u32,
    required: *mut u32,
) -> NTSTATUS {
    let Some(tramp) = real.query_multiple else {
        return STATUS_UNSUCCESSFUL;
    };
    let ctx = match classify(real, h, KEY_QUERY_VALUE) {
        Target::Real(r) => return tramp(r as HANDLE, entries, count, buffer, buffer_len, required),
        Target::Deleted => return STATUS_KEY_DELETED,
        Target::Fail(st) => return st,
        Target::Merge(c) => c,
    };
    if let Err(st) = ctx.check(KEY_QUERY_VALUE) {
        return st;
    }
    if buffer_len.is_null() || (entries.is_null() && count != 0) {
        return STATUS_ACCESS_VIOLATION;
    }
    let n = count as usize;
    let slot = |i: usize| (entries as *mut u8).add(i * KEY_VALUE_ENTRY_SIZE);
    let mut names = Vec::with_capacity(n);
    for i in 0..n {
        let us = core::ptr::read_unaligned(slot(i) as *const *const UnicodeString);
        match read_us(us) {
            Ok(s) => names.push(s),
            Err(st) => return st,
        }
    }
    let folded: Vec<String> = names.iter().map(|s| fold(s)).collect();
    let hits = regclient::with_key(&ctx.path, |node| {
        folded.iter().map(|f| hit(node, f)).collect::<Vec<_>>()
    })
    .unwrap_or_else(|_| folded.iter().map(|_| Hit::Real).collect());
    // One value (or `None`) per entry, so the two lists have the same length; resolving stops
    // at the first missing value, where the layout stops too.
    let mut values: Vec<Option<Value>> = Vec::with_capacity(n);
    for (name, found) in names.iter().zip(hits) {
        let v = match found {
            Hit::Overlay(v) => Some(v),
            Hit::Hidden => None,
            Hit::Real => match ctx.with_real(real, READS, |rh| real_value(real, rh, name)) {
                Ok(v) => v.flatten(),
                Err(st) => return st,
            },
        };
        let missing = v.is_none();
        values.push(v);
        if missing {
            break;
        }
    }
    values.resize(n, None);
    let refs: Vec<Option<&Value>> = values.iter().map(Option::as_ref).collect();
    // Seeded from the caller's array: the layout fills only the entries it copies.
    let mut seeds: Vec<ValueEntry> = (0..n)
        .map(|i| {
            let p = slot(i);
            ValueEntry {
                data_length: core::ptr::read_unaligned(p.add(8) as *const u32),
                data_offset: core::ptr::read_unaligned(p.add(12) as *const u32),
                ty: core::ptr::read_unaligned(p.add(16) as *const u32),
            }
        })
        .collect();
    let blen = core::ptr::read_unaligned(buffer_len);
    if buffer.is_null() && blen != 0 {
        return STATUS_ACCESS_VIOLATION;
    }
    let mut empty = [0u8; 0];
    let buf: &mut [u8] = if blen == 0 {
        &mut empty
    } else {
        core::slice::from_raw_parts_mut(buffer as *mut u8, blen as usize)
    };
    let w = layout::write_multiple_values(&refs, &mut seeds, buf);
    for (i, e) in seeds.iter().enumerate() {
        e.store(core::slice::from_raw_parts_mut(
            slot(i),
            KEY_VALUE_ENTRY_SIZE,
        ));
    }
    if w.status != STATUS_OBJECT_NAME_NOT_FOUND {
        core::ptr::write_unaligned(buffer_len, w.buffer_length);
        if !required.is_null() {
            core::ptr::write_unaligned(required, w.result_length);
        }
    }
    w.status
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs_registry::Overlay;

    const P: &str = r"\Registry\Machine\Software\Q";

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn real_key(subs: &[&str], vals: &[&str]) -> RealKey {
        RealKey {
            subkeys: names(subs),
            values: vals
                .iter()
                .map(|n| Value {
                    name: n.to_string(),
                    ty: 1,
                    data: vec![],
                })
                .collect(),
            ..RealKey::default()
        }
    }

    /// The entry lists name the same keys and values, in the same order, as `merge`.
    fn agrees(o: &Overlay, subs: &[&str], vals: &[&str]) {
        let r = real_key(subs, vals);
        let node = o.node(P);
        let m = merge(Some(&r), node, false).unwrap();
        let s = sub_entries(Some(&r.subkeys), node);
        let sn: Vec<&str> = s.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(sn, m.subkeys.iter().map(String::as_str).collect::<Vec<_>>());
        for e in &s {
            if let Some(i) = e.real {
                assert_eq!(fold(&r.subkeys[i as usize]), fold(&e.name));
            }
        }
        let rv: Vec<String> = r.values.iter().map(|v| v.name.clone()).collect();
        let v = val_entries(Some(&rv), node);
        let vn: Vec<String> = v
            .iter()
            .map(|e| match e {
                ValEntry::Overlay(v) => v.name.clone(),
                ValEntry::Real(i) => rv[*i as usize].clone(),
            })
            .collect();
        assert_eq!(
            vn,
            m.values.iter().map(|v| v.name.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn entry_lists_follow_the_merge() {
        let mut o = Overlay::new();
        o.create_key(&format!(r"{P}\zeta"), false, false, 1)
            .unwrap();
        o.create_key(&format!(r"{P}\Alpha"), false, false, 2)
            .unwrap();
        o.create_key(&format!(r"{P}\Real\Deep"), false, false, 2)
            .unwrap();
        o.delete_key(&format!(r"{P}\GONE"), 3).unwrap();
        o.set_value(P, "B", 1, b"x", 4).unwrap();
        o.set_value(P, "new", 1, b"y", 4).unwrap();
        o.delete_value(P, "c", 5).unwrap();
        agrees(&o, &["Gone", "Zed", "real", "bee"], &["a", "b", "C", "d"]);
        let s = sub_entries(Some(&names(&["Gone", "Zed", "real"])), o.node(P));
        assert_eq!(
            s,
            vec![
                SubEntry {
                    name: "Zed".into(),
                    real: Some(1),
                    touched: false
                },
                SubEntry {
                    name: "real".into(),
                    real: Some(2),
                    touched: true
                },
                SubEntry {
                    name: "Alpha".into(),
                    real: None,
                    touched: true
                },
                SubEntry {
                    name: "zeta".into(),
                    real: None,
                    touched: true
                },
            ]
        );
    }

    #[test]
    fn a_created_node_hides_the_real_lists() {
        let mut o = Overlay::new();
        o.create_key(P, false, false, 1).unwrap();
        o.create_key(&format!(r"{P}\kid"), false, false, 1).unwrap();
        agrees(&o, &["old"], &["x"]);
        assert!(val_entries(Some(&names(&["x"])), o.node(P)).is_empty());
    }

    /// An overflow that reports a length the buffer already has (Windows' Node padding) still
    /// grows the buffer, and an overflow is never the answer.
    #[test]
    fn grown_grows_on_every_overflow() {
        let mut calls = vec![];
        let r = unsafe {
            grown(|_, len, need| {
                calls.push(len);
                *need = len;
                if len < 2048 {
                    STATUS_BUFFER_OVERFLOW
                } else {
                    STATUS_SUCCESS
                }
            })
        };
        assert_eq!(r.map(|b| b.len()), Ok(2048));
        assert_eq!(calls, [512, 1024, 2048]);
        let r = unsafe {
            grown(|_, _, need| {
                *need = 8;
                STATUS_BUFFER_OVERFLOW
            })
        };
        assert_eq!(r, Err(STATUS_UNSUCCESSFUL));
    }

    #[test]
    fn no_node_is_the_real_lists() {
        let o = Overlay::new();
        agrees(&o, &["b", "a"], &["y", "x"]);
        assert_eq!(
            val_entries(Some(&names(&["y", "x"])), None),
            vec![ValEntry::Real(0), ValEntry::Real(1)]
        );
    }
}

//! The registry write hooks: `NtSetValueKey`, `NtDeleteValueKey`, `NtDeleteKey`,
//! `NtRenameKey`, `NtSetInformationKey` and `NtFlushKey` (registry overlay spec sections 3.1,
//! 3.2 and 6).
//!
//! **Every write on a virtualised path goes to the director, never to the real registry.**
//! That holds for synthetic handles and for pass-through (real) handles alike: a write through
//! a real handle is copy-on-write. The caller keeps its handle; from then on the overlay touches
//! the path, so the query hooks (decided per call, `regquery`) merge it. A handle the overlay
//! does not serve (not a key, or a key outside `\Registry\Machine` and `\Registry\User`) goes to
//! the real call: [`Write::Pass`].
//!
//! **Order of checks, as Windows makes them:** the arguments the caller passes (`NtSetValueKey`'s
//! value name and data, `NtSetInformationKey`'s class and length), then the access the handle was
//! granted, then whether the key is deleted, then the call's own conditions. A director that
//! cannot be asked fails the write with `STATUS_UNSUCCESSFUL` (spec section 6).
//!
//! **Deleted keys.** `NtDeleteKey` marks the handle's record deleted: every later call through
//! it but `NtClose` answers `STATUS_KEY_DELETED`. Any other handle on a tombstoned path (or one
//! below it) gets the same, decided per call from `regclient::lookup`, which reports a path below
//! a tombstone as tombstoned.
//!
//! **Rename.** `REG_RENAME_KEY` moves overlay nodes only, so a key with a real counterpart is
//! renamed by copying its merged subtree to the new name as keys created here, then tombstoning
//! the old name ([`copy_rename`]). A key created here (so is everything below it) is renamed by
//! the director directly.
//!
//! **Locks.** No `regkeys` table lock is held across a real call or a director request.
#![allow(unsafe_code)]

use core::ffi::c_void;
use std::collections::HashSet;

use vfs_protocol::{ST_BAD_REQUEST, ST_EXISTS, ST_NOT_FOUND};
use vfs_registry::overlay::{MAX_DATA, MAX_KEY_NAME, MAX_VALUE_NAME};
use vfs_registry::path::{self, fold};
use vfs_registry::{merge, utf16_len, Child, KeyView, Lookup, Node, Value};
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};

use crate::ntdef::{
    UnicodeString, KEY_VALUE_BASIC_INFORMATION, STATUS_ACCESS_DENIED, STATUS_ACCESS_VIOLATION,
    STATUS_BUFFER_OVERFLOW, STATUS_BUFFER_TOO_SMALL, STATUS_CANNOT_DELETE,
    STATUS_INFO_LENGTH_MISMATCH, STATUS_INSUFFICIENT_RESOURCES, STATUS_INVALID_HANDLE,
    STATUS_INVALID_INFO_CLASS, STATUS_INVALID_PARAMETER, STATUS_KEY_DELETED,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_SUCCESS, STATUS_UNSUCCESSFUL,
};
use crate::regclient;
use crate::regkeys::{
    self, gone, KeyHandle, KeyRef, Mode, Real, DELETE, KEY_ENUMERATE_SUB_KEYS, KEY_QUERY_VALUE,
    KEY_SET_VALUE, KEY_WRITE,
};

/// `KeyFlagsInformation`: `KeyFlags` (offset 4) has `REG_FLAG_VOLATILE` (bit 0).
const KEY_FLAGS_INFORMATION: u32 = 5;
const REG_FLAG_VOLATILE: u32 = 1;

/// A rename that copies more keys than this, or more value bytes than [`MAX_COPY_BYTES`],
/// fails with `STATUS_INSUFFICIENT_RESOURCES` before anything is written.
pub(crate) const MAX_COPY_KEYS: usize = 10_000;
pub(crate) const MAX_COPY_BYTES: usize = 64 << 20;

/// What a write hook does with a call.
pub(crate) enum Write {
    /// Not a key the overlay serves: the real call.
    Pass,
    /// Answered here.
    Done(NTSTATUS),
}

/// The handle's record. `Err(Write::Pass)` for a handle the overlay does not serve,
/// `Err(Write::Done(STATUS_UNSUCCESSFUL))` for one that cannot be resolved.
unsafe fn target(real: &Real, h: isize) -> Result<KeyRef, Write> {
    match regkeys::key_handle(real, h, Mode::Write) {
        KeyHandle::Key(k) => Ok(k),
        KeyHandle::NotOurs => Err(Write::Pass),
        KeyHandle::Invalid => Err(Write::Done(STATUS_INVALID_HANDLE)),
        // It may be a key the overlay serves: a write through it fails closed (spec section 6).
        KeyHandle::Unresolvable => Err(Write::Done(STATUS_UNSUCCESSFUL)),
    }
}

/// The key's overlay state, for a write through `t`: `STATUS_KEY_DELETED` when it was deleted
/// (through this handle, or its path or an ancestor is tombstoned), `STATUS_UNSUCCESSFUL` when
/// the director cannot be asked.
fn live(t: &KeyRef) -> Result<Lookup, NTSTATUS> {
    if t.deleted {
        return Err(STATUS_KEY_DELETED);
    }
    match regclient::lookup(&t.path) {
        Ok((Lookup::Tombstoned, _)) => Err(STATUS_KEY_DELETED),
        Ok((l, _)) => Ok(l),
        Err(_) => Err(STATUS_UNSUCCESSFUL),
    }
}

/// A director write's failure as the NT status the caller gets (spec section 6).
fn write_status(st: i32) -> NTSTATUS {
    match st {
        // Over the spec's limits (oversize data, a bad name).
        ST_BAD_REQUEST => STATUS_INVALID_PARAMETER,
        // The key is tombstoned (a delete raced this write).
        ST_NOT_FOUND => STATUS_KEY_DELETED,
        // A name that exists (rename, create).
        ST_EXISTS => STATUS_CANNOT_DELETE,
        _ => STATUS_UNSUCCESSFUL,
    }
}

fn done(r: Result<(), NTSTATUS>) -> Write {
    Write::Done(match r {
        Ok(()) => STATUS_SUCCESS,
        Err(st) => st,
    })
}

/// The caller's `UNICODE_STRING` as UTF-16 units: `Ok(None)` for a NULL pointer. An odd length drops
/// its last byte, as the server reads it (`ntbuf::value_name_units`).
unsafe fn units<'a>(us: *const UnicodeString) -> Result<Option<&'a [u16]>, NTSTATUS> {
    crate::ntbuf::value_name_units(us)
}

// ---- NtSetValueKey ----

/// `NtSetValueKey` with the overlay on. The type and data go to the director unchanged (any type
/// number; empty data is a value too). A NULL or empty name is the key's default value.
///
/// # Safety
/// The arguments are the caller's NT arguments.
pub(crate) unsafe fn set_value_key(
    real: &Real,
    h: isize,
    name: *const UnicodeString,
    ty: u32,
    data: *const c_void,
    size: u32,
) -> Write {
    let t = match target(real, h) {
        Ok(t) => t,
        Err(w) => return w,
    };
    done((|| {
        let name = units(name)?.unwrap_or(&[]);
        if name.len() > MAX_VALUE_NAME {
            return Err(STATUS_INVALID_PARAMETER);
        }
        // Over the spec's limit (the director would refuse it too): never copied.
        if size as usize > MAX_DATA {
            return Err(STATUS_INVALID_PARAMETER);
        }
        if data.is_null() && size != 0 {
            return Err(STATUS_ACCESS_VIOLATION);
        }
        t.check(KEY_SET_VALUE)?;
        live(&t)?;
        let bytes: &[u8] = if size == 0 {
            &[]
        } else {
            core::slice::from_raw_parts(data as *const u8, size as usize)
        };
        regclient::set_value(&t.path, &String::from_utf16_lossy(name), ty, bytes)
            .map_err(write_status)
    })())
}

// ---- NtDeleteValueKey ----

/// Where a value of the merged view is.
enum Found {
    Overlay,
    /// Tombstoned, or the key was created here.
    Hidden,
    /// Whatever the real key has.
    Real,
}

fn find_value(n: Option<&Node>, folded: &str) -> Found {
    let Some(n) = n else {
        return Found::Real;
    };
    if n.values.iter().any(|v| fold(&v.name) == folded) {
        return Found::Overlay;
    }
    if n.created || n.value_tombstones.iter().any(|t| t == folded) {
        return Found::Hidden;
    }
    Found::Real
}

/// Whether the real key has a value of this name.
unsafe fn real_has_value(real: &Real, t: &KeyRef, name: &[u16]) -> Result<bool, NTSTATUS> {
    let q = real.query_value.ok_or(STATUS_UNSUCCESSFUL)?;
    let mut w = name.to_vec();
    let bytes = (w.len() * 2).min(u16::MAX as usize & !1) as u16;
    let us = UnicodeString {
        length: bytes,
        maximum_length: bytes,
        buffer: w.as_mut_ptr(),
    };
    let found = t.with_real(real, KEY_QUERY_VALUE, |h| {
        let mut buf = [0u64; 2];
        let mut need = 0u32;
        match q(
            h as HANDLE,
            &us,
            KEY_VALUE_BASIC_INFORMATION,
            buf.as_mut_ptr().cast(),
            16,
            &mut need,
        ) {
            STATUS_SUCCESS | STATUS_BUFFER_OVERFLOW | STATUS_BUFFER_TOO_SMALL => Ok(true),
            STATUS_OBJECT_NAME_NOT_FOUND => Ok(false),
            st => Err(st),
        }
    })?;
    Ok(found == Some(true))
}

/// `NtDeleteValueKey` with the overlay on: a tombstone for the name in the overlay, which also
/// hides a real value. A name the merged view does not have is `STATUS_OBJECT_NAME_NOT_FOUND`.
///
/// # Safety
/// The arguments are the caller's NT arguments.
pub(crate) unsafe fn delete_value_key(real: &Real, h: isize, name: *const UnicodeString) -> Write {
    let t = match target(real, h) {
        Ok(t) => t,
        Err(w) => return w,
    };
    done((|| {
        let name = units(name)?.unwrap_or(&[]);
        // As Wine (and Windows) answer a name longer than any value can have.
        if name.len() > MAX_VALUE_NAME {
            return Err(STATUS_OBJECT_NAME_NOT_FOUND);
        }
        t.check(KEY_SET_VALUE)?;
        live(&t)?;
        let name_s = String::from_utf16_lossy(name);
        let f = fold(&name_s);
        let found =
            regclient::with_key(&t.path, |n| find_value(n, &f)).map_err(|_| STATUS_UNSUCCESSFUL)?;
        let exists = match found {
            Found::Overlay => true,
            Found::Hidden => false,
            Found::Real => real_has_value(real, &t, name)?,
        };
        if !exists {
            return Err(STATUS_OBJECT_NAME_NOT_FOUND);
        }
        regclient::delete_value(&t.path, &name_s).map_err(write_status)
    })())
}

// ---- NtDeleteKey ----

/// A hive root or above (`\Registry\Machine\SOFTWARE`, `\Registry\User\<sid>`): Windows refuses
/// to delete or rename these (`STATUS_ACCESS_DENIED`).
fn is_hive_root(p: &str) -> bool {
    p.split('\\').filter(|c| !c.is_empty()).count() <= 3
}

/// Whether the key has a subkey in the merged view (real minus tombstones, plus the overlay's).
unsafe fn has_subkeys(real: &Real, t: &KeyRef, state: Lookup) -> Result<bool, NTSTATUS> {
    let node = regclient::key(&t.path).map_err(|_| STATUS_UNSUCCESSFUL)?;
    if let Some(n) = &node {
        if n.children.values().any(|(_, c)| *c == Child::Present) {
            return Ok(true);
        }
    }
    let created =
        state == (Lookup::Present { created: true }) || node.as_ref().is_some_and(|n| n.created);
    if created {
        return Ok(false);
    }
    let names = t
        .with_real(real, KEY_ENUMERATE_SUB_KEYS, |h| {
            crate::regquery::real_subkeys(real, h)
        })?
        .unwrap_or_default();
    Ok(names.iter().any(|s| {
        let dead = node
            .as_ref()
            .and_then(|n| n.children.get(&fold(s)))
            .is_some_and(|(_, c)| *c == Child::Tombstone);
        !dead
    }))
}

/// `NtDeleteKey` with the overlay on: `REG_DELETE_KEY` (a tombstone over the real key, the
/// overlay subtree dropped). A key with subkeys is `STATUS_CANNOT_DELETE`, as on Windows; on
/// success the handle is marked deleted.
///
/// # Safety
/// `h` is the caller's handle.
pub(crate) unsafe fn delete_key(real: &Real, h: isize) -> Write {
    let t = match target(real, h) {
        Ok(t) => t,
        Err(w) => return w,
    };
    done((|| {
        t.check(DELETE)?;
        let state = live(&t)?;
        if is_hive_root(&t.path) {
            return Err(STATUS_ACCESS_DENIED);
        }
        if has_subkeys(real, &t, state)? {
            return Err(STATUS_CANNOT_DELETE);
        }
        regclient::delete_key(&t.path).map_err(write_status)?;
        regkeys::mark_deleted(t.handle);
        Ok(())
    })())
}

// ---- NtRenameKey ----

/// Whether `p` exists in the merged view (a key that refuses even a read exists).
unsafe fn exists(real: &Real, p: &str, wow64: u32) -> Result<bool, NTSTATUS> {
    match regclient::lookup(p) {
        Ok((Lookup::Present { .. }, _)) => Ok(true),
        Ok((Lookup::Tombstoned, _)) => Ok(false),
        Ok((Lookup::Absent, _)) => {
            Ok(!regkeys::below_created(p) && regkeys::real_exists(real, p, wow64))
        }
        Err(_) => Err(STATUS_UNSUCCESSFUL),
    }
}

/// `NtRenameKey` with the overlay on. The checks follow Windows (and Wine's server, which
/// matches it): the handle needs `KEY_WRITE`; the new name must be one non-empty component of at
/// most 255 characters (`STATUS_INVALID_PARAMETER`); a name that already exists under the parent,
/// the key's own included in any case, is `STATUS_CANNOT_DELETE`.
///
/// # Safety
/// The arguments are the caller's NT arguments.
pub(crate) unsafe fn rename_key(real: &Real, h: isize, new_name: *const UnicodeString) -> Write {
    let t = match target(real, h) {
        Ok(t) => t,
        Err(w) => return w,
    };
    done((|| {
        // ntdll's own checks, before the handle is looked at.
        let name = units(new_name)?.ok_or(STATUS_ACCESS_VIOLATION)?;
        if name.is_empty() {
            return Err(STATUS_INVALID_PARAMETER);
        }
        t.check(KEY_WRITE)?;
        let leaf = String::from_utf16_lossy(name);
        if leaf.contains('\\') || utf16_len(&leaf) > MAX_KEY_NAME {
            return Err(STATUS_INVALID_PARAMETER);
        }
        let state = live(&t)?;
        if is_hive_root(&t.path) {
            return Err(STATUS_ACCESS_DENIED);
        }
        let parent = path::parent(&t.path).ok_or(STATUS_ACCESS_DENIED)?;
        let dest = format!("{parent}\\{leaf}");
        if fold(&leaf) == fold(path::leaf(&t.path)) || exists(real, &dest, t.wow64)? {
            return Err(STATUS_CANNOT_DELETE);
        }
        if state == (Lookup::Present { created: true }) {
            // Created here, and so is everything below it: the director moves the nodes.
            regclient::rename_key(&t.path, &leaf).map_err(write_status)?;
        } else {
            copy_rename(real, &t, &dest)?;
        }
        regkeys::retarget(real, t.handle, &dest);
        Ok(())
    })())
}

/// One key of a subtree being copied, relative to the subtree's root (`""` for the root, else
/// `\a\b`).
struct Copied {
    rel: String,
    volatile: bool,
    values: Vec<Value>,
}

/// A real key's lists (values with data) and its volatile flag, through a private handle.
/// `None` when there is no real key.
unsafe fn read_real_key(
    real: &Real,
    p: &str,
    wow64: u32,
) -> Result<Option<(KeyView, bool)>, NTSTATUS> {
    let h = match regkeys::open_private(real, p, KEY_QUERY_VALUE | KEY_ENUMERATE_SUB_KEYS | wow64) {
        Ok(h) => h,
        Err(st) if gone(st) => return Ok(None),
        Err(st) => return Err(st),
    };
    let r = (|| {
        let subkeys = crate::regquery::real_subkeys(real, h)?;
        let values = crate::regquery::real_values(real, h, true)?;
        Ok((
            KeyView {
                subkeys,
                values,
                ..KeyView::default()
            },
            real_volatile(real, h),
        ))
    })();
    regkeys::close_real(real, h);
    match r {
        Ok(v) => Ok(Some(v)),
        Err(st) if gone(st) => Ok(None),
        Err(st) => Err(st),
    }
}

/// `REG_FLAG_VOLATILE` from `KeyFlagsInformation`; a host that does not answer it (Wine) reads
/// as not volatile.
unsafe fn real_volatile(real: &Real, h: isize) -> bool {
    let Some(q) = real.query else {
        return false;
    };
    let mut buf = [0u32; 3];
    let mut need = 0u32;
    let st = q(
        h as HANDLE,
        KEY_FLAGS_INFORMATION,
        buf.as_mut_ptr().cast(),
        12,
        &mut need,
    );
    st == STATUS_SUCCESS && buf[1] & REG_FLAG_VOLATILE != 0
}

/// How much a rename may copy.
#[derive(Clone, Copy)]
struct Limits {
    keys: usize,
    bytes: usize,
}

const COPY_LIMITS: Limits = Limits {
    keys: MAX_COPY_KEYS,
    bytes: MAX_COPY_BYTES,
};

/// One key of the subtree, read for [`collect`]: `(path, whether a real key may show at it)` to
/// the key's overlay node and, when one shows, the real key with its volatile flag.
type KeyRead<'a> = dyn FnMut(&str, bool) -> KeyReadResult + 'a;

/// What a [`KeyRead`] returns.
type KeyReadResult = Result<(Option<Node>, Option<(KeyView, bool)>), NTSTATUS>;

/// The merged view of the subtree at `root`, parents before children, each key's subkeys in
/// merged order. Bounded by `limits` (for a rename, [`MAX_COPY_KEYS`] and [`MAX_COPY_BYTES`]):
/// past either, `STATUS_INSUFFICIENT_RESOURCES` before anything is written.
fn collect(root: &str, limits: Limits, read: &mut KeyRead<'_>) -> Result<Vec<Copied>, NTSTATUS> {
    let mut out = Vec::new();
    let mut bytes = 0usize;
    // (relative path, whether a real key may show at it)
    let mut stack = vec![(String::new(), true)];
    while let Some((rel, real_may_show)) = stack.pop() {
        if out.len() >= limits.keys {
            return Err(STATUS_INSUFFICIENT_RESOURCES);
        }
        let (node, rk) = read(&format!("{root}{rel}"), real_may_show)?;
        let created = node.as_ref().is_some_and(|n| n.created);
        let rk = rk.filter(|_| !created);
        let Some(m) = merge(rk.as_ref().map(|r| &r.0), node.as_ref(), false) else {
            if rel.is_empty() {
                return Err(STATUS_KEY_DELETED);
            }
            // Gone from under the copy (a racing delete of the real key): nothing to copy.
            continue;
        };
        let volatile = node.as_ref().is_some_and(|n| n.volatile)
            || (!created && rk.as_ref().is_some_and(|r| r.1));
        bytes = bytes.saturating_add(
            m.values
                .iter()
                .map(|v| v.data.len() + v.name.len() * 2)
                .sum::<usize>(),
        );
        if bytes > limits.bytes {
            return Err(STATUS_INSUFFICIENT_RESOURCES);
        }
        let real_names: HashSet<String> = rk
            .as_ref()
            .map(|r| r.0.subkeys.iter().map(|s| fold(s)).collect())
            .unwrap_or_default();
        for s in m.subkeys.iter().rev() {
            stack.push((format!("{rel}\\{s}"), real_names.contains(&fold(s))));
        }
        out.push(Copied {
            rel,
            volatile,
            values: m.values,
        });
    }
    Ok(out)
}

/// Rename a key that has a real counterpart: its merged subtree is written at `dest` as keys
/// created here (values, subkeys and volatile flags; classes are not modelled by the overlay),
/// then the old name is tombstoned. The whole subtree is read, and checked against the bounds,
/// before anything is written. A failure partway through tombstones `dest` again, so nothing
/// half-copied stays visible; if even that fails, the partial copy stays (writes are not
/// transactional).
unsafe fn copy_rename(real: &Real, t: &KeyRef, dest: &str) -> Result<(), NTSTATUS> {
    let mut read = |p: &str, real_may_show: bool| {
        let node = regclient::key(p).map_err(|_| STATUS_UNSUCCESSFUL)?;
        let rk = if real_may_show && !node.as_ref().is_some_and(|n| n.created) {
            read_real_key(real, p, t.wow64)?
        } else {
            None
        };
        Ok((node, rk))
    };
    let tree = collect(&t.path, COPY_LIMITS, &mut read)?;
    let Some(root) = tree.first() else {
        return Err(STATUS_KEY_DELETED);
    };
    match regclient::create_key(dest, root.volatile) {
        Ok(()) => {}
        // Someone created the name since it was checked: nothing of ours is there.
        Err(ST_EXISTS) => return Err(STATUS_CANNOT_DELETE),
        Err(st) => return Err(write_status(st)),
    }
    let write = || -> Result<(), i32> {
        for (i, c) in tree.iter().enumerate() {
            let p = format!("{dest}{}", c.rel);
            if i != 0 {
                regclient::create_key(&p, c.volatile)?;
            }
            for v in &c.values {
                regclient::set_value(&p, &v.name, v.ty, &v.data)?;
            }
        }
        regclient::delete_key(&t.path)
    };
    write().map_err(|st| {
        let _ = regclient::delete_key(dest);
        write_status(st)
    })
}

// ---- NtSetInformationKey and NtFlushKey ----

/// `NtSetInformationKey` with the overlay on. The class and length are checked as Windows
/// checks them, then `KEY_SET_VALUE`; the call then succeeds and changes nothing. No ring
/// operation carries a key's last-write time or flags, so neither the real key nor the overlay
/// keeps them (a recorded deviation: a later query reports the overlay's own last-write time).
///
/// # Safety
/// The arguments are the caller's NT arguments.
pub(crate) unsafe fn set_information_key(
    real: &Real,
    h: isize,
    class: u32,
    info: *const c_void,
    len: u32,
) -> Write {
    let t = match target(real, h) {
        Ok(t) => t,
        Err(w) => return w,
    };
    done((|| {
        let want = match class {
            // KeyWriteTimeInformation: a LARGE_INTEGER.
            0 => 8,
            // Wow64Flags/UserFlags, ControlFlags, SetVirtualization, SetHandleTags, SetLayer:
            // one ULONG each.
            1 | 2 | 3 | 5 | 6 => 4,
            _ => return Err(STATUS_INVALID_INFO_CLASS),
        };
        if len != want {
            return Err(STATUS_INFO_LENGTH_MISMATCH);
        }
        if info.is_null() {
            return Err(STATUS_ACCESS_VIOLATION);
        }
        t.check(KEY_SET_VALUE)?;
        not_deleted(&t)
    })())
}

/// The key is not deleted. Nothing is written, so a director that cannot be asked is no failure.
fn not_deleted(t: &KeyRef) -> Result<(), NTSTATUS> {
    match live(t) {
        Err(STATUS_KEY_DELETED) => Err(STATUS_KEY_DELETED),
        _ => Ok(()),
    }
}

/// `NtFlushKey` with the overlay on: nothing of a virtualised key is in the real registry to
/// flush (the director keeps the overlay durable), so it succeeds; a deleted key is
/// `STATUS_KEY_DELETED`. No access right is needed.
///
/// # Safety
/// `h` is the caller's handle.
pub(crate) unsafe fn flush_key(real: &Real, h: isize) -> Write {
    let t = match target(real, h) {
        Ok(t) => t,
        Err(w) => return w,
    };
    done(not_deleted(&t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hive_roots_are_three_components_or_fewer() {
        assert!(is_hive_root(r"\Registry\Machine"));
        assert!(is_hive_root(r"\Registry\Machine\Software"));
        assert!(is_hive_root(r"\Registry\User\<CurrentUser>"));
        assert!(!is_hive_root(r"\Registry\User\<CurrentUser>\Software"));
        assert!(!is_hive_root(r"\Registry\Machine\Software\X"));
    }

    #[test]
    fn a_write_through_an_unresolvable_handle_fails_closed() {
        let real = crate::regkeys::tests::real_whose_name_query_fails(false);
        assert!(matches!(
            unsafe { target(&real, 0x0123_4580) },
            Err(Write::Done(STATUS_UNSUCCESSFUL))
        ));
        let real = crate::regkeys::tests::real_whose_name_query_fails(true);
        assert!(matches!(
            unsafe { target(&real, 0x0123_4590) },
            Err(Write::Pass)
        ));
    }

    #[test]
    fn director_failures_map_to_nt_statuses() {
        assert_eq!(write_status(ST_BAD_REQUEST), STATUS_INVALID_PARAMETER);
        assert_eq!(write_status(ST_NOT_FOUND), STATUS_KEY_DELETED);
        assert_eq!(write_status(ST_EXISTS), STATUS_CANNOT_DELETE);
        assert_eq!(write_status(vfs_protocol::ST_IO_ERROR), STATUS_UNSUCCESSFUL);
    }

    /// A [`KeyRead`] over an overlay alone: every key's node, and no real keys.
    fn created_tree(o: &vfs_registry::Overlay) -> impl FnMut(&str, bool) -> KeyReadResult + '_ {
        move |p: &str, _| Ok((o.node(p).cloned(), None))
    }

    const P: &str = r"\Registry\Machine\Software\Copy";

    /// Keys created here under `P` (`""` is `P` itself), each with one 10-byte value `v`.
    fn overlay_of(keys: &[&str]) -> vfs_registry::Overlay {
        let mut o = vfs_registry::Overlay::new();
        for (i, k) in keys.iter().enumerate() {
            let p = if k.is_empty() {
                P.to_string()
            } else {
                format!(r"{P}\{k}")
            };
            o.create_key(&p, false, false, i as u64).unwrap();
            o.set_value(&p, "v", 3, &[0; 10], i as u64).unwrap();
        }
        o
    }

    #[test]
    fn the_copy_stops_at_its_key_bound_before_anything_is_written() {
        let o = overlay_of(&["", "a", r"a\b", "c"]);
        let lim = |keys, bytes| Limits { keys, bytes };
        let r = collect(P, lim(3, 1 << 20), &mut created_tree(&o));
        assert_eq!(r.err(), Some(STATUS_INSUFFICIENT_RESOURCES));
        let all = collect(P, lim(4, 1 << 20), &mut created_tree(&o)).unwrap();
        let rels: Vec<&str> = all.iter().map(|c| c.rel.as_str()).collect();
        assert_eq!(
            rels,
            ["", r"\a", r"\a\b", r"\c"],
            "parents first, merged order"
        );
    }

    #[test]
    fn the_copy_stops_at_its_byte_bound() {
        let o = overlay_of(&["", "a", "b"]);
        // Each value is 10 data bytes plus a 2-byte name.
        let ok = collect(
            P,
            Limits {
                keys: 10,
                bytes: 36,
            },
            &mut created_tree(&o),
        );
        assert_eq!(ok.map(|t| t.len()), Ok(3));
        let r = collect(
            P,
            Limits {
                keys: 10,
                bytes: 35,
            },
            &mut created_tree(&o),
        );
        assert_eq!(r.err(), Some(STATUS_INSUFFICIENT_RESOURCES));
    }

    #[test]
    fn the_copy_merges_real_keys_with_the_overlay() {
        // The root overlays a real key: an overlay value of the same name wins, the real
        // default value and a deleted real subkey's absence carry over, and the volatile flag
        // comes from the real key.
        let mut o = vfs_registry::Overlay::new();
        o.set_value(P, "same", 1, b"o\0", 1).unwrap();
        o.delete_key(&format!(r"{P}\Gone"), 2).unwrap();
        let real_root = KeyView {
            subkeys: vec!["Kept".into(), "Gone".into()],
            values: vec![
                Value {
                    name: "same".into(),
                    ty: 4,
                    data: vec![1, 0, 0, 0],
                },
                Value {
                    name: "".into(),
                    ty: 1,
                    data: b"d\0".to_vec(),
                },
            ],
            ..KeyView::default()
        };
        let mut read = |p: &str, may: bool| {
            let rk = match (p == P, may) {
                (true, _) => Some((real_root.clone(), true)),
                (false, true) => Some((KeyView::default(), false)),
                (false, false) => None,
            };
            Ok((o.node(p).cloned(), rk))
        };
        let t = collect(P, COPY_LIMITS, &mut read).unwrap();
        assert_eq!(t.len(), 2);
        assert!(t[0].volatile);
        assert_eq!(
            t[0].values,
            vec![
                Value {
                    name: "same".into(),
                    ty: 1,
                    data: b"o\0".to_vec()
                },
                Value {
                    name: "".into(),
                    ty: 1,
                    data: b"d\0".to_vec()
                },
            ]
        );
        assert_eq!(t[1].rel, r"\Kept");
    }

    #[test]
    fn a_value_is_found_in_the_overlay_hidden_or_left_to_the_real_key() {
        let mut o = vfs_registry::Overlay::new();
        let p = r"\Registry\Machine\Software\W";
        o.set_value(p, "Set", 4, &[1, 0, 0, 0], 1).unwrap();
        o.delete_value(p, "gone", 2).unwrap();
        let n = o.node(p);
        assert!(matches!(find_value(n, "set"), Found::Overlay));
        assert!(matches!(find_value(n, "gone"), Found::Hidden));
        assert!(matches!(find_value(n, "other"), Found::Real));
        assert!(matches!(find_value(None, "x"), Found::Real));
        o.create_key(&format!(r"{p}\New"), false, false, 3).unwrap();
        assert!(matches!(
            find_value(o.node(&format!(r"{p}\New")), "x"),
            Found::Hidden
        ));
    }
}

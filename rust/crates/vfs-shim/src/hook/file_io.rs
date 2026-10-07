//! `NtReadFile`, `NtWriteFile`, `NtLockFile`, `NtUnlockFile`, `NtFlushBuffersFile`.

use super::{
    TRAMP_FLUSH, TRAMP_LOCK, TRAMP_READ, TRAMP_UNLOCK, TRAMP_WRITE, open_synth, synth_path,
};
use crate::ntdef::{
    STATUS_END_OF_FILE, STATUS_INVALID_HANDLE, STATUS_SUCCESS, STATUS_UNSUCCESSFUL,
};
use core::ffi::c_void;
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};

/// `NtLockFile` hook — grants byte-range locks on synthetic handles locally.
///
/// **Why this exists.** A synthetic handle is a tagged value in `fuse_synth`'s
/// table, not a kernel file object, so any NT call without a detour hands that
/// value to the real kernel and gets `STATUS_INVALID_HANDLE` back. Measured
/// 2026-08-14: `GetPrivateProfileStringW` — how Skyrim loads `SkyrimPrefs.ini`
/// — issues `NtOpenFile → NtLockFile → NtQueryInformationFile → NtReadFile →
/// NtUnlockFile → NtClose`, and with `NtLockFile` unhooked the sequence
/// stopped dead at step 2. The API then returned the *caller's default* for
/// every key, so the game received no INI data at all — not stale data, not
/// real-disk data. `WritePrivateProfileStringW` failed the same way one
/// operation earlier. Neither showed up as a read or write at the director;
/// both showed up as an open and nothing else.
///
/// **The deliberate semantic gap.** This grants a lock that does not exist.
/// Nothing is recorded, nothing conflicts, and two callers asking for the same
/// exclusive byte range both get `STATUS_SUCCESS`. That is chosen, not
/// overlooked:
///
/// - Inside a sealed managed root the director is the only route to the bytes,
///   and there is no cross-process byte-range locking anywhere in the design
///   today — so there is no lock table for a real answer to consult.
/// - Refusing instead (`STATUS_LOCK_NOT_GRANTED`) would leave the profile APIs
///   exactly as broken as an unhooked call did; it swaps a wrong status for a
///   different wrong status.
///
/// **Do not read that as "there is only one writer".** There is not, by
/// design: `cpiw_hook` propagates injection into child processes, so a
/// launcher and a game — or a game and a mod manager's helper — are routinely
/// in one session. And the API that exposed this bug is the worst case for a
/// fake lock: `WritePrivateProfileString` is a read-modify-write, and the lock
/// it takes here is exactly what stops two of those from losing each other's
/// updates. Two injected writers on one INI will both be granted the same
/// exclusive range and one update will disappear.
///
/// That is a real hole, not a theoretical one; it is accepted because the
/// alternative on offer was every INI staying unreadable, not because it is
/// harmless. Closing it needs a byte-range table in the director — the only
/// component both processes share. Until then
/// `hookstats::note_synthetic_lock` counts every grant by path, so the
/// contention shows up in a report instead of only in corrupted settings.
///
/// **Which handles this answers.** Only ones [`open_synth`] resolves. The
/// bit-47 tag test alone would also catch `INVALID_HANDLE_VALUE` and any
/// closed or never-issued synthetic handle, and answering `STATUS_SUCCESS` for
/// those would report a lock held on a file the caller never opened.
///
/// **Completion.** Answered synchronously: `STATUS_SUCCESS`, a completed
/// `IO_STATUS_BLOCK`, and `SetEvent` if the caller supplied one — the same
/// shape `read_hook` uses, including its one limitation, that we do not run
/// the caller's APC. That limitation is counted rather than assumed away:
/// `note_read_completion` classifies every synthetic lock by the completion
/// its caller expected, so an APC-supplied lock — the shape that would wait
/// forever on a callback we never make — shows up in the report's async
/// section instead of passing for an ordinary grant. `FailImmediately` needs
/// no branch: `false` means the caller is willing to block for the lock, and
/// an immediate grant satisfies that strictly better than waiting.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn lock_hook_body(
    handle: HANDLE,
    event: HANDLE,
    apc: *const c_void,
    apc_ctx: *const c_void,
    iosb: *mut c_void,
    byte_offset: *const i64,
    length: *const i64,
    key: u32,
    fail_immediately: u8,
    exclusive: u8,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::Lock);
    let tramp = match TRAMP_LOCK.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::fuse_synth::is_fuse_synth(handle as isize) {
        if !open_synth(handle) {
            return STATUS_INVALID_HANDLE;
        }
        // Classified the same way `read_hook`/`write_hook` classify theirs: an
        // APC-supplied lock is a completion we accept and never deliver, and
        // that is the one caller shape here that can actually hang. Counting
        // it is what makes it visible in the async section instead of looking
        // like an ordinary synchronous grant.
        crate::hookstats::note_read_completion(!apc.is_null(), !event.is_null());
        crate::hookstats::note_synthetic_lock(
            if exclusive != 0 {
                "lock-exclusive"
            } else {
                "lock-shared"
            },
            synth_path(handle).as_deref(),
        );
        crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, 0);
        if !event.is_null() {
            windows_sys::Win32::System::Threading::SetEvent(event);
        }
        return STATUS_SUCCESS;
    }
    tramp(
        handle,
        event,
        apc,
        apc_ctx,
        iosb,
        byte_offset,
        length,
        key,
        fail_immediately,
        exclusive,
    )
}

/// `NtUnlockFile` hook — the release half of [`lock_hook`], and success for
/// the same reason: a lock that was never recorded cannot fail to be released.
pub(super) unsafe fn unlock_hook_body(
    handle: HANDLE,
    iosb: *mut c_void,
    byte_offset: *const i64,
    length: *const i64,
    key: u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::Unlock);
    let tramp = match TRAMP_UNLOCK.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::fuse_synth::is_fuse_synth(handle as isize) {
        if !open_synth(handle) {
            return STATUS_INVALID_HANDLE;
        }
        crate::hookstats::note_synthetic_lock("unlock", synth_path(handle).as_deref());
        crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, 0);
        return STATUS_SUCCESS;
    }
    tramp(handle, iosb, byte_offset, length, key)
}

/// `NtFlushBuffersFile` hook. Success on a synthetic handle: the director owns
/// durability for everything behind one, and there is no user-mode buffer here
/// to push — `write_hook` forwards each write over the ring as it happens.
///
/// Unlike the lock pair this is not a lie about state, but it is still weaker
/// than what the caller asked for: it promises the bytes are durable, and what
/// it can actually guarantee is that they reached the director.
pub(super) unsafe fn flush_hook_body(handle: HANDLE, iosb: *mut c_void) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::FlushBuffers);
    let tramp = match TRAMP_FLUSH.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::fuse_synth::is_fuse_synth(handle as isize) {
        if !open_synth(handle) {
            return STATUS_INVALID_HANDLE;
        }
        crate::hookstats::note_synthetic_lock("flush", synth_path(handle).as_deref());
        crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, 0);
        return STATUS_SUCCESS;
    }
    tramp(handle, iosb)
}

/// `NtWriteFile` hook. For synthetic (fuse) write handles, forward the game's
/// buffer to the director overlay over the ring and complete the IRP; real handles
/// pass straight through. `ByteOffset` NULL / negative sentinel = current pos.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn write_hook_body(
    handle: HANDLE,
    event: HANDLE,
    apc: *const c_void,
    apc_ctx: *const c_void,
    iosb: *mut c_void,
    buffer: *mut c_void,
    length: u32,
    byte_offset: *const i64,
    key: *const u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::Write);
    let tramp = match TRAMP_WRITE.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::fuse_synth::is_fuse_synth(handle as isize) {
        crate::hookstats::note_read_completion(!apc.is_null(), !event.is_null());
        let explicit = crate::ntbuf::explicit_offset(byte_offset);
        if let Some((fh, size, _is_dir, pos, append_only)) =
            crate::fuse_synth::lookup(handle as isize)
        {
            // Append-only access (FILE_APPEND_DATA without FILE_WRITE_DATA)
            // forces every write to the current end of file at the kernel
            // level, ignoring any offset the caller supplies — a real handle
            // enforces this itself; ours has to do it here.
            let off = if append_only {
                pos
            } else {
                explicit.unwrap_or(pos)
            };
            let want = length as usize;
            // The file is changing: the read cache drops it. (A write handle
            // already dropped it at open; this keeps the rule local.)
            if want > 0 {
                if let Some(f) = crate::fuse_synth::cache(handle as isize) {
                    crate::read_cache::invalidate(&f);
                }
            }
            let n = if want == 0 || buffer.is_null() {
                0usize
            } else {
                // SAFETY: NtWriteFile contract — buffer is readable for `length` bytes.
                let slice = unsafe { core::slice::from_raw_parts(buffer as *const u8, want) };
                match crate::fuse_client::global()
                    .ok_or(vfs_protocol::ST_IO_ERROR)
                    .and_then(|c| c.write(fh, off, slice))
                {
                    Ok(n) => n,
                    Err(_) => {
                        crate::ntbuf::iosb_set(iosb, STATUS_UNSUCCESSFUL, 0);
                        return STATUS_UNSUCCESSFUL;
                    }
                }
            };
            // Append-only always tracks position (every write moved EOF
            // forward regardless of what the caller passed); otherwise only
            // an implicit-offset write consumes the file pointer.
            if append_only || explicit.is_none() {
                crate::fuse_synth::set_position(handle as isize, off + n as u64);
            }
            // The synthetic size was set once at open and never touched
            // since — a write that extends the file must bump it too, or
            // `read_hook`'s EOF check and `fuse_query_information`'s
            // `metadata().len()` keep reporting the pre-write length forever.
            // Only reachable now that writes actually reach the director
            // instead of falling through to a real file (whose kernel FCB
            // would have tracked this for free).
            let end = off + n as u64;
            if end > size {
                crate::fuse_synth::grow_size(handle as isize, end);
            }
            crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, n);
            if !event.is_null() {
                windows_sys::Win32::System::Threading::SetEvent(event);
            }
            return STATUS_SUCCESS;
        }
        // Tagged synth handle with no table entry — never hand it to the real
        // NtWriteFile (mirrors read_hook).
        return STATUS_UNSUCCESSFUL;
    }
    tramp(
        handle,
        event,
        apc,
        apc_ctx,
        iosb,
        buffer,
        length,
        byte_offset,
        key,
    )
}

/// `NtReadFile` hook. Synthetic (fuse) handles are answered from the director
/// over the ring; real handles pass straight through. `ByteOffset` of NULL or
/// the "use current position" sentinel (-1/-2) means "current position".
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn read_hook_body(
    handle: HANDLE,
    event: HANDLE,
    apc: *const c_void,
    apc_ctx: *const c_void,
    iosb: *mut c_void,
    buffer: *mut c_void,
    length: u32,
    byte_offset: *const i64,
    key: *const u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::Read);
    let tramp = match TRAMP_READ.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::fuse_synth::is_fuse_synth(handle as isize) {
        let explicit = crate::ntbuf::explicit_offset(byte_offset);
        if let Some(view) = crate::fuse_synth::lookup_read(handle as isize) {
            let (fh, size, pos) = (view.fh, view.size, view.position);
            let off = explicit.unwrap_or(pos);
            let want = length as usize;
            if off >= size {
                crate::ntbuf::iosb_set(iosb, STATUS_END_OF_FILE, 0);
                return STATUS_END_OF_FILE;
            }
            // Phase 1: fill the game's NtReadFile buffer in place (no intermediate tmp).
            let max = want.min((size - off) as usize);
            let n = if max == 0 || buffer.is_null() {
                0usize
            } else {
                // SAFETY: NtReadFile contract — buffer is writable for `length` bytes.
                let slice = unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, max) };
                // A small synchronous read of an immutable file is offered to
                // the read cache first. Not one that asked for completion by
                // APC or event (those keep exactly the path they had), and not
                // one whose handle's size has moved from what the cache was
                // told at open. A cache answer is `max` bytes, as the ring's
                // would be; `None` is the uncached read below, unchanged.
                let cached = match &view.cache {
                    Some(f) if apc.is_null() && event.is_null() && f.size() == Some(size) => {
                        crate::fuse_client::global().and_then(|c| c.read_cached(f, fh, off, slice))
                    }
                    _ => None,
                };
                match cached.ok_or(()).or_else(|()| {
                    crate::fuse_client::global()
                        .ok_or(vfs_protocol::ST_IO_ERROR)
                        .and_then(|c| c.read_fragmented(fh, off, slice))
                }) {
                    Ok(n) => n,
                    Err(_) => {
                        crate::ntbuf::iosb_set(iosb, STATUS_UNSUCCESSFUL, 0);
                        return STATUS_UNSUCCESSFUL;
                    }
                }
            };
            {
                if explicit.is_none() {
                    crate::fuse_synth::set_position(handle as isize, off + n as u64);
                }
                let at_eof = off + n as u64 >= size;
                let status = if at_eof && n == 0 {
                    STATUS_END_OF_FILE
                } else {
                    STATUS_SUCCESS
                };
                crate::ntbuf::iosb_set(iosb, status, n);
                if !event.is_null() {
                    windows_sys::Win32::System::Threading::SetEvent(event);
                }
                return status;
            }
        }
        return STATUS_UNSUCCESSFUL;
    }
    tramp(
        handle,
        event,
        apc,
        apc_ctx,
        iosb,
        buffer,
        length,
        byte_offset,
        key,
    )
}

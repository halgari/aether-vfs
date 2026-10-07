//! Whole-file copy-up: copying a base-only file into the upper before the
//! first write lands, and the open-for-write path that drives it.

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::{Condvar, Mutex};

use vfs_core::fold;
use vfs_provider::{
    copy_up_name, is_dir, map_io_err, not_found, Handle, VPath, KIND_DIR, KIND_FILE, OPEN_CREATE,
    OPEN_READ, OPEN_TRUNC, OPEN_WRITE,
};

use super::{Layer, OverlayProvider};

/// The copy-ups in flight, keyed by `(root, folded path)`. A thread that
/// finds its key taken sleeps on `done` until the copy ends; it never spins.
#[derive(Default)]
pub(super) struct InFlight {
    keys: Mutex<HashSet<(u32, String)>>,
    done: Condvar,
}

impl InFlight {
    /// Claims `key`, waiting for whoever holds it to finish first. The
    /// returned guard releases the claim and wakes the waiters on drop.
    pub(super) fn claim(&self, key: (u32, String)) -> Result<CopyGuard<'_>, i32> {
        let mut keys = self.keys.lock().map_err(|_| map_io_err())?;
        while keys.contains(&key) {
            keys = self.done.wait(keys).map_err(|_| map_io_err())?;
        }
        keys.insert(key.clone());
        Ok(CopyGuard { in_flight: self, key })
    }
}

/// Removes its key from the in-flight set on drop, including on early return —
/// so a failed copy still releases the slot for the next attempt.
pub(super) struct CopyGuard<'a> {
    in_flight: &'a InFlight,
    key: (u32, String),
}

impl Drop for CopyGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut g) = self.in_flight.keys.lock() {
            g.remove(&self.key);
        }
        self.in_flight.done.notify_all();
    }
}

impl OverlayProvider {
    /// `.cu.<n>.<name>` sibling of `path`, in the same directory as the
    /// eventual destination — so the final rename stays within one parent
    /// (and, for a disk-backed upper, one volume) and is atomic.
    pub(super) fn temp_copy_path(&self, path: &str, n: u64) -> String {
        match path.rsplit_once('/') {
            Some((parent, name)) => format!("{parent}/{}", copy_up_name(n, name)),
            None => copy_up_name(n, path),
        }
    }

    /// Copy the whole base file at `p` into upper if it is not already there.
    /// A no-op if `p` is absent from base too, or is a directory (directories
    /// are represented implicitly, never copied). Guarded by `copying` so two
    /// concurrent callers for the same path copy exactly once: whoever loses
    /// the race sleeps until the winner's slot clears, then re-checks upper
    /// before ever touching base.
    pub(super) fn copy_up_if_needed(&self, p: VPath) -> Result<(), i32> {
        if self.upper.getattr(p)?.is_some() {
            return Ok(());
        }
        let Some(stat) = self.base.getattr(p)? else {
            return Ok(());
        };
        if stat.kind != KIND_FILE {
            return Ok(());
        }

        // Folded: two callers that spell one file differently must wait for
        // each other. Keyed by the spelling, each would copy the file up on
        // its own, and the second copy's rename would replace the file the
        // first caller had already opened for writing. Keyed with the root:
        // the same relative path under two roots is two files.
        let _guard = self.copying.claim((p.root.0, fold(p.rel)))?;

        // Re-check: another thread may have finished the copy between our
        // first getattr above and winning the slot just now.
        if self.upper.getattr(p)?.is_some() {
            return Ok(());
        }
        self.copy_file_up(p)
    }

    /// Copies base's `p` into a `.cu.` temp file in upper and renames it over
    /// `p` only on complete success. The destination is never opened,
    /// touched, or truncated directly: if the read, a write, flush, close,
    /// or the final rename fails, the temp file is removed and the original
    /// error is propagated (not the cleanup's) — so a partial copy can never
    /// be mistaken for a complete one by a later `getattr`/copy-up check,
    /// and a concurrent reader can never observe a half-written destination.
    pub(super) fn copy_file_up(&self, p: VPath) -> Result<(), i32> {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let tmp_rel = self.temp_copy_path(p.rel, n);
        let tmp = VPath::new(p.root, &tmp_rel);

        let (bh, size, _) = self.base.open(p, OPEN_READ)?;
        let copied = self.copy_bytes(bh, size, tmp);
        let _ = self.base.close(bh);

        let result = copied.and_then(|_| self.upper.rename(tmp, p));
        if result.is_err() {
            let _ = self.upper.remove(tmp);
        }
        result
    }

    pub(super) fn copy_bytes(&self, bh: Handle, size: u64, dest: VPath) -> Result<(), i32> {
        let (uh, _, _) = self
            .upper
            .open(dest, OPEN_WRITE | OPEN_CREATE | OPEN_TRUNC)?;
        let copied = self.copy_loop(bh, uh, size).and_then(|_| self.upper.flush(uh));
        let closed = self.upper.close(uh);
        // Prefer the copy/flush error over the close error: it happened
        // first and is almost always the more useful one to report, but
        // either way *an* error here must never be swallowed.
        copied.and(closed)
    }

    pub(super) fn copy_loop(&self, bh: Handle, uh: Handle, size: u64) -> Result<(), i32> {
        let mut buf = [0u8; 65536];
        let mut off = 0u64;
        while off < size {
            let n = self.base.read_at(bh, off, &mut buf)?;
            if n == 0 {
                break;
            }
            self.upper.write_at(uh, off, &buf[..n])?;
            off += n as u64;
        }
        Ok(())
    }

    pub(super) fn open_for_write(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        if self.upper.getattr(p)?.is_none() {
            // An ancestor directory being opaquely removed is deliberately
            // NOT something OPEN_CREATE can paper over. Clearing the
            // ancestor's whiteout here would silently resurrect every other
            // base entry under it that the caller never asked to restore;
            // creating the file anyway while leaving the ancestor whiteout
            // in place would leave it permanently invisible to
            // `hidden_by_whiteout`'s ancestor walk while still showing up
            // through `readdir`'s upper merge — an inconsistent state with
            // no good reading. Refusing is the only option with no
            // surprising side effect; the way back is explicit: `mkdir` the
            // ancestor, which clears exactly its own whiteout.
            if self.ancestor_whited_out(p)? {
                return Err(not_found());
            }
            if self.is_whiteout(p)? {
                if flags & OPEN_CREATE == 0 {
                    return Err(not_found());
                }
                // OPEN_CREATE explicitly asks to (re)create over a whiteout
                // on this exact path; clear it so the new file is genuinely
                // visible afterward.
                self.clear_whiteout(p)?;
            } else {
                // The base serves a **directory** at this path. Falling
                // through to `upper.open(…, OPEN_CREATE)` below would create
                // a *file* in the upper named after it — which then shadows
                // the directory for every later lookup, and makes the whole
                // subtree unlistable. That is reachable from an ordinary
                // Windows call: `CreateFileW(dir, GENERIC_WRITE, OPEN_ALWAYS,
                // FILE_FLAG_BACKUP_SEMANTICS)` sets no `FILE_DIRECTORY_FILE`,
                // so nothing upstream recognises it as a directory open, and
                // `FILE_OPEN_IF` arrives here carrying `OPEN_CREATE`.
                //
                // `copy_up_if_needed` already declines to copy a directory,
                // but declining quietly is what let the create through.
                // Refuse instead, with the status that says why — the shim
                // turns it back into the directory open the caller wanted
                // (`hook::dir_open_downgrades`), and a caller that really did
                // mean "create a file here" gets NT's own answer for a file
                // create over a directory.
                if matches!(self.base.getattr(p)?, Some(st) if st.kind == KIND_DIR) {
                    return Err(is_dir());
                }
                self.copy_up_if_needed(p)?;
            }
        }
        let (uh, size, is_dir) = self.upper.open(p, flags)?;
        self.invalidate_if_marker(p);
        let h = self.track(Layer::Upper, uh)?;
        Ok((h, size, is_dir))
    }
}

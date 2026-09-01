//! `seekable(p)` — give a forward-only provider positional reads.
//!
//! Spec §6: *"`seekable(p)`: `SeqRead` → `Read`"*, and §6's flag table makes it
//! mandatory rather than advisory — *"`SeqRead` provider not wrapped in
//! `seekable`: **hard error** — the director cannot issue positional reads"*.
//! The director's read path is `read_at(handle, offset, buf)`, so a provider
//! that can only stream (a CDN response, a decompressing pipe, a tar member)
//! is unmountable until something turns one into the other. This is that
//! something, and it is the reason a host can write §8's `SteamCdn` with
//! `read_next` alone and never implement a seek.
//!
//! ## The cursor, and the reopen
//!
//! One cursor per open handle. A read at the cursor is a straight `read_next`.
//! A read *ahead* of it is a forward skip — `read_next` into a discard buffer
//! until the cursor arrives — which is the best a forward-only source can do
//! and is exactly what a sequential source costs. A read *behind* it cannot be
//! served forward at all, so the handle is **reopened** and the skip starts
//! from zero. Spec §6's own test list names this case (*"`seekable` reopening
//! on a backward seek"*), because it is the one place where the wrapper's cost
//! is not proportional to the bytes asked for.
//!
//! Reopening strips `OPEN_CREATE`/`OPEN_TRUNC`/`OPEN_EXCL`/`OPEN_APPEND` from
//! the flags it replays. Replaying them would be catastrophic rather than
//! merely wrong: a backward seek on a handle opened with `OPEN_TRUNC` would
//! truncate the file a second time, and one opened `OPEN_EXCL` would fail to
//! reopen because it now exists. The remaining flags are read intent, which is
//! what a reopen is for.
//!
//! ## Why a lock per handle rather than one for the provider
//!
//! The skip loop calls `read_next` on the inner provider while holding the
//! cursor, and on a `slow` source — the only kind anyone wraps in this — that
//! call is the expensive part of a read. One provider-wide mutex would make
//! every concurrent reader on *different files* queue behind it, turning the
//! director's worker pool into a single thread for the whole mount. So the
//! outer map is locked only long enough to clone one `Arc<Mutex<OpenRec>>` out
//! of it, and the inner call happens under that handle's own lock.
//!
//! ## A positional inner provider is passed straight through
//!
//! `seekable(disk(..))` has nothing to do, and `Capabilities::seekable()`
//! already leaves a non-sequential access level alone. Rather than route a
//! positional provider's reads through a cursor that would only ever agree with
//! the offset it was given, the wrapper records at construction whether the
//! inner provider is sequential and forwards `read_at` unchanged when it is
//! not. That matters because a host applying the recommended wrapping
//! everywhere (spec §6's `vfs.auto`) should not pay for it where it is a no-op.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use vfs_provider::{
    bad_fh, map_io_err, Access, Capabilities, DirEntry, Handle, Provider, RootId, SetAttr, Stat,
    VPath, OPEN_APPEND, OPEN_CREATE, OPEN_EXCL, OPEN_TRUNC,
};

/// Bytes discarded per `read_next` while skipping forward. 64 KiB is the block
/// size §8c measured as the best round-trip unit across the Node bridge; the
/// buffer is per-skip and stack-free (a `Vec`), so the size is a throughput
/// choice and not a memory one.
const SKIP_CHUNK: usize = 64 * 1024;

/// Flags that must not be replayed when a backward seek reopens a handle — see
/// the module docs.
const REOPEN_MASK: u32 = !(OPEN_CREATE | OPEN_TRUNC | OPEN_EXCL | OPEN_APPEND);

struct OpenRec {
    /// The inner provider's handle. Replaced on a reopen.
    inner: Handle,
    root: RootId,
    path: String,
    flags: u32,
    /// How many bytes have been consumed from `inner` through `read_next`.
    cursor: u64,
}

/// Positional reads over a forward-only provider (see the module docs).
pub struct SeekableProvider {
    inner: Arc<dyn Provider>,
    /// Whether the inner provider actually needs the cursor. `false` makes
    /// `read_at` a straight forward.
    sequential: bool,
    next: AtomicU64,
    opens: Mutex<HashMap<Handle, Arc<Mutex<OpenRec>>>>,
}

impl SeekableProvider {
    pub fn new(inner: Arc<dyn Provider>) -> Self {
        let sequential = inner.capabilities().access == Access::SeqRead;
        SeekableProvider {
            inner,
            sequential,
            next: AtomicU64::new(1),
            opens: Mutex::new(HashMap::new()),
        }
    }

    /// The wrapped provider.
    pub fn inner(&self) -> &Arc<dyn Provider> {
        &self.inner
    }

    /// Clone one handle's record out of the map, holding the map's lock only
    /// for the lookup — see the module docs on why this is two locks.
    fn rec(&self, h: Handle) -> Result<Arc<Mutex<OpenRec>>, i32> {
        let g = self.opens.lock().map_err(|_| map_io_err())?;
        g.get(&h).map(Arc::clone).ok_or_else(bad_fh)
    }

    /// The inner handle, for the ops that neither seek nor stream.
    fn inner_handle(&self, h: Handle) -> Result<Handle, i32> {
        let rec = self.rec(h)?;
        let g = rec.lock().map_err(|_| map_io_err())?;
        Ok(g.inner)
    }

    /// Reopen `rec`'s file and reset its cursor to zero.
    fn reopen(&self, rec: &mut OpenRec) -> Result<(), i32> {
        // Close first: a provider with a per-file lock or a bounded handle
        // count would otherwise be holding two handles to the same path, and a
        // failed close is not a reason to abandon the seek.
        let _ = self.inner.close(rec.inner);
        let (h, _, _) = self
            .inner
            .open(VPath::new(rec.root, &rec.path), rec.flags & REOPEN_MASK)?;
        rec.inner = h;
        rec.cursor = 0;
        Ok(())
    }

    /// Advance `rec`'s cursor to `offset`, reopening if that means going back.
    /// `Ok(false)` means the source ended before reaching `offset`, which is a
    /// read past EOF and answers zero bytes rather than an error.
    fn seek_to(&self, rec: &mut OpenRec, offset: u64) -> Result<bool, i32> {
        if offset < rec.cursor {
            self.reopen(rec)?;
        }
        let mut skip = Vec::new();
        while rec.cursor < offset {
            let want = ((offset - rec.cursor) as usize).min(SKIP_CHUNK);
            if skip.len() < want {
                skip.resize(want, 0);
            }
            let n = self.inner.read_next(rec.inner, &mut skip[..want])?;
            if n == 0 {
                return Ok(false);
            }
            rec.cursor += n as u64;
        }
        Ok(true)
    }
}

impl Provider for SeekableProvider {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities().seekable()
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        self.inner.getattr(p)
    }

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        self.inner.readdir(p)
    }

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        let (inner, size, is_dir) = self.inner.open(p, flags)?;
        let h = self.next.fetch_add(1, Ordering::Relaxed);
        self.opens.lock().map_err(|_| map_io_err())?.insert(
            h,
            Arc::new(Mutex::new(OpenRec {
                inner,
                root: p.root,
                path: p.rel.to_string(),
                flags,
                cursor: 0,
            })),
        );
        Ok((h, size, is_dir))
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        let rec = {
            let mut g = self.opens.lock().map_err(|_| map_io_err())?;
            g.remove(&h).ok_or_else(bad_fh)?
        };
        let inner = rec.lock().map_err(|_| map_io_err())?.inner;
        self.inner.close(inner)
    }

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let rec = self.rec(h)?;
        let mut g = rec.lock().map_err(|_| map_io_err())?;
        if !self.sequential {
            return self.inner.read_at(g.inner, offset, buf);
        }
        if !self.seek_to(&mut g, offset)? {
            return Ok(0);
        }
        let n = self.inner.read_next(g.inner, buf)?;
        g.cursor += n as u64;
        Ok(n)
    }

    /// Still available, and it keeps the cursor honest so a caller mixing
    /// `read_next` with `read_at` on one handle is not silently given the wrong
    /// bytes on its next positional read.
    fn read_next(&self, h: Handle, buf: &mut [u8]) -> Result<usize, i32> {
        let rec = self.rec(h)?;
        let mut g = rec.lock().map_err(|_| map_io_err())?;
        let n = self.inner.read_next(g.inner, buf)?;
        g.cursor += n as u64;
        Ok(n)
    }

    fn write_at(&self, h: Handle, offset: u64, buf: &[u8]) -> Result<usize, i32> {
        self.inner.write_at(self.inner_handle(h)?, offset, buf)
    }

    fn set_len(&self, h: Handle, len: u64) -> Result<(), i32> {
        self.inner.set_len(self.inner_handle(h)?, len)
    }

    fn flush(&self, h: Handle) -> Result<(), i32> {
        self.inner.flush(self.inner_handle(h)?)
    }

    fn mkdir(&self, p: VPath) -> Result<(), i32> {
        self.inner.mkdir(p)
    }

    fn remove(&self, p: VPath) -> Result<(), i32> {
        self.inner.remove(p)
    }

    fn rename(&self, from: VPath, to: VPath) -> Result<(), i32> {
        self.inner.rename(from, to)
    }

    fn set_attr(&self, p: VPath, attr: SetAttr) -> Result<(), i32> {
        self.inner.set_attr(p, attr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs_provider::conformance::SeqFixture;
    use vfs_provider::{CaseMatch, RwMemFixture, ST_NOT_SUPPORTED, OPEN_READ};

    fn seekable_seq() -> Arc<dyn Provider> {
        Arc::new(SeekableProvider::new(Arc::new(SeqFixture::new())))
    }

    /// The spec's own example: `assert_conformance(casefold(seekable(seq_fixture())))`
    /// minus the `casefold` that does not exist yet. This is the test that says
    /// the promotion is real: `SeqFixture` deliberately has no `read_at`, so
    /// every positional case in the suite is being served by this wrapper.
    #[test]
    fn seekable_over_a_sequential_provider_passes_the_positional_suite() {
        vfs_provider::assert_conformance(seekable_seq());
    }

    #[test]
    fn sequential_access_is_promoted_to_positional() {
        assert_eq!(
            SeqFixture::new().capabilities().access,
            Access::SeqRead,
            "the fixture has to be sequential for this test to mean anything"
        );
        assert_eq!(seekable_seq().capabilities().access, Access::Read);
    }

    #[test]
    fn the_inner_provider_really_cannot_do_positional_reads() {
        // Without this, the test above could be passing because `SeqFixture`
        // grew a `read_at` and the wrapper is doing nothing.
        let seq = SeqFixture::new();
        let (h, _, _) = seq.open(VPath::at_default("a.txt"), OPEN_READ).unwrap();
        assert_eq!(seq.read_at(h, 0, &mut [0u8; 4]).err(), Some(ST_NOT_SUPPORTED));
        seq.close(h).unwrap();
    }

    #[test]
    fn a_backward_seek_reopens_and_serves_the_right_bytes() {
        let p = seekable_seq();
        let (h, size, _) = p.open(VPath::at_default("sub/b.txt"), OPEN_READ).unwrap();
        assert_eq!(size, 6); // "world!"

        let mut buf = [0u8; 6];
        // Forward to the end...
        let n = p.read_at(h, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"world!");
        // ...then all the way back. A forward-only source cannot do this
        // without reopening, so a wrapper that skipped the reopen would return
        // 0 bytes here (already at EOF) rather than the first byte.
        let mut one = [0u8; 1];
        assert_eq!(p.read_at(h, 0, &mut one).unwrap(), 1);
        assert_eq!(&one, b"w");
        // And a partial backward seek lands mid-file.
        let mut two = [0u8; 2];
        assert_eq!(p.read_at(h, 3, &mut two).unwrap(), 2);
        assert_eq!(&two, b"ld");
        p.close(h).unwrap();
    }

    #[test]
    fn a_forward_skip_does_not_reopen_and_lands_on_the_right_byte() {
        let p = seekable_seq();
        let (h, _, _) = p.open(VPath::at_default("a.txt"), OPEN_READ).unwrap();
        let mut buf = [0u8; 2];
        // "hello" — read 'h', then jump to index 3 without going backwards.
        assert_eq!(p.read_at(h, 0, &mut buf[..1]).unwrap(), 1);
        assert_eq!(&buf[..1], b"h");
        let n = p.read_at(h, 3, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"lo");
        p.close(h).unwrap();
    }

    #[test]
    fn reads_at_and_past_eof_answer_zero_rather_than_erroring() {
        let p = seekable_seq();
        let (h, size, _) = p.open(VPath::at_default("a.txt"), OPEN_READ).unwrap();
        assert_eq!(p.read_at(h, size, &mut [0u8; 4]).unwrap(), 0);
        assert_eq!(p.read_at(h, size + 100, &mut [0u8; 4]).unwrap(), 0);
        p.close(h).unwrap();
    }

    #[test]
    fn two_handles_keep_separate_cursors() {
        let p = seekable_seq();
        let (h1, _, _) = p.open(VPath::at_default("a.txt"), OPEN_READ).unwrap();
        let (h2, _, _) = p.open(VPath::at_default("a.txt"), OPEN_READ).unwrap();
        assert_ne!(h1, h2);
        let mut a = [0u8; 2];
        let mut b = [0u8; 2];
        assert_eq!(p.read_at(h1, 0, &mut a).unwrap(), 2);
        // h2's cursor is untouched by h1's read, so this is "he" and not "ll".
        assert_eq!(p.read_at(h2, 0, &mut b).unwrap(), 2);
        assert_eq!(&a, b"he");
        assert_eq!(&b, b"he");
        p.close(h1).unwrap();
        p.close(h2).unwrap();
    }

    #[test]
    fn a_closed_handle_is_rejected() {
        let p = seekable_seq();
        let (h, _, _) = p.open(VPath::at_default("a.txt"), OPEN_READ).unwrap();
        p.close(h).unwrap();
        assert!(p.read_at(h, 0, &mut [0u8; 4]).is_err());
        assert!(p.close(h).is_err());
    }

    /// A positional inner provider is forwarded, cursor unused — and it must
    /// still pass conformance at its own (unchanged) access level, writes
    /// included.
    #[test]
    fn seekable_over_a_positional_provider_is_a_passthrough_and_still_conforms() {
        let inner: Arc<dyn Provider> = Arc::new(RwMemFixture::new());
        let p = SeekableProvider::new(inner);
        assert!(!p.sequential, "a ReadWrite inner needs no cursor");
        assert_eq!(p.capabilities().access, Access::ReadWrite);
        vfs_provider::assert_conformance(Arc::new(p));
    }

    #[test]
    fn a_skip_longer_than_one_chunk_still_lands_correctly() {
        // The skip loop is the only place with a chunk boundary in it, and the
        // fixture tree's files are six bytes long. This drives it with a source
        // big enough to need several `read_next` calls per seek.
        struct BigSeq {
            body: Vec<u8>,
            cursors: Mutex<HashMap<Handle, usize>>,
            next: AtomicU64,
        }
        impl Provider for BigSeq {
            fn capabilities(&self) -> Capabilities {
                // getattr below compares `p.rel` to "big.bin" by byte equality.
                Capabilities {
                    access: Access::SeqRead,
                    case: CaseMatch::Sensitive,
                    ..Capabilities::read_only()
                }
            }
            fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
                Ok(if p.rel == "big.bin" {
                    Some(Stat { kind: vfs_provider::KIND_FILE, size: self.body.len() as u64, mtime: 0 })
                } else {
                    None
                })
            }
            fn readdir(&self, _p: VPath) -> Result<Vec<DirEntry>, i32> {
                Ok(Vec::new())
            }
            fn open(&self, _p: VPath, _flags: u32) -> Result<(Handle, u64, bool), i32> {
                let h = self.next.fetch_add(1, Ordering::Relaxed);
                self.cursors.lock().unwrap().insert(h, 0);
                Ok((h, self.body.len() as u64, false))
            }
            fn close(&self, h: Handle) -> Result<(), i32> {
                self.cursors.lock().unwrap().remove(&h);
                Ok(())
            }
            fn read_next(&self, h: Handle, buf: &mut [u8]) -> Result<usize, i32> {
                let mut g = self.cursors.lock().unwrap();
                let c = g.get_mut(&h).ok_or_else(bad_fh)?;
                let n = (self.body.len() - *c).min(buf.len());
                buf[..n].copy_from_slice(&self.body[*c..*c + n]);
                *c += n;
                Ok(n)
            }
        }

        // 300 KiB: several 64 KiB skip chunks, with a recognisable byte at each
        // offset so a mis-counted chunk is visible rather than plausible.
        let body: Vec<u8> = (0..300 * 1024).map(|i| (i % 251) as u8).collect();
        let inner: Arc<dyn Provider> = Arc::new(BigSeq {
            body: body.clone(),
            cursors: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        });
        let p = SeekableProvider::new(inner);
        let (h, size, _) = p.open(VPath::at_default("big.bin"), OPEN_READ).unwrap();
        assert_eq!(size, body.len() as u64);
        for offset in [0u64, 1, 65_535, 65_536, 65_537, 200_000, 299_000] {
            let mut buf = [0u8; 8];
            let n = p.read_at(h, offset, &mut buf).unwrap();
            assert_eq!(
                &buf[..n],
                &body[offset as usize..offset as usize + n],
                "wrong bytes at offset {offset}"
            );
        }
        p.close(h).unwrap();
    }
}

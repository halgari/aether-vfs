//! `readonly(p)` — demote a `ReadWrite` provider to `Read`.
//!
//! Spec §6's primitive catalog names this as the way a host protects a vanilla
//! install: the same `disk` provider that a scratch directory uses, wrapped so
//! that nothing can write through it. Without it a host has no way to express
//! "read this, never modify it" other than by not mounting anything writable at
//! all, which is not the same statement.
//!
//! **The clamp is enforced twice, and both halves are load-bearing.**
//! [`Capabilities::read_only_clamp`] makes the *declaration* honest, which is
//! what `Director::open` and `MountGraph::open` consult before they even reach a
//! provider — and it is what makes a refused write land in
//! `rejected_writes()` for spec §7's discovery workflow. The refusals in the
//! methods below then make the *behaviour* match the declaration for a caller
//! that already holds a handle, or that reached this provider through a
//! combinator which routed a write here anyway. A wrapper that only clamped
//! capabilities would be a provider whose declaration and behaviour disagree,
//! which is the defect class `LayeredProvider` and `CachingProvider` were both
//! fixed for.
//!
//! Handles pass straight through rather than being renumbered through a table
//! of this provider's own. There is nothing to translate: a handle is
//! provider-scoped, this wrapper adds no per-handle state, and a table would
//! cost a lock per read to hold a value equal to its key.

use std::sync::Arc;

use vfs_provider::{
    read_only, Capabilities, DirEntry, Handle, Provider, SetAttr, Stat, VPath, OPEN_APPEND,
    OPEN_CREATE, OPEN_TRUNC, OPEN_WRITE,
};

/// Every open flag that asks to change something. `OPEN_EXCL` is absent
/// deliberately: on its own it only qualifies `OPEN_CREATE`, which is here.
const MUTATING_FLAGS: u32 = OPEN_WRITE | OPEN_CREATE | OPEN_TRUNC | OPEN_APPEND;

/// Reads pass through; everything that would mutate answers `ST_READ_ONLY`.
pub struct ReadOnlyProvider {
    inner: Arc<dyn Provider>,
}

impl ReadOnlyProvider {
    pub fn new(inner: Arc<dyn Provider>) -> Self {
        ReadOnlyProvider { inner }
    }

    /// The wrapped provider, for a host that also reads it for diagnostics.
    pub fn inner(&self) -> &Arc<dyn Provider> {
        &self.inner
    }
}

impl Provider for ReadOnlyProvider {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities().read_only_clamp()
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        self.inner.getattr(p)
    }

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        self.inner.readdir(p)
    }

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        if flags & MUTATING_FLAGS != 0 {
            return Err(read_only());
        }
        self.inner.open(p, flags)
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        self.inner.close(h)
    }

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        self.inner.read_at(h, offset, buf)
    }

    fn read_next(&self, h: Handle, buf: &mut [u8]) -> Result<usize, i32> {
        self.inner.read_next(h, buf)
    }

    fn write_at(&self, _h: Handle, _offset: u64, _buf: &[u8]) -> Result<usize, i32> {
        Err(read_only())
    }

    fn set_len(&self, _h: Handle, _len: u64) -> Result<(), i32> {
        Err(read_only())
    }

    /// Forwarded, not refused. A flush cannot change content — no write reached
    /// the inner provider through this wrapper — and refusing it would make a
    /// caller that flushes every handle before closing it fail on a read.
    fn flush(&self, h: Handle) -> Result<(), i32> {
        self.inner.flush(h)
    }

    fn mkdir(&self, _p: VPath) -> Result<(), i32> {
        Err(read_only())
    }

    fn remove(&self, _p: VPath) -> Result<(), i32> {
        Err(read_only())
    }

    fn rename(&self, _from: VPath, _to: VPath) -> Result<(), i32> {
        Err(read_only())
    }

    fn set_attr(&self, _p: VPath, _attr: SetAttr) -> Result<(), i32> {
        Err(read_only())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs_provider::{Access, RwMemFixture, ST_READ_ONLY, OPEN_READ};

    fn ro() -> ReadOnlyProvider {
        ReadOnlyProvider::new(Arc::new(RwMemFixture::new()))
    }

    #[test]
    fn a_readonly_wrapper_over_a_writable_provider_passes_conformance() {
        // The whole point of the clamp: the suite must run the *read* cases and
        // not the write ones, because the declaration says `Read`. If the clamp
        // were missing, `assert_writable` would run and fail on the first
        // `open(OPEN_WRITE | OPEN_CREATE)` — which is the mutation check for
        // this test.
        let p: Arc<dyn Provider> = Arc::new(ro());
        vfs_provider::assert_conformance(p);
    }

    #[test]
    fn readwrite_is_demoted_to_read() {
        assert_eq!(
            RwMemFixture::new().capabilities().access,
            Access::ReadWrite,
            "the fixture has to be writable for this test to mean anything"
        );
        assert_eq!(ro().capabilities().access, Access::Read);
    }

    #[test]
    fn a_sequential_provider_is_not_promoted_by_the_clamp() {
        // `read_only_clamp` demotes ReadWrite and leaves everything else alone.
        // A `readonly(seqread)` that reported `Read` would tell the director it
        // may issue positional reads against a provider with no `read_at`.
        let seq: Arc<dyn Provider> = Arc::new(vfs_provider::conformance::SeqFixture::new());
        let p = ReadOnlyProvider::new(seq);
        assert_eq!(p.capabilities().access, Access::SeqRead);
    }

    #[test]
    fn opening_for_write_is_refused_rather_than_opened_and_then_failing() {
        let p = ro();
        for flag in [OPEN_WRITE, OPEN_CREATE, OPEN_TRUNC, OPEN_APPEND] {
            assert_eq!(
                p.open(VPath::at_default("a.txt"), flag).err(),
                Some(ST_READ_ONLY),
                "open with flag {flag:#x} must be refused at open time"
            );
        }
        // And a plain read still works, so the refusal is not a blanket one.
        let (h, size, _) = p.open(VPath::at_default("a.txt"), OPEN_READ).unwrap();
        assert_eq!(size, 5);
        p.close(h).unwrap();
    }

    #[test]
    fn every_mutating_method_answers_read_only() {
        let p = ro();
        let f = VPath::at_default("a.txt");
        assert_eq!(p.write_at(1, 0, b"x").err(), Some(ST_READ_ONLY));
        assert_eq!(p.set_len(1, 0).err(), Some(ST_READ_ONLY));
        assert_eq!(p.mkdir(f).err(), Some(ST_READ_ONLY));
        assert_eq!(p.remove(f).err(), Some(ST_READ_ONLY));
        assert_eq!(p.rename(f, VPath::at_default("b.txt")).err(), Some(ST_READ_ONLY));
        assert_eq!(p.set_attr(f, SetAttr::default()).err(), Some(ST_READ_ONLY));
    }

    #[test]
    fn the_inner_provider_is_genuinely_untouched() {
        // A wrapper that refused writes but had already let one through would
        // pass every assertion above. This is the one that would catch it.
        let inner: Arc<dyn Provider> = Arc::new(RwMemFixture::new());
        let p = ReadOnlyProvider::new(Arc::clone(&inner));
        assert!(p.open(VPath::at_default("new.txt"), OPEN_WRITE | OPEN_CREATE).is_err());
        assert!(
            inner.getattr(VPath::at_default("new.txt")).unwrap().is_none(),
            "the refused create must not have reached the inner provider"
        );
    }
}

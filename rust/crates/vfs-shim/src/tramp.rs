//! Trampoline slots: where a detour's "call the original" pointer lives.
//!
//! Each hooked export has one [`Tramp`] static. The install stores the trampoline there
//! **before** it enables the detour, and the hook body reads it on every call. It replaces 60
//! `static mut` trampolines (`Option<fn>`), whose reads and writes were each a `static_mut_refs`
//! hazard and whose "stored before enabled, cleared if enabling fails" rule was repeated by
//! hand at every install site.
//!
//! # Cost
//!
//! [`Tramp::get`] is one load of a pointer-sized word and a compare with null: the same code as
//! reading an `Option<fn>` (the null-pointer niche) from a plain static. On x86-64 an
//! `Acquire` load is an ordinary `mov`, so the hot hooks (`NtReadFile`, `NtClose`, ...) pay
//! nothing for the ordering and nothing for a lock.
//!
//! # Ordering
//!
//! `set` is `Release` and `get` is `Acquire`. The trampoline is a code stub the `retour` crate
//! built just before the store; a thread that sees the pointer must also see that stub. Enabling
//! the detour is what lets another thread reach a hook at all, and that patch has its own
//! ordering, so in practice the pair only has to be correct for the early-payload path
//! (`install_late`), where the pointers come from another module's memory. Nothing here needs
//! `SeqCst`: a slot has one writer at a time (install) and independent readers.
#![allow(unsafe_code)]

use core::marker::PhantomData;
use core::sync::atomic::{AtomicPtr, Ordering};

/// The untyped storage of a [`Tramp`]: a pointer that is null while no trampoline is stored.
///
/// This is what the install table holds, so one loop can fill slots of every function type.
/// Reading it back is only possible through the typed [`Tramp`].
pub(crate) struct RawTramp(AtomicPtr<()>);

impl RawTramp {
    const fn new() -> Self {
        RawTramp(AtomicPtr::new(core::ptr::null_mut()))
    }

    /// Store `tramp` (`None` clears the slot).
    ///
    /// # Safety
    /// A non-null `tramp` must be a callable trampoline with exactly the signature this slot's
    /// [`Tramp`] was declared with: [`Tramp::get`] hands it out as that function type.
    pub(crate) unsafe fn store(&self, tramp: Option<*const ()>) {
        let p = tramp.map_or(core::ptr::null_mut(), |p| p.cast_mut());
        self.0.store(p, Ordering::Release);
    }
}

/// A trampoline slot for a hook whose original has the function-pointer type `F`.
pub(crate) struct Tramp<F: Copy> {
    raw: RawTramp,
    _ty: PhantomData<F>,
}

impl<F: Copy> Tramp<F> {
    /// An empty slot.
    pub(crate) const fn new() -> Self {
        Tramp {
            raw: RawTramp::new(),
            _ty: PhantomData,
        }
    }

    /// The untyped slot, for the install loop.
    pub(crate) const fn raw(&self) -> &RawTramp {
        &self.raw
    }

    /// The original function, or `None` if nothing is stored (the hook is not installed, or
    /// enabling it failed). One `Acquire` load; see the module docs.
    #[inline(always)]
    pub(crate) fn get(&self) -> Option<F> {
        const { assert!(size_of::<F>() == size_of::<*const ()>()) };
        let p = self.raw.0.load(Ordering::Acquire);
        if p.is_null() {
            return None;
        }
        // SAFETY: `F` is pointer-sized (checked above) and `p` was stored from an `F` by `set`,
        // or by `RawTramp::store` under its contract that it has this slot's signature.
        Some(unsafe { core::mem::transmute_copy::<*mut (), F>(&p) })
    }

    /// Store `f` (`None` clears the slot).
    pub(crate) fn set(&self, f: Option<F>) {
        const { assert!(size_of::<F>() == size_of::<*const ()>()) };
        let p = match f {
            // SAFETY: `F` is pointer-sized (checked above); the bits are only ever read back
            // as an `F`.
            Some(f) => unsafe { core::mem::transmute_copy::<F, *mut ()>(&f) },
            None => core::ptr::null_mut(),
        };
        self.raw.0.store(p, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A plain fn type: `extern "system"` definitions are checked by the panic-containment scan.
    type F = fn(u32) -> u32;
    fn double(x: u32) -> u32 {
        x * 2
    }
    fn triple(x: u32) -> u32 {
        x * 3
    }

    #[test]
    fn an_empty_slot_reads_none() {
        let t: Tramp<F> = Tramp::new();
        assert!(t.get().is_none());
    }

    #[test]
    fn a_stored_function_reads_back_and_calls() {
        let t: Tramp<F> = Tramp::new();
        t.set(Some(double));
        assert_eq!(t.get().expect("stored")(21), 42);
        t.set(Some(triple));
        assert_eq!(t.get().expect("stored")(14), 42);
    }

    #[test]
    fn clearing_a_slot_reads_none() {
        let t: Tramp<F> = Tramp::new();
        t.set(Some(double));
        t.set(None);
        assert!(t.get().is_none());
    }

    #[test]
    fn the_untyped_store_fills_the_typed_slot() {
        let t: Tramp<F> = Tramp::new();
        // SAFETY: `double` has this slot's signature.
        unsafe { t.raw().store(Some(double as F as *const ())) };
        assert_eq!(t.get().expect("stored")(5), 10);
        // SAFETY: clearing needs no signature.
        unsafe { t.raw().store(None) };
        assert!(t.get().is_none());
    }

    /// The `Option<fn>` the slot replaces is one word; so is the slot.
    #[test]
    fn a_slot_is_one_word() {
        assert_eq!(size_of::<Tramp<F>>(), size_of::<usize>());
    }
}

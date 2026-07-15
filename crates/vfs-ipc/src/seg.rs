//! SharedSeg: the crate's entire audited unsafe surface.

use core::sync::atomic::{AtomicU32, AtomicU64};

/// A raw view over a shared-memory segment: a raw pointer (NOT derived from a
/// `&[u8]`, so interior mutation is sound) plus a length. This module is the
/// crate's ENTIRE `unsafe` surface; each site carries a `// SAFETY:` note.
pub struct SharedSeg {
    ptr: *mut u8,
    len: usize,
}

#[allow(unsafe_code)]
// SAFETY: the segment is shared memory intended for concurrent access; all
// interior mutation goes through atomics or protocol-exclusive byte writes.
unsafe impl Send for SharedSeg {}
#[allow(unsafe_code)]
unsafe impl Sync for SharedSeg {}

impl SharedSeg {
    /// # Safety
    /// `ptr` must be valid for reads/writes of `len` bytes for the whole lifetime
    /// of the returned `SharedSeg`, and 8-byte aligned.
    #[allow(unsafe_code)]
    pub unsafe fn from_raw(ptr: *mut u8, len: usize) -> Self {
        SharedSeg { ptr, len }
    }

    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn in_bounds(&self, off: usize, n: usize) -> bool {
        off.checked_add(n).map_or(false, |end| end <= self.len)
    }

    #[allow(unsafe_code)]
    pub(crate) fn atomic_u32(&self, off: usize) -> Option<&AtomicU32> {
        if !self.in_bounds(off, 4) || (self.ptr as usize + off) % 4 != 0 {
            return None;
        }
        // SAFETY: in-bounds and 4-aligned; AtomicU32 has the same layout as u32;
        // shared atomic access to shared memory is the intended use.
        Some(unsafe { &*(self.ptr.add(off) as *const AtomicU32) })
    }

    #[allow(unsafe_code)]
    pub(crate) fn atomic_u64(&self, off: usize) -> Option<&AtomicU64> {
        if !self.in_bounds(off, 8) || (self.ptr as usize + off) % 8 != 0 {
            return None;
        }
        // SAFETY: in-bounds and 8-aligned; AtomicU64 has the same layout as u64.
        Some(unsafe { &*(self.ptr.add(off) as *const AtomicU64) })
    }

    #[allow(unsafe_code)]
    pub fn write_bytes(&self, off: usize, data: &[u8]) -> bool {
        if !self.in_bounds(off, data.len()) {
            return false;
        }
        // SAFETY: in-bounds; the caller holds exclusive ownership of this slot's
        // payload region per the ring protocol (CLAIMED client / PROCESSING server).
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr.add(off), data.len());
        }
        true
    }

    /// Run `f` with a mutable view of `n` bytes at `off` (server bulk arena fill).
    ///
    /// The callback has exclusive access for the duration of the call under the
    /// ring/arena bank ownership protocol (slot CLAIMED → COMPLETED).
    #[allow(unsafe_code)]
    pub fn with_mut_bytes<R>(
        &self,
        off: usize,
        n: usize,
        f: impl FnOnce(&mut [u8]) -> R,
    ) -> Option<R> {
        if !self.in_bounds(off, n) {
            return None;
        }
        // SAFETY: in-bounds; exclusive bank/slot ownership per protocol.
        let slice = unsafe { core::slice::from_raw_parts_mut(self.ptr.add(off), n) };
        Some(f(slice))
    }

    /// Copy `dest.len()` bytes from `off` into `dest` (no intermediate `Vec`).
    #[allow(unsafe_code)]
    pub fn copy_to(&self, off: usize, dest: &mut [u8]) -> Option<()> {
        if !self.in_bounds(off, dest.len()) {
            return None;
        }
        // SAFETY: in-bounds; read of a settled region per the protocol.
        unsafe {
            core::ptr::copy_nonoverlapping(self.ptr.add(off), dest.as_mut_ptr(), dest.len());
        }
        Some(())
    }

    #[allow(unsafe_code)]
    pub fn read_bytes(&self, off: usize, n: usize) -> Option<Vec<u8>> {
        let mut v = vec![0u8; n];
        self.copy_to(off, &mut v)?;
        Some(v)
    }

    #[allow(unsafe_code)]
    pub(crate) fn read_u32(&self, off: usize) -> Option<u32> {
        if !self.in_bounds(off, 4) {
            return None;
        }
        let mut b = [0u8; 4];
        // SAFETY: in-bounds; 4-byte copy out.
        unsafe { core::ptr::copy_nonoverlapping(self.ptr.add(off), b.as_mut_ptr(), 4) };
        Some(u32::from_le_bytes(b))
    }

    #[allow(unsafe_code)]
    pub(crate) fn read_i32(&self, off: usize) -> Option<i32> {
        self.read_u32(off).map(|v| v as i32)
    }

    #[allow(unsafe_code)]
    pub(crate) fn read_u64(&self, off: usize) -> Option<u64> {
        if !self.in_bounds(off, 8) {
            return None;
        }
        let mut b = [0u8; 8];
        // SAFETY: in-bounds; 8-byte copy out.
        unsafe { core::ptr::copy_nonoverlapping(self.ptr.add(off), b.as_mut_ptr(), 8) };
        Some(u64::from_le_bytes(b))
    }

    pub(crate) fn write_u32(&self, off: usize, v: u32) -> bool {
        self.write_bytes(off, &v.to_le_bytes())
    }
    pub(crate) fn write_i32(&self, off: usize, v: i32) -> bool {
        self.write_bytes(off, &v.to_le_bytes())
    }
    pub(crate) fn write_u64(&self, off: usize, v: u64) -> bool {
        self.write_bytes(off, &v.to_le_bytes())
    }
}

/// Owns an 8-aligned heap buffer and exposes a `SharedSeg` over it. For tests and
/// single-process setups — no `unsafe` at the call site.
pub struct OwnedSeg {
    _raw: Vec<u8>,
    seg: SharedSeg,
}

impl OwnedSeg {
    #[allow(unsafe_code)]
    pub fn new(len: usize) -> Self {
        let mut raw = vec![0u8; len + 8];
        let off = (8 - (raw.as_ptr() as usize % 8)) % 8;
        // SAFETY: `raw` outlives `seg` (both owned here); its heap allocation is
        // stable across moves of `OwnedSeg`; `ptr`/`len` are in-bounds & 8-aligned.
        let seg = unsafe { SharedSeg::from_raw(raw.as_mut_ptr().add(off), len) };
        OwnedSeg { _raw: raw, seg }
    }
    pub fn seg(&self) -> &SharedSeg {
        &self.seg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::Ordering;

    #[test]
    fn atomic_and_scalar_roundtrip() {
        let owned = OwnedSeg::new(64);
        let seg = owned.seg();
        seg.atomic_u32(8).unwrap().store(0xABCD, Ordering::Relaxed);
        assert_eq!(seg.read_u32(8), Some(0xABCD));
        seg.write_u64(16, 0x0102_0304_0506_0708);
        assert_eq!(seg.atomic_u64(16).unwrap().load(Ordering::Relaxed), 0x0102_0304_0506_0708);
        seg.write_i32(24, -5);
        assert_eq!(seg.read_i32(24), Some(-5));
    }

    #[test]
    fn bytes_roundtrip_and_bounds() {
        let owned = OwnedSeg::new(32);
        let seg = owned.seg();
        assert!(seg.write_bytes(4, b"hello"));
        assert_eq!(seg.read_bytes(4, 5), Some(b"hello".to_vec()));
        // out of bounds
        assert!(!seg.write_bytes(30, b"toolong"));
        assert_eq!(seg.read_bytes(30, 8), None);
        assert_eq!(seg.read_u64(30), None);
    }

    #[test]
    fn with_mut_bytes_and_copy_to() {
        let owned = OwnedSeg::new(64);
        let seg = owned.seg();
        let n = seg
            .with_mut_bytes(8, 16, |buf| {
                buf[..5].copy_from_slice(b"world");
                5
            })
            .unwrap();
        assert_eq!(n, 5);
        let mut dest = [0u8; 5];
        assert!(seg.copy_to(8, &mut dest).is_some());
        assert_eq!(&dest, b"world");
        assert!(seg.copy_to(60, &mut [0u8; 8]).is_none());
    }

    #[test]
    fn misaligned_atomic_is_none() {
        let owned = OwnedSeg::new(32);
        let seg = owned.seg();
        // offset 2 is not 4-aligned relative to the 8-aligned base
        assert!(seg.atomic_u32(2).is_none());
        assert!(seg.atomic_u64(4).is_none());
    }
}

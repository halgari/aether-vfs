//! The contract a ring's shared-memory backing meets.

use crate::seg::SharedSeg;

/// What a ring host needs from the memory the ring lives in.
///
/// `vfs-win`'s `SharedMapping` (a named page-file section) and `vfs-unix`'s
/// `FileMapping` (an `mmap` over a real file, so a shim inside Wine and a native
/// Linux director can share one ring) both implement it. The host picks one by
/// target and writes everything above that choice once; this trait is the
/// meaning of "identical" that the two types used to share only by convention.
pub trait RingBacking: Send + Sync {
    /// The mapped region as a `SharedSeg`.
    fn seg(&self) -> &SharedSeg;

    /// The mapped length in bytes.
    fn len(&self) -> usize;

    /// Whether the mapping is zero-length (never true for a live mapping).
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Raw start of the mapped region, for carving an arena after the ring.
    fn as_mut_ptr(&self) -> *mut u8;
}

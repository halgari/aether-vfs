//! Seqlock publish / read_stable.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::layout::{H_GENERATION, H_MAGIC, HEADER_SIZE, MAGIC, read_u32};
use crate::reader::SnapshotReader;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishError {
    ImageTooLarge,
    BadImage,
    Misaligned,
}

/// A heap buffer whose exposed bytes start at an 8-byte-aligned address, so the
/// generation field (Header offset 8) is 8-aligned for atomic access. No unsafe.
pub struct AlignedBuf {
    raw: Vec<u8>,
    off: usize,
    len: usize,
}

impl AlignedBuf {
    pub fn new(len: usize) -> Self {
        // Over-allocate by 8 and expose an 8-aligned subslice. `raw`'s buffer
        // address is stable (no reallocation follows), so `off` stays valid.
        let raw = vec![0u8; len + 8];
        let off = (8 - (raw.as_ptr() as usize % 8)) % 8;
        AlignedBuf { raw, off, len }
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.raw[self.off..self.off + self.len]
    }
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.raw[self.off..self.off + self.len]
    }
}

fn is_aligned(b: &[u8]) -> bool {
    (b.as_ptr() as usize).is_multiple_of(8)
}

/// SAFETY-bearing helper: view the generation slot as an `&AtomicU64`.
#[allow(unsafe_code)]
fn generation(b: &[u8]) -> &AtomicU64 {
    debug_assert!(b.len() >= H_GENERATION + 8);
    debug_assert!(is_aligned(b));
    let ptr = b[H_GENERATION..H_GENERATION + 8].as_ptr() as *const AtomicU64;
    // SAFETY: the generation slot is in-bounds (callers validate len) and
    // 8-aligned (callers validate alignment). AtomicU64 has the same layout as
    // u64. Concurrent atomic access across threads/processes is the intended use
    // of this shared region; no non-atomic access to these 8 bytes occurs.
    unsafe { &*ptr }
}

/// Publish `image` into `shared` under the seqlock. See module docs.
pub fn publish(shared: &mut [u8], image: &[u8]) -> Result<(), PublishError> {
    if image.len() < HEADER_SIZE || read_u32(image, H_MAGIC) != Some(MAGIC) {
        return Err(PublishError::BadImage);
    }
    if image.len() > shared.len() {
        return Err(PublishError::ImageTooLarge);
    }
    if !is_aligned(shared) {
        return Err(PublishError::Misaligned);
    }
    let cur = generation(shared).load(Ordering::Relaxed);
    let odd = cur | 1;
    generation(shared).store(odd, Ordering::Release);
    // Copy everything except the 8-byte generation slot.
    shared[..H_GENERATION].copy_from_slice(&image[..H_GENERATION]);
    shared[H_GENERATION + 8..image.len()].copy_from_slice(&image[H_GENERATION + 8..image.len()]);
    let next_even = (odd + 1) & !1;
    generation(shared).store(next_even, Ordering::Release);
    Ok(())
}

/// Read `shared` under the seqlock, retrying across an overlapping publish.
/// Returns `None` if the buffer is misaligned or holds no valid snapshot.
pub fn read_stable<T>(shared: &[u8], f: impl Fn(&SnapshotReader) -> T) -> Option<T> {
    if !is_aligned(shared) || shared.len() < HEADER_SIZE {
        return None;
    }
    let gen = generation(shared);
    loop {
        let g1 = gen.load(Ordering::Acquire);
        if g1 & 1 != 0 {
            core::hint::spin_loop();
            continue;
        }
        let reader = match SnapshotReader::open(shared) {
            Ok(r) => r,
            Err(_) => {
                // No valid snapshot: distinguish "none" from "mid-publish".
                return if gen.load(Ordering::Acquire) == g1 { None } else { continue };
            }
        };
        let val = f(&reader);
        if gen.load(Ordering::Acquire) == g1 {
            return Some(val);
        }
        // else a publish overlapped; retry.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::SnapshotBuilder;
    use crate::reader::SnapResolution;

    fn image() -> Vec<u8> {
        let mut b = SnapshotBuilder::new();
        let a = b.add_file("a.esp", b"src/a", 10, 1, 0, [0; 32]);
        let root = b.add_dir("", &[("a.esp".into(), a)]);
        b.set_root(root);
        b.finish()
    }

    #[test]
    fn publish_then_read_stable() {
        let img = image();
        let mut buf = AlignedBuf::new(img.len() + 64);
        publish(buf.as_bytes_mut(), &img).unwrap();
        // generation is even after publish
        let g = read_stable(buf.as_bytes(), |r| r.generation()).unwrap();
        assert_eq!(g % 2, 0);
        assert!(g >= 2);
        // content is readable
        let res = read_stable(buf.as_bytes(), |r| r.resolve(&["a.esp"])).unwrap();
        assert!(matches!(res, SnapResolution::File { .. }));
    }

    #[test]
    fn misaligned_publish_errors() {
        let img = image();
        // Force a misaligned slice by offsetting into an aligned buffer by 1.
        let mut buf = AlignedBuf::new(img.len() + 64);
        let bytes = buf.as_bytes_mut();
        let err = publish(&mut bytes[1..], &img).unwrap_err();
        assert_eq!(err, PublishError::Misaligned);
    }

    #[test]
    fn image_too_large_errors() {
        let img = image();
        let mut buf = AlignedBuf::new(img.len() - 1);
        assert_eq!(
            publish(buf.as_bytes_mut(), &img).unwrap_err(),
            PublishError::ImageTooLarge
        );
    }

    #[test]
    fn read_stable_on_empty_buffer_is_none() {
        let buf = AlignedBuf::new(128); // all zeros → bad magic
        assert!(read_stable(buf.as_bytes(), |r| r.root()).is_none());
    }
}

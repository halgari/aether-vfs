//! Byte cursor and writer helpers shared by the codecs.

use vfs_registry::utf16_len;

pub(crate) fn put_str(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(&(s.len() as u32).to_le_bytes());
    b.extend_from_slice(s.as_bytes());
}

/// Bounds-checked cursor over a payload.
pub(crate) struct Rd<'a>(pub(crate) &'a [u8]);

impl<'a> Rd<'a> {
    pub(crate) fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (h, t) = self.0.split_at(n);
        self.0 = t;
        Some(h)
    }
    pub(crate) fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    pub(crate) fn bool(&mut self) -> Option<bool> {
        match self.u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
    pub(crate) fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    pub(crate) fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    pub(crate) fn bytes(&mut self, max: usize) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        if n > max {
            return None;
        }
        self.take(n)
    }
    pub(crate) fn str(&mut self) -> Option<&'a str> {
        let n = self.u32()? as usize;
        core::str::from_utf8(self.take(n)?).ok()
    }
    /// A string whose length in UTF-16 units is at most `max_units`.
    pub(crate) fn str_max(&mut self, max_units: usize) -> Option<&'a str> {
        let s = self.str()?;
        (utf16_len(s) <= max_units).then_some(s)
    }
    pub(crate) fn done(&self) -> Option<()> {
        self.0.is_empty().then_some(())
    }
}

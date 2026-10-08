//! Random-access byte sources.

use std::io;
use std::ops::Range;

/// A random-access byte source of known length (a file, a buffer, a
/// store-backed reader, a sub-range of another source).
pub trait RangeRead {
    /// Fill `buf` with the bytes at `off..off + buf.len()`. Fails with
    /// `UnexpectedEof` if that range is not entirely inside the source.
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()>;
    /// Total length in bytes.
    fn len(&self) -> u64;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn eof(off: u64, n: usize, len: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        format!("read of {n} bytes at {off} past end of {len}-byte source"),
    )
}

impl RangeRead for [u8] {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let len = <[u8]>::len(self) as u64;
        let end = off
            .checked_add(buf.len() as u64)
            .ok_or_else(|| eof(off, buf.len(), len))?;
        if end > len {
            return Err(eof(off, buf.len(), len));
        }
        buf.copy_from_slice(&self[off as usize..end as usize]);
        Ok(())
    }
    fn len(&self) -> u64 {
        <[u8]>::len(self) as u64
    }
}

impl RangeRead for Vec<u8> {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        self.as_slice().read_at(off, buf)
    }
    fn len(&self) -> u64 {
        Vec::len(self) as u64
    }
}

impl<T: RangeRead + ?Sized> RangeRead for &T {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        (**self).read_at(off, buf)
    }
    fn len(&self) -> u64 {
        (**self).len()
    }
}

impl<T: RangeRead + ?Sized> RangeRead for std::sync::Arc<T> {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        (**self).read_at(off, buf)
    }
    fn len(&self) -> u64 {
        (**self).len()
    }
}

impl<T: RangeRead + ?Sized> RangeRead for Box<T> {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        (**self).read_at(off, buf)
    }
    fn len(&self) -> u64 {
        (**self).len()
    }
}

/// Read `len` bytes at `off` into a new buffer. The bounds are checked
/// before allocating, so a corrupt length field cannot trigger a huge
/// allocation.
pub fn read_vec<R: RangeRead + ?Sized>(r: &R, off: u64, len: u64) -> io::Result<Vec<u8>> {
    let total = r.len();
    match off.checked_add(len) {
        Some(end) if end <= total => {}
        _ => return Err(eof(off, len as usize, total)),
    }
    let mut buf = vec![0u8; len as usize];
    r.read_at(off, &mut buf)?;
    Ok(buf)
}

/// A window `start..start + len` of another source.
#[derive(Debug, Clone)]
pub struct SubRange<R> {
    inner: R,
    start: u64,
    len: u64,
}

impl<R: RangeRead> SubRange<R> {
    pub fn new(inner: R, range: Range<u64>) -> io::Result<Self> {
        if range.start > range.end || range.end > inner.len() {
            return Err(eof(
                range.start,
                (range.end.saturating_sub(range.start)) as usize,
                inner.len(),
            ));
        }
        Ok(SubRange {
            inner,
            start: range.start,
            len: range.end - range.start,
        })
    }
}

impl<R: RangeRead> RangeRead for SubRange<R> {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let end = off
            .checked_add(buf.len() as u64)
            .ok_or_else(|| eof(off, buf.len(), self.len))?;
        if end > self.len {
            return Err(eof(off, buf.len(), self.len));
        }
        self.inner.read_at(self.start + off, buf)
    }
    fn len(&self) -> u64 {
        self.len
    }
}

/// A file opened for positioned reads.
#[derive(Debug)]
pub struct FileRange {
    file: std::fs::File,
    len: u64,
}

impl FileRange {
    pub fn open(path: impl AsRef<std::path::Path>) -> io::Result<Self> {
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        Ok(FileRange { file, len })
    }
}

impl RangeRead for FileRange {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let end = off
            .checked_add(buf.len() as u64)
            .ok_or_else(|| eof(off, buf.len(), self.len))?;
        if end > self.len {
            return Err(eof(off, buf.len(), self.len));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file.read_exact_at(buf, off)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            let mut done = 0;
            while done < buf.len() {
                let n = self.file.seek_read(&mut buf[done..], off + done as u64)?;
                if n == 0 {
                    return Err(eof(off, buf.len(), self.len));
                }
                done += n;
            }
            Ok(())
        }
    }
    fn len(&self) -> u64 {
        self.len
    }
}

/// A `Read + Seek` cursor over a `RangeRead`, for libraries that want a
/// stream (the `zip` and `sevenz-rust2` crates).
#[derive(Debug, Clone)]
pub struct RangeCursor<R> {
    inner: R,
    pos: u64,
}

impl<R: RangeRead> RangeCursor<R> {
    pub fn new(inner: R) -> Self {
        RangeCursor { inner, pos: 0 }
    }
    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: RangeRead> io::Read for RangeCursor<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.inner.len().saturating_sub(self.pos);
        let n = (buf.len() as u64).min(left) as usize;
        if n == 0 {
            return Ok(0);
        }
        self.inner.read_at(self.pos, &mut buf[..n])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: RangeRead> io::Seek for RangeCursor<R> {
    fn seek(&mut self, to: io::SeekFrom) -> io::Result<u64> {
        let (base, delta) = match to {
            io::SeekFrom::Start(p) => {
                self.pos = p;
                return Ok(p);
            }
            io::SeekFrom::End(d) => (self.inner.len(), d),
            io::SeekFrom::Current(d) => (self.pos, d),
        };
        self.pos = base.checked_add_signed(delta).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "seek before start of source")
        })?;
        Ok(self.pos)
    }
}

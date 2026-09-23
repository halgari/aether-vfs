//! Pack files: append-only files of block records.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

pub fn pack_path(dir: &Path, id: u32) -> PathBuf {
    dir.join(format!("{id:08}.pack"))
}

/// Parses a pack file name such as `00000012.pack` into its id.
pub fn parse_pack_name(name: &str) -> Option<u32> {
    let stem = name.strip_suffix(".pack")?;
    if stem.len() != 8 || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    stem.parse().ok()
}

/// Deletes a pack file. A file that is already gone counts as deleted.
pub fn remove_pack_file(dir: &Path, id: u32) -> io::Result<()> {
    match std::fs::remove_file(pack_path(dir, id)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Lists pack ids present in `dir`.
pub fn list_pack_ids(dir: &Path) -> io::Result<Vec<u32>> {
    let mut ids = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        if let Some(id) = entry?.file_name().to_str().and_then(parse_pack_name) {
            ids.push(id);
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Shared read handles to pack files, opened on first use.
pub struct PackFiles {
    dir: PathBuf,
    handles: RwLock<HashMap<u32, Arc<File>>>,
}

impl PackFiles {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            handles: RwLock::new(HashMap::new()),
        }
    }

    pub fn get(&self, id: u32) -> io::Result<Arc<File>> {
        if let Some(f) = self.handles.read().unwrap().get(&id) {
            return Ok(f.clone());
        }
        let file = Arc::new(File::open(pack_path(&self.dir, id))?);
        Ok(self
            .handles
            .write()
            .unwrap()
            .entry(id)
            .or_insert(file)
            .clone())
    }

    /// Positioned read; safe to call from many threads at once.
    pub fn read_exact_at(&self, id: u32, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let file = self.get(id)?;
        read_exact_at(&file, buf, offset)
    }

    /// Drops the cached handle so the file can be deleted (required on Windows).
    pub fn close(&self, id: u32) {
        self.handles.write().unwrap().remove(&id);
    }
}

struct ActivePack {
    id: u32,
    out: BufWriter<File>,
    len: u64,
}

/// Appends records to the single active pack.
///
/// After an I/O error in `append`, `flush_buffer` or `sync`, the active pack is abandoned: its
/// buffered bytes are dropped and nothing more is appended to it, so the offsets handed out for
/// later records always match the file. Its tail may hold a partial record, which compaction
/// tolerates. The next record starts a new pack, and the abandoned pack is sealed then.
pub struct PackWriter {
    dir: PathBuf,
    max_pack_size: u64,
    active: Option<ActivePack>,
    /// The pack abandoned after an I/O error, kept open so its written bytes can still be
    /// synced. Never set while `active` is.
    abandoned: Option<(u32, File)>,
    /// Test hook: the next `append` writes part of its record, then fails.
    #[cfg(test)]
    pub(crate) fail_next_append: bool,
}

impl PackWriter {
    pub fn new(dir: PathBuf, max_pack_size: u64) -> Self {
        Self {
            dir,
            max_pack_size,
            active: None,
            abandoned: None,
            #[cfg(test)]
            fail_next_append: false,
        }
    }

    /// Continues appending to an existing pack after a clean shutdown.
    pub fn resume(&mut self, id: u32) -> io::Result<()> {
        let file = OpenOptions::new()
            .append(true)
            .open(pack_path(&self.dir, id))?;
        let len = file.metadata()?.len();
        self.active = Some(ActivePack {
            id,
            out: BufWriter::with_capacity(1 << 20, file),
            len,
        });
        Ok(())
    }

    pub fn active_id(&self) -> Option<u32> {
        self.active.as_ref().map(|a| a.id)
    }

    /// The active pack, or else the pack abandoned after an I/O error: the pack to seal when
    /// the next pack starts.
    pub fn current_id(&self) -> Option<u32> {
        self.active_id()
            .or(self.abandoned.as_ref().map(|(id, _)| *id))
    }

    /// True if a record of `record_len` bytes needs a new pack first.
    /// An empty pack always accepts one record, however large.
    pub fn needs_new_pack(&self, record_len: u64) -> bool {
        match &self.active {
            None => true,
            Some(a) => a.len > 0 && a.len + record_len > self.max_pack_size,
        }
    }

    /// Stops appending to the active pack and drops its buffered bytes.
    fn abandon(&mut self) {
        if let Some(a) = self.active.take() {
            let (file, _unwritten) = a.out.into_parts();
            tracing::warn!(pack = a.id, "abandoning pack after an i/o error");
            self.abandoned = Some((a.id, file));
        }
    }

    /// Syncs and closes the current pack (if any) and creates pack `id`. Returns the previous pack id.
    pub fn start_pack(&mut self, id: u32) -> io::Result<Option<u32>> {
        self.sync()?;
        let old = match self.active.take() {
            Some(a) => Some(a.id),
            None => self.abandoned.take().map(|(id, _)| id),
        };
        let file = OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(pack_path(&self.dir, id))?;
        self.active = Some(ActivePack {
            id,
            out: BufWriter::with_capacity(1 << 20, file),
            len: 0,
        });
        Ok(old)
    }

    /// Appends one record (header + payload). Returns (pack id, offset).
    pub fn append(&mut self, header: &[u8], payload: &[u8]) -> io::Result<(u32, u64)> {
        let a = self.active.as_mut().expect("append without an active pack");
        let offset = a.len;
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_append) {
            let _ = a.out.write_all(header);
            let _ = a.out.write_all(&payload[..payload.len() / 2]);
            self.abandon();
            return Err(io::Error::other("injected append failure"));
        }
        match a
            .out
            .write_all(header)
            .and_then(|()| a.out.write_all(payload))
        {
            Ok(()) => {
                a.len += (header.len() + payload.len()) as u64;
                Ok((a.id, offset))
            }
            Err(e) => {
                self.abandon();
                Err(e)
            }
        }
    }

    /// Hands buffered bytes to the OS so readers can see them (no fsync).
    pub fn flush_buffer(&mut self) -> io::Result<()> {
        if let Some(a) = &mut self.active
            && let Err(e) = a.out.flush()
        {
            self.abandon();
            return Err(e);
        }
        Ok(())
    }

    /// Flushes and fsyncs the active pack, or the abandoned pack if there is no active one.
    pub fn sync(&mut self) -> io::Result<()> {
        if let Some(a) = &mut self.active {
            if let Err(e) = a.out.flush().and_then(|()| a.out.get_ref().sync_data()) {
                self.abandon();
                return Err(e);
            }
        } else if let Some((_, file)) = &self.abandoned {
            file.sync_data()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pack_names() {
        assert_eq!(parse_pack_name("00000012.pack"), Some(12));
        assert_eq!(parse_pack_name("12.pack"), None);
        assert_eq!(parse_pack_name("0000001x.pack"), None);
        assert_eq!(parse_pack_name("00000012.tmp"), None);
        assert_eq!(
            pack_path(Path::new("p"), 7),
            Path::new("p").join("00000007.pack")
        );
    }

    #[test]
    fn append_rotate_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = PackWriter::new(dir.path().to_path_buf(), 100);
        assert!(w.needs_new_pack(10));
        assert_eq!(w.start_pack(1).unwrap(), None);
        assert!(!w.needs_new_pack(200)); // empty pack accepts anything
        assert_eq!(w.append(b"head", b"payload").unwrap(), (1, 0));
        assert_eq!(w.append(b"h2", b"p2").unwrap(), (1, 11));
        assert!(w.needs_new_pack(90));
        w.flush_buffer().unwrap();

        let files = PackFiles::new(dir.path().to_path_buf());
        let mut buf = [0u8; 7];
        files.read_exact_at(1, &mut buf, 4).unwrap();
        assert_eq!(&buf, b"payload");
        assert!(files.read_exact_at(1, &mut [0u8; 10], 10).is_err());

        assert_eq!(w.start_pack(2).unwrap(), Some(1));
        assert_eq!(w.active_id(), Some(2));
        assert_eq!(list_pack_ids(dir.path()).unwrap(), vec![1, 2]);
    }

    #[test]
    fn resume_continues_at_end() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = PackWriter::new(dir.path().to_path_buf(), 1000);
        w.start_pack(5).unwrap();
        w.append(b"abc", b"def").unwrap();
        w.sync().unwrap();
        drop(w);
        let mut w = PackWriter::new(dir.path().to_path_buf(), 1000);
        w.resume(5).unwrap();
        assert_eq!(w.append(b"x", b"y").unwrap(), (5, 6));
    }

    #[test]
    fn failed_append_abandons_the_pack() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = PackWriter::new(dir.path().to_path_buf(), 1000);
        w.start_pack(1).unwrap();
        assert_eq!(w.append(b"head", b"payload").unwrap(), (1, 0));
        w.flush_buffer().unwrap(); // as append_records does after each batch
        w.fail_next_append = true;
        assert!(w.append(b"head", b"payload").is_err());
        // The pack is abandoned: nothing more is appended to it.
        assert_eq!(w.active_id(), None);
        assert_eq!(w.current_id(), Some(1));
        assert!(w.needs_new_pack(1));
        w.sync().unwrap();
        assert_eq!(w.start_pack(2).unwrap(), Some(1));
        assert_eq!(w.current_id(), Some(2));
        assert_eq!(w.append(b"h2", b"p2").unwrap(), (2, 0));
        w.flush_buffer().unwrap();
        let files = PackFiles::new(dir.path().to_path_buf());
        let mut buf = [0u8; 4];
        files.read_exact_at(2, &mut buf, 0).unwrap();
        assert_eq!(&buf, b"h2p2");
        // The buffered bytes of the failed record were dropped, not written.
        assert_eq!(
            std::fs::metadata(pack_path(dir.path(), 1)).unwrap().len(),
            11
        );
    }

    #[test]
    fn closed_pack_can_be_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = PackWriter::new(dir.path().to_path_buf(), 1000);
        w.start_pack(1).unwrap();
        w.append(b"a", b"b").unwrap();
        w.start_pack(2).unwrap();
        let files = PackFiles::new(dir.path().to_path_buf());
        files.read_exact_at(1, &mut [0u8; 2], 0).unwrap();
        files.close(1);
        std::fs::remove_file(pack_path(dir.path(), 1)).unwrap();
    }
}

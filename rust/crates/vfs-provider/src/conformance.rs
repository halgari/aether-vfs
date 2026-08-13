//! One conformance suite, run against every provider in every language.
//!
//! Cases are selected by the provider's *declared* capabilities: a provider
//! that declares `Access::Read` is held to the positional-read cases and not
//! to the sequential ones. Bindings expose [`assert_conformance`] so a
//! host-language provider is held to exactly the same standard as a Rust one.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::{
    map_io_err, not_found, Access, Capabilities, DirEntry, Handle, Provider, RootId, Stat, VPath,
    KIND_DIR, KIND_FILE,
};

/// The reference tree every conformance-tested provider must expose.
pub const FIXTURE_FILES: &[(&str, &[u8])] = &[("a.txt", b"hello"), ("sub/b.txt", b"world!")];

/// Write the reference tree to a real directory, for disk-like providers.
pub fn write_fixture_tree(dir: &Path) {
    if dir.exists() {
        std::fs::remove_dir_all(dir).expect("clear the fixture tree");
    }
    std::fs::create_dir_all(dir.join("sub")).expect("create fixture tree");
    for (rel, body) in FIXTURE_FILES {
        let p = dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        std::fs::write(p, body).expect("write fixture file");
    }
}

/// In-memory reference provider, used to test the suite itself. Root-blind by
/// design — it serves the same tree under every root id, which is one of the
/// two legal behaviors the suite accepts (see `assert_common`'s non-default-
/// root case). `PerRootFixture` in the test module below covers the other
/// legal behavior and verifies the root id actually reaches the provider.
pub struct MemFixture {
    files: HashMap<String, Vec<u8>>,
    next: AtomicU64,
    opens: Mutex<HashMap<Handle, Vec<u8>>>,
}

impl MemFixture {
    pub fn new() -> Self {
        Self::build(None)
    }

    /// A fixture missing one path, to prove the suite detects a gap.
    pub fn missing(path: &str) -> Self {
        Self::build(Some(path.to_string()))
    }

    fn build(omit: Option<String>) -> Self {
        let mut files = HashMap::new();
        for (rel, body) in FIXTURE_FILES {
            if omit.as_deref() == Some(*rel) {
                continue;
            }
            files.insert((*rel).to_string(), body.to_vec());
        }
        MemFixture { files, next: AtomicU64::new(1), opens: Mutex::new(HashMap::new()) }
    }
}

impl Default for MemFixture {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for MemFixture {
    fn capabilities(&self) -> Capabilities {
        Capabilities::read_only()
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        if p.rel.is_empty() || p.rel == "sub" {
            return Ok(Some(Stat { kind: KIND_DIR, size: 0, mtime: 0 }));
        }
        Ok(self
            .files
            .get(p.rel)
            .map(|b| Stat { kind: KIND_FILE, size: b.len() as u64, mtime: 0 }))
    }

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        let prefix = if p.rel.is_empty() { String::new() } else { format!("{}/", p.rel) };
        let mut seen: HashMap<String, DirEntry> = HashMap::new();
        for (rel, body) in &self.files {
            let Some(rest) = rel.strip_prefix(&prefix) else { continue };
            match rest.split_once('/') {
                Some((dir, _)) => {
                    seen.entry(dir.to_string()).or_insert(DirEntry {
                        name: dir.to_string(),
                        stat: Stat { kind: KIND_DIR, size: 0, mtime: 0 },
                    });
                }
                None => {
                    seen.insert(
                        rest.to_string(),
                        DirEntry {
                            name: rest.to_string(),
                            stat: Stat { kind: KIND_FILE, size: body.len() as u64, mtime: 0 },
                        },
                    );
                }
            }
        }
        if seen.is_empty() && !p.rel.is_empty() {
            return Err(not_found());
        }
        let mut out: Vec<DirEntry> = seen.into_values().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn open(&self, p: VPath, _flags: u32) -> Result<(Handle, u64, bool), i32> {
        let body = self.files.get(p.rel).ok_or_else(not_found)?.clone();
        let size = body.len() as u64;
        let h = self.next.fetch_add(1, Ordering::Relaxed);
        self.opens.lock().map_err(|_| map_io_err())?.insert(h, body);
        Ok((h, size, false))
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        self.opens.lock().map_err(|_| map_io_err())?.remove(&h);
        Ok(())
    }

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let g = self.opens.lock().map_err(|_| map_io_err())?;
        let body = g.get(&h).ok_or_else(crate::bad_fh)?;
        let start = (offset as usize).min(body.len());
        let n = (body.len() - start).min(buf.len());
        buf[..n].copy_from_slice(&body[start..start + n]);
        Ok(n)
    }
}

/// Sequential reference provider, used to give `assert_sequential` standing
/// coverage. Composed from [`MemFixture`] rather than duplicating its tree
/// walking: `getattr` and `readdir` delegate straight through, and `open`/
/// `read_next` are implemented on top of `MemFixture`'s own `open`/`read_at`,
/// tracking only the forward cursor `read_next` needs. Deliberately does not
/// implement `read_at`, so it inherits the trait's `ST_NOT_SUPPORTED`
/// default — correct for a provider that can only read forward, and the
/// suite (`assert_sequential`'s positional-read-refused case) checks it.
pub struct SeqFixture {
    inner: MemFixture,
    next: AtomicU64,
    /// Our handle -> (inner `MemFixture` handle, forward cursor).
    opens: Mutex<HashMap<Handle, (Handle, u64)>>,
}

impl SeqFixture {
    pub fn new() -> Self {
        SeqFixture { inner: MemFixture::new(), next: AtomicU64::new(1), opens: Mutex::new(HashMap::new()) }
    }
}

impl Default for SeqFixture {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for SeqFixture {
    fn capabilities(&self) -> Capabilities {
        Capabilities { access: Access::SeqRead, immutable: true, ..Capabilities::read_only() }
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        self.inner.getattr(p)
    }

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        self.inner.readdir(p)
    }

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        let (inner_h, size, is_dir) = self.inner.open(p, flags)?;
        let h = self.next.fetch_add(1, Ordering::Relaxed);
        self.opens.lock().map_err(|_| map_io_err())?.insert(h, (inner_h, 0));
        Ok((h, size, is_dir))
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        let inner_h = self.opens.lock().map_err(|_| map_io_err())?.remove(&h).map(|(ih, _)| ih);
        match inner_h {
            Some(ih) => self.inner.close(ih),
            None => Ok(()),
        }
    }

    fn read_next(&self, h: Handle, buf: &mut [u8]) -> Result<usize, i32> {
        let mut g = self.opens.lock().map_err(|_| map_io_err())?;
        let (inner_h, cursor) = g.get_mut(&h).ok_or_else(crate::bad_fh)?;
        let n = self.inner.read_at(*inner_h, *cursor, buf)?;
        *cursor += n as u64;
        Ok(n)
    }
}

/// In-memory `ReadWrite` reference provider. The `FIXTURE_FILES` tree is served
/// read-only from an inner `MemFixture`; written paths live in `extra`, so the
/// read cases keep seeing the exact reference tree.
pub struct RwMemFixture {
    base: MemFixture,
    extra: Mutex<HashMap<String, Vec<u8>>>,
    dirs: Mutex<Vec<String>>,
    next: AtomicU64,
    opens: Mutex<HashMap<Handle, String>>,
    discard: bool,
}

impl RwMemFixture {
    pub fn new() -> Self {
        Self::build(false)
    }

    /// Accepts writes and drops them — proves the suite catches a provider
    /// whose writes do not stick.
    pub fn discarding_writes() -> Self {
        Self::build(true)
    }

    fn build(discard: bool) -> Self {
        RwMemFixture {
            base: MemFixture::new(),
            extra: Mutex::new(HashMap::new()),
            dirs: Mutex::new(Vec::new()),
            next: AtomicU64::new(1),
            opens: Mutex::new(HashMap::new()),
            discard,
        }
    }
}

impl Default for RwMemFixture {
    fn default() -> Self {
        Self::new()
    }
}

/// Copy `body[offset..]` into `buf`, clamped like a positional read at EOF.
fn copy_at(body: &[u8], offset: u64, buf: &mut [u8]) -> usize {
    let start = (offset as usize).min(body.len());
    let n = (body.len() - start).min(buf.len());
    buf[..n].copy_from_slice(&body[start..start + n]);
    n
}

impl Provider for RwMemFixture {
    fn capabilities(&self) -> Capabilities {
        Capabilities { access: Access::ReadWrite, immutable: false, slow: false, preferred_block: None }
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        if let Some(body) = self.extra.lock().map_err(|_| map_io_err())?.get(p.rel) {
            return Ok(Some(Stat { kind: KIND_FILE, size: body.len() as u64, mtime: 0 }));
        }
        if self.dirs.lock().map_err(|_| map_io_err())?.iter().any(|d| d == p.rel) {
            return Ok(Some(Stat { kind: KIND_DIR, size: 0, mtime: 0 }));
        }
        self.base.getattr(p)
    }

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        let prefix = if p.rel.is_empty() { String::new() } else { format!("{}/", p.rel) };
        let mut seen: HashMap<String, DirEntry> = HashMap::new();

        match self.base.readdir(p) {
            Ok(entries) => {
                for e in entries {
                    seen.insert(e.name.clone(), e);
                }
            }
            Err(e) if e == not_found() => {}
            Err(e) => return Err(e),
        }

        for (rel, body) in self.extra.lock().map_err(|_| map_io_err())?.iter() {
            let Some(rest) = rel.strip_prefix(prefix.as_str()) else { continue };
            if rest.is_empty() || rest.contains('/') {
                continue;
            }
            seen.insert(
                rest.to_string(),
                DirEntry {
                    name: rest.to_string(),
                    stat: Stat { kind: KIND_FILE, size: body.len() as u64, mtime: 0 },
                },
            );
        }

        for d in self.dirs.lock().map_err(|_| map_io_err())?.iter() {
            let Some(rest) = d.strip_prefix(prefix.as_str()) else { continue };
            if rest.is_empty() || rest.contains('/') {
                continue;
            }
            seen.entry(rest.to_string()).or_insert(DirEntry {
                name: rest.to_string(),
                stat: Stat { kind: KIND_DIR, size: 0, mtime: 0 },
            });
        }

        if seen.is_empty() && !p.rel.is_empty() {
            return Err(not_found());
        }
        let mut out: Vec<DirEntry> = seen.into_values().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        let mut extra = self.extra.lock().map_err(|_| map_io_err())?;
        let exists = extra.contains_key(p.rel) || self.base.files.contains_key(p.rel);

        if flags & crate::OPEN_EXCL != 0 && exists {
            return Err(crate::bad_request());
        }
        if flags & crate::OPEN_CREATE != 0 {
            extra.entry(p.rel.to_string()).or_default();
        } else if !exists {
            return Err(not_found());
        }
        if flags & crate::OPEN_TRUNC != 0 {
            extra.insert(p.rel.to_string(), Vec::new());
        }

        let size = extra
            .get(p.rel)
            .map(|b| b.len())
            .or_else(|| self.base.files.get(p.rel).map(|b| b.len()))
            .unwrap_or(0) as u64;
        drop(extra);

        let h = self.next.fetch_add(1, Ordering::Relaxed);
        self.opens.lock().map_err(|_| map_io_err())?.insert(h, p.rel.to_string());
        Ok((h, size, false))
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        self.opens.lock().map_err(|_| map_io_err())?.remove(&h);
        Ok(())
    }

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let path = self.opens.lock().map_err(|_| map_io_err())?.get(&h).cloned().ok_or_else(crate::bad_fh)?;
        let extra = self.extra.lock().map_err(|_| map_io_err())?;
        if let Some(body) = extra.get(&path) {
            return Ok(copy_at(body, offset, buf));
        }
        drop(extra);
        if let Some(body) = self.base.files.get(&path) {
            return Ok(copy_at(body, offset, buf));
        }
        Err(crate::bad_fh())
    }

    fn write_at(&self, h: Handle, offset: u64, buf: &[u8]) -> Result<usize, i32> {
        let path = self.opens.lock().map_err(|_| map_io_err())?.get(&h).cloned().ok_or_else(crate::bad_fh)?;
        let mut extra = self.extra.lock().map_err(|_| map_io_err())?;
        let body = extra.entry(path).or_default();
        let end = offset as usize + buf.len();
        if body.len() < end {
            body.resize(end, 0);
        }
        // A discarding fixture still tracks size (so getattr looks correct)
        // but never actually stores the bytes — that gap is only visible on
        // read-back, which is exactly what the suite must catch.
        if !self.discard {
            body[offset as usize..end].copy_from_slice(buf);
        }
        Ok(buf.len())
    }

    fn set_len(&self, h: Handle, len: u64) -> Result<(), i32> {
        let path = self.opens.lock().map_err(|_| map_io_err())?.get(&h).cloned().ok_or_else(crate::bad_fh)?;
        self.extra.lock().map_err(|_| map_io_err())?.entry(path).or_default().resize(len as usize, 0);
        Ok(())
    }

    fn flush(&self, _h: Handle) -> Result<(), i32> {
        Ok(())
    }

    fn mkdir(&self, p: VPath) -> Result<(), i32> {
        self.dirs.lock().map_err(|_| map_io_err())?.push(p.rel.to_string());
        Ok(())
    }

    fn remove(&self, p: VPath) -> Result<(), i32> {
        let had_file = self.extra.lock().map_err(|_| map_io_err())?.remove(p.rel).is_some();
        let mut dirs = self.dirs.lock().map_err(|_| map_io_err())?;
        let before = dirs.len();
        dirs.retain(|d| d != p.rel);
        let had_dir = dirs.len() != before;
        drop(dirs);
        if had_file || had_dir {
            Ok(())
        } else {
            Err(not_found())
        }
    }

    fn rename(&self, from: VPath, to: VPath) -> Result<(), i32> {
        if from.root != to.root {
            return Err(crate::bad_request());
        }
        let mut extra = self.extra.lock().map_err(|_| map_io_err())?;
        let body = extra.remove(from.rel).ok_or_else(not_found)?;
        extra.insert(to.rel.to_string(), body);
        Ok(())
    }

    fn set_attr(&self, _p: VPath, _attr: crate::SetAttr) -> Result<(), i32> {
        Ok(())
    }
}

/// Read every byte of an open handle, looping over short reads.
fn read_all(p: &Arc<dyn Provider>, h: Handle, size: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(size as usize);
    let mut buf = [0u8; 3]; // deliberately small: forces the short-read loop
    let mut off = 0u64;
    loop {
        let n = p.read_at(h, off, &mut buf).expect("read_at");
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        off += n as u64;
        assert!(
            out.len() <= size as usize,
            "read_at returned more than the file's {size} bytes — the provider is \
             probably ignoring the offset and re-serving the same block"
        );
    }
    out
}

/// Run the conformance cases implied by `p`'s declared capabilities.
///
/// Panics with a message naming the failing case. `p` must expose
/// [`FIXTURE_FILES`] under every root.
pub fn assert_conformance(p: Arc<dyn Provider>) {
    let caps = p.capabilities();
    caps.validate().expect("capabilities: self-contradictory declaration");

    assert_eq!(
        p.capabilities(),
        caps,
        "capabilities must be constant for the provider's lifetime"
    );

    assert_common(&p);
    match caps.access {
        Access::SeqRead => assert_sequential(&p),
        Access::Read | Access::ReadWrite => assert_positional(&p),
    }
    if caps.access == Access::ReadWrite {
        assert_writable(&p); // last: these cases mutate
    }
}

fn assert_common(p: &Arc<dyn Provider>) {
    // Root of the provider is the empty string and is a directory.
    let root = p
        .getattr(VPath::at_default(""))
        .expect("getattr: provider root")
        .expect("getattr: provider root must exist");
    assert_eq!(root.kind, KIND_DIR, "the provider root must be a directory");

    // Every fixture file is visible with the right size.
    for (rel, body) in FIXTURE_FILES {
        let st = p
            .getattr(VPath::at_default(rel))
            .unwrap_or_else(|e| panic!("getattr({rel}) failed with status {e}"))
            .unwrap_or_else(|| panic!("getattr({rel}) reported the file missing"));
        assert_eq!(st.kind, KIND_FILE, "getattr({rel}) should report a file");
        assert_eq!(st.size, body.len() as u64, "getattr({rel}) size mismatch");
    }

    // An absent path is Ok(None), not an error.
    assert!(
        p.getattr(VPath::at_default("nope.txt")).expect("getattr: absent path must not error").is_none(),
        "getattr of an absent path must report None"
    );

    // Opening an absent path fails with NOT_FOUND, not some other error and
    // not success. The old vfs-source suite asserted this; six ports depend
    // on it staying true.
    match p.open(VPath::at_default("nope.txt"), crate::OPEN_READ) {
        Err(e) if e == crate::not_found() => {}
        Err(e) => panic!("open of an absent path returned status {e}, expected ST_NOT_FOUND"),
        Ok((h, _, _)) => {
            let _ = p.close(h);
            panic!("open of an absent path succeeded; it must fail with ST_NOT_FOUND");
        }
    }

    // readdir of the root lists both entries, with correct stat info.
    let entries = p.readdir(VPath::at_default("")).expect("readdir: provider root");
    let mut names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["a.txt", "sub"], "readdir of the root listed {names:?}");

    for entry in &entries {
        if entry.name == "sub" {
            assert_eq!(
                entry.stat.kind, KIND_DIR,
                "readdir(root) listed 'sub' with kind {:?}, expected KIND_DIR",
                entry.stat.kind
            );
            continue;
        }
        if let Some((_, body)) = FIXTURE_FILES.iter().find(|(rel, _)| *rel == entry.name) {
            assert_eq!(
                entry.stat.kind, KIND_FILE,
                "readdir(root) listed {:?} with kind {:?}, expected KIND_FILE",
                entry.name, entry.stat.kind
            );
            assert_eq!(
                entry.stat.size,
                body.len() as u64,
                "readdir(root) listed {:?} with the wrong size",
                entry.name
            );
        }
    }

    // getattr on the subdirectory agrees it is a directory.
    let sub_attr = p
        .getattr(VPath::at_default("sub"))
        .expect("getattr: sub")
        .expect("getattr: sub must exist");
    assert_eq!(sub_attr.kind, KIND_DIR, "getattr(sub) should report a directory");

    // readdir of a subdirectory.
    let sub = p.readdir(VPath::at_default("sub")).expect("readdir: sub");
    assert_eq!(sub.len(), 1, "readdir(sub) should list exactly one entry");
    assert_eq!(sub[0].name, "b.txt");

    // A non-default root must be handled coherently. Both answers are legal:
    // a provider over one backing store (a zip, a directory) correctly ignores
    // the root id and returns the same tree, while a multi-root provider may
    // report not-found for a root it does not serve. What is not legal is
    // panicking or returning an unrelated status.
    match p.getattr(VPath::new(RootId(7), "a.txt")) {
        Ok(_) => {}
        Err(e) if e == crate::not_found() => {}
        Err(e) => panic!(
            "getattr under a non-default root returned status {e}; expected Ok or ST_NOT_FOUND"
        ),
    }

    // Handles are provider-scoped: two opens are independent.
    let (h1, _, _) = p.open(VPath::at_default("a.txt"), crate::OPEN_READ).expect("open #1");
    let (h2, _, _) = p.open(VPath::at_default("a.txt"), crate::OPEN_READ).expect("open #2");
    assert_ne!(h1, h2, "two concurrent opens must yield distinct handles");
    p.close(h1).expect("close #1");
    p.close(h2).expect("close #2");
}

fn assert_positional(p: &Arc<dyn Provider>) {
    for (rel, body) in FIXTURE_FILES {
        let (h, size, is_dir) = p.open(VPath::at_default(rel), crate::OPEN_READ).expect("open");
        assert!(!is_dir, "open({rel}) reported a directory");
        assert_eq!(size, body.len() as u64, "open({rel}) size mismatch");

        assert_eq!(read_all(p, h, size), *body, "read_at({rel}) content mismatch");

        // Reading at EOF yields zero, not an error.
        assert_eq!(
            p.read_at(h, size, &mut [0u8; 4]).expect("read_at at EOF must not error"),
            0,
            "read_at at EOF must return 0"
        );

        // Reading past EOF yields zero too.
        assert_eq!(
            p.read_at(h, size + 100, &mut [0u8; 4]).expect("read_at past EOF must not error"),
            0,
            "read_at past EOF must return 0"
        );

        // A zero-length buffer reads zero bytes.
        assert_eq!(
            p.read_at(h, 0, &mut []).expect("read_at with an empty buffer must not error"),
            0
        );

        // An unaligned mid-file read returns the right bytes.
        if body.len() >= 3 {
            let mut buf = [0u8; 2];
            let n = p.read_at(h, 1, &mut buf).expect("unaligned read_at");
            assert!(n > 0, "unaligned read_at({rel}) returned 0 bytes for a mid-file offset");
            assert_eq!(&buf[..n], &body[1..1 + n], "unaligned read_at({rel}) content mismatch");
        }

        p.close(h).expect("close");
    }

    // A closed handle is no longer valid.
    let (h, _, _) = p.open(VPath::at_default("a.txt"), crate::OPEN_READ).expect("open");
    p.close(h).expect("close");
    assert!(
        p.read_at(h, 0, &mut [0u8; 4]).is_err(),
        "read_at on a closed handle must fail"
    );
}

fn assert_sequential(p: &Arc<dyn Provider>) {
    for (rel, body) in FIXTURE_FILES {
        // A sequential provider must refuse positional reads rather than
        // silently returning something plausible.
        let (probe, _, _) = p.open(VPath::at_default(rel), crate::OPEN_READ).expect("open");
        match p.read_at(probe, 0, &mut [0u8; 4]) {
            Err(e) if e == crate::not_supported() => {}
            Err(e) => panic!("read_at on a SeqRead provider returned status {e}, expected ST_NOT_SUPPORTED"),
            Ok(n) => panic!("read_at on a SeqRead provider succeeded with {n} bytes; it must be refused"),
        }
        p.close(probe).expect("close");

        let (h, _, _) = p.open(VPath::at_default(rel), crate::OPEN_READ).expect("open");
        let mut out = Vec::new();
        let mut buf = [0u8; 3];
        loop {
            let n = p.read_next(h, &mut buf).expect("read_next");
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            // Bound the loop: a provider whose cursor does not advance would
            // otherwise hang here instead of failing.
            assert!(
                out.len() <= body.len(),
                "read_next returned more than {rel}'s {} bytes — the cursor is not advancing",
                body.len()
            );
        }
        assert_eq!(out, *body, "read_next({rel}) content mismatch");
        p.close(h).expect("close");

        // Reopening resets the cursor.
        let (h2, _, _) = p.open(VPath::at_default(rel), crate::OPEN_READ).expect("reopen");
        let mut first = [0u8; 1];
        let n = p.read_next(h2, &mut first).expect("read_next after reopen");
        assert_eq!(n, 1, "reopen did not reset the cursor — read_next returned {n} bytes");
        assert_eq!(&first[..1], &body[..1], "reopen returned the wrong first byte");
        p.close(h2).expect("close");
    }
}

/// Write cases. Run last, because they mutate. Every path is `w_`-prefixed so
/// the reference tree the read cases assert is never disturbed.
fn assert_writable(p: &Arc<dyn Provider>) {
    use crate::{OPEN_APPEND, OPEN_CREATE, OPEN_EXCL, OPEN_TRUNC, OPEN_WRITE};

    let f = VPath::at_default("w_new.txt");

    // Create, write, read back through a fresh handle.
    let (h, _, _) = p.open(f, OPEN_WRITE | OPEN_CREATE).expect("open create");
    assert_eq!(p.write_at(h, 0, b"hello").expect("write_at"), 5);
    p.flush(h).expect("flush");
    p.close(h).expect("close");

    let st = p.getattr(f).expect("getattr after write").expect("file must exist after write");
    assert_eq!(st.size, 5, "size after write");

    let (h, size, _) = p.open(f, crate::OPEN_READ).expect("reopen for read");
    assert_eq!(size, 5);
    let mut buf = [0u8; 8];
    let n = p.read_at(h, 0, &mut buf).expect("read_at");
    assert_eq!(&buf[..n], b"hello", "written bytes did not read back");
    p.close(h).expect("close");

    // EXCL refuses an existing path.
    assert!(
        p.open(f, OPEN_WRITE | OPEN_CREATE | OPEN_EXCL).is_err(),
        "OPEN_EXCL must fail on an existing file"
    );

    // TRUNC empties it.
    let (h, _, _) = p.open(f, OPEN_WRITE | OPEN_TRUNC).expect("open trunc");
    p.close(h).expect("close");
    assert_eq!(p.getattr(f).expect("getattr").expect("exists").size, 0, "TRUNC must empty the file");

    // Positional overwrite mid-file.
    let (h, _, _) = p.open(f, OPEN_WRITE).expect("open write");
    p.write_at(h, 0, b"abcdef").expect("write_at");
    p.write_at(h, 2, b"XY").expect("overwrite");
    p.close(h).expect("close");
    let (h, _, _) = p.open(f, crate::OPEN_READ).expect("reopen");
    let n = p.read_at(h, 0, &mut buf).expect("read_at");
    assert_eq!(&buf[..n], b"abXYef", "positional overwrite wrong");
    p.close(h).expect("close");

    // set_len shrinks and grows; growth zero-fills.
    let (h, _, _) = p.open(f, OPEN_WRITE).expect("open");
    p.set_len(h, 2).expect("shrink");
    p.set_len(h, 4).expect("grow");
    p.close(h).expect("close");
    let (h, size, _) = p.open(f, crate::OPEN_READ).expect("reopen");
    assert_eq!(size, 4, "set_len size wrong");
    let n = p.read_at(h, 0, &mut buf).expect("read_at");
    assert_eq!(&buf[..n], b"ab\0\0", "set_len growth must zero-fill");
    p.close(h).expect("close");

    // Append lands at end of file.
    let (h, _, _) = p.open(f, OPEN_WRITE | OPEN_TRUNC).expect("open trunc");
    p.write_at(h, 0, b"one").expect("write");
    p.close(h).expect("close");
    let (h, _, _) = p.open(f, OPEN_WRITE | OPEN_APPEND).expect("open append");
    p.write_at(h, 3, b"two").expect("append");
    p.close(h).expect("close");
    let (h, _, _) = p.open(f, crate::OPEN_READ).expect("reopen");
    let n = p.read_at(h, 0, &mut buf).expect("read_at");
    assert_eq!(&buf[..n], b"onetwo", "append did not land at end");
    p.close(h).expect("close");

    // mkdir is visible to getattr and readdir.
    let d = VPath::at_default("w_dir");
    p.mkdir(d).expect("mkdir");
    let st = p.getattr(d).expect("getattr dir").expect("dir must exist");
    assert_eq!(st.kind, crate::KIND_DIR, "mkdir did not produce a directory");
    assert!(
        p.readdir(VPath::at_default(""))
            .expect("readdir root")
            .iter()
            .any(|e| e.name == "w_dir"),
        "mkdir not visible in readdir"
    );

    // rename moves content and clears the old name.
    let g = VPath::at_default("w_moved.txt");
    p.rename(f, g).expect("rename");
    assert!(p.getattr(f).expect("getattr old").is_none(), "rename left the old name behind");
    let st = p.getattr(g).expect("getattr new").expect("renamed file must exist");
    assert_eq!(st.size, 6, "rename lost content");

    // Cross-root rename is refused.
    assert_eq!(
        p.rename(g, VPath::new(RootId(9), "w_moved.txt")),
        Err(crate::bad_request()),
        "cross-root rename must be refused"
    );

    // remove clears a file and an empty directory.
    p.remove(g).expect("remove file");
    assert!(p.getattr(g).expect("getattr removed").is_none(), "remove did not delete the file");
    p.remove(d).expect("remove dir");
    assert!(p.getattr(d).expect("getattr removed dir").is_none(), "remove did not delete the dir");

    // set_attr accepts an mtime without error.
    let keep = VPath::at_default("w_attr.txt");
    let (h, _, _) = p.open(keep, OPEN_WRITE | OPEN_CREATE).expect("open create");
    p.close(h).expect("close");
    p.set_attr(keep, crate::SetAttr { mtime: Some(1_700_000_000), size: None })
        .expect("set_attr mtime");
    p.remove(keep).expect("cleanup");

    // The reference tree survived: write cases must not disturb it. Compare
    // bytes, not just size — a same-length scribble is the corruption this
    // check exists to catch, and a size comparison cannot see it.
    for (rel, body) in FIXTURE_FILES {
        let vp = VPath::at_default(rel);
        let st = p
            .getattr(vp)
            .unwrap_or_else(|e| panic!("getattr({rel}) after writes failed with {e}"))
            .unwrap_or_else(|| panic!("write cases destroyed {rel}"));
        assert_eq!(st.size, body.len() as u64, "write cases altered {rel}'s size");

        let (h, _, _) = p
            .open(vp, crate::OPEN_READ)
            .unwrap_or_else(|e| panic!("reopen({rel}) after writes failed with {e}"));
        let got = read_all(p, h, st.size);
        p.close(h).expect("close");
        assert_eq!(got, *body, "write cases altered {rel}'s content");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reference_fixture_passes_its_own_suite() {
        assert_conformance(std::sync::Arc::new(MemFixture::new()));
    }

    #[test]
    fn the_sequential_fixture_passes_its_own_suite() {
        assert_conformance(std::sync::Arc::new(SeqFixture::new()));
    }

    #[test]
    #[should_panic(expected = "getattr")]
    fn a_provider_that_loses_a_file_fails_the_suite() {
        assert_conformance(std::sync::Arc::new(MemFixture::missing("a.txt")));
    }

    /// Serves different content per root, proving `VPath` carries the root id
    /// through. This is not an obligation on real providers — a zip serves one
    /// archive under every root — it verifies the plumbing, not the contract.
    struct PerRootFixture;

    impl Provider for PerRootFixture {
        fn capabilities(&self) -> Capabilities {
            Capabilities::read_only()
        }
        fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
            Ok(Some(Stat { kind: KIND_FILE, size: u64::from(p.root.0), mtime: 0 }))
        }
        fn readdir(&self, _p: VPath) -> Result<Vec<DirEntry>, i32> {
            Ok(Vec::new())
        }
        fn open(&self, _p: VPath, _f: u32) -> Result<(Handle, u64, bool), i32> {
            Err(not_found())
        }
        fn close(&self, _h: Handle) -> Result<(), i32> {
            Ok(())
        }
    }

    #[test]
    fn vpath_carries_the_root_id_to_the_provider() {
        let p = PerRootFixture;
        let at0 = p.getattr(VPath::new(RootId(0), "same")).unwrap().unwrap();
        let at3 = p.getattr(VPath::new(RootId(3), "same")).unwrap().unwrap();
        assert_eq!(at0.size, 0);
        assert_eq!(at3.size, 3, "the provider did not receive the root id");
    }

    #[test]
    fn the_writable_fixture_passes_its_own_suite() {
        assert_conformance(std::sync::Arc::new(RwMemFixture::new()));
    }

    #[test]
    #[should_panic(expected = "read back")]
    fn a_provider_whose_writes_vanish_fails_the_suite() {
        assert_conformance(std::sync::Arc::new(RwMemFixture::discarding_writes()));
    }
}

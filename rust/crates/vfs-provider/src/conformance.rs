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
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir.join("sub")).expect("create fixture tree");
    for (rel, body) in FIXTURE_FILES {
        let p = dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        std::fs::write(p, body).expect("write fixture file");
    }
}

/// In-memory reference provider, used to test the suite itself. Serves the
/// fixture tree under every root unless built with [`MemFixture::root_blind`].
pub struct MemFixture {
    files: HashMap<String, Vec<u8>>,
    /// When false, only `RootId(0)` resolves — the correct behavior here is
    /// "same tree under every root", so this models a root-blind bug.
    root_aware: bool,
    next: AtomicU64,
    opens: Mutex<HashMap<Handle, Vec<u8>>>,
}

impl MemFixture {
    pub fn new() -> Self {
        Self::build(None, true)
    }

    /// A fixture missing one path, to prove the suite detects a gap.
    pub fn missing(path: &str) -> Self {
        Self::build(Some(path.to_string()), true)
    }

    /// A fixture that serves content only under `RootId(0)`, to prove the
    /// suite detects a provider that ignores the root id.
    pub fn root_blind() -> Self {
        Self::build(None, false)
    }

    fn build(omit: Option<String>, root_aware: bool) -> Self {
        let mut files = HashMap::new();
        for (rel, body) in FIXTURE_FILES {
            if omit.as_deref() == Some(*rel) {
                continue;
            }
            files.insert((*rel).to_string(), body.to_vec());
        }
        MemFixture { files, root_aware, next: AtomicU64::new(1), opens: Mutex::new(HashMap::new()) }
    }

    fn visible(&self, p: VPath) -> bool {
        self.root_aware || p.root == RootId::DEFAULT
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
        if !self.visible(p) {
            return Ok(None);
        }
        if p.rel.is_empty() || p.rel == "sub" {
            return Ok(Some(Stat { kind: KIND_DIR, size: 0, mtime: 0 }));
        }
        Ok(self
            .files
            .get(p.rel)
            .map(|b| Stat { kind: KIND_FILE, size: b.len() as u64, mtime: 0 }))
    }

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        if !self.visible(p) {
            return Err(not_found());
        }
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
        if !self.visible(p) {
            return Err(not_found());
        }
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

    // readdir of the root lists both entries.
    let entries = p.readdir(VPath::at_default("")).expect("readdir: provider root");
    let mut names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["a.txt", "sub"], "readdir of the root listed {names:?}");

    // readdir of a subdirectory.
    let sub = p.readdir(VPath::at_default("sub")).expect("readdir: sub");
    assert_eq!(sub.len(), 1, "readdir(sub) should list exactly one entry");
    assert_eq!(sub[0].name, "b.txt");

    // Root scoping: the same relative path must resolve under a non-default root.
    let alt = p
        .getattr(VPath::new(RootId(7), "a.txt"))
        .expect("root scoping: getattr under RootId(7) failed");
    assert!(
        alt.is_some(),
        "root scoping: a.txt resolved under root 0 but not under root 7 — \
         the provider is ignoring the root id"
    );

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
        let (h, _, _) = p.open(VPath::at_default(rel), crate::OPEN_READ).expect("open");
        let mut out = Vec::new();
        let mut buf = [0u8; 3];
        loop {
            let n = p.read_next(h, &mut buf).expect("read_next");
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, *body, "read_next({rel}) content mismatch");
        p.close(h).expect("close");

        // Reopening resets the cursor.
        let (h2, _, _) = p.open(VPath::at_default(rel), crate::OPEN_READ).expect("reopen");
        let mut first = [0u8; 1];
        let n = p.read_next(h2, &mut first).expect("read_next after reopen");
        assert_eq!(&first[..n], &body[..n], "reopen did not reset the cursor");
        p.close(h2).expect("close");
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
    #[should_panic(expected = "getattr")]
    fn a_provider_that_loses_a_file_fails_the_suite() {
        assert_conformance(std::sync::Arc::new(MemFixture::missing("a.txt")));
    }

    #[test]
    #[should_panic(expected = "root scoping")]
    fn a_provider_that_ignores_the_root_id_fails_the_suite() {
        assert_conformance(std::sync::Arc::new(MemFixture::root_blind()));
    }
}

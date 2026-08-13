//! Upper-over-base with `.wh.*` whiteouts and whole-file copy-up.
//!
//! Base is read through the `Provider` interface and never mutated. Upper is
//! itself a `Provider` and must declare `Access::ReadWrite` — validated once
//! at construction, not at first write, so a misconfigured stack fails fast
//! rather than surprising the first writer. Writing to a base-only path
//! copies the whole file into upper before the write lands: the domain here
//! is INIs, saves, and logs, not the multi-gigabyte read-only assets, so
//! whole-file copy-up beats a lazy, block-tracked one on both simplicity and
//! correctness. Removing a base-visible path — file or directory — writes a
//! `.wh.<name>` marker into upper instead of touching base.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use vfs_provider::{
    bad_fh, bad_request, map_io_err, not_found, not_supported, Access, Capabilities, DirEntry,
    Handle, Provider, SetAttr, Stat, VPath, KIND_DIR, KIND_FILE, OPEN_CREATE, OPEN_READ,
    OPEN_TRUNC, OPEN_WRITE,
};

#[derive(Clone, Copy)]
enum Layer {
    Upper,
    Base,
}

/// Upper-over-base with `.wh.*` whiteouts and copy-up (see module docs).
pub struct OverlayProvider {
    base: Arc<dyn Provider>,
    upper: Arc<dyn Provider>,
    next: AtomicU64,
    opens: Mutex<HashMap<u64, (Layer, Handle)>>,
    /// Paths currently being copied up, so two concurrent writers to the same
    /// base-only path copy exactly once instead of racing.
    copying: Mutex<HashSet<String>>,
}

/// Removes `path` from the in-flight set on drop, including on early return —
/// so a failed copy still releases the slot for the next attempt.
struct CopyGuard<'a> {
    copying: &'a Mutex<HashSet<String>>,
    path: &'a str,
}

impl Drop for CopyGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut g) = self.copying.lock() {
            g.remove(self.path);
        }
    }
}

impl OverlayProvider {
    /// `upper` may be a bare `Provider` value or one already behind an `Arc`
    /// — either way it is normalized to `Arc<dyn Provider>`. Fails if `upper`
    /// does not declare `Access::ReadWrite`: that must be caught here, not at
    /// first write.
    pub fn new<U>(base: Arc<dyn Provider>, upper: U) -> Result<Self, &'static str>
    where
        U: Provider + 'static,
    {
        let upper: Arc<dyn Provider> = Arc::new(upper);
        if upper.capabilities().access != Access::ReadWrite {
            return Err("OverlayProvider: upper must declare Access::ReadWrite");
        }
        Ok(Self {
            base,
            upper,
            next: AtomicU64::new(1),
            opens: Mutex::new(HashMap::new()),
            copying: Mutex::new(HashSet::new()),
        })
    }

    fn track(&self, layer: Layer, inner: Handle) -> Result<Handle, i32> {
        let h = self.next.fetch_add(1, Ordering::Relaxed);
        self.opens
            .lock()
            .map_err(|_| map_io_err())?
            .insert(h, (layer, inner));
        Ok(h)
    }

    fn lookup(&self, h: Handle) -> Result<(Layer, Handle), i32> {
        self.opens
            .lock()
            .map_err(|_| map_io_err())?
            .get(&h)
            .copied()
            .ok_or_else(bad_fh)
    }

    /// `.wh.<name>` sibling of `path`, in the same directory.
    fn whiteout_path(&self, path: &str) -> String {
        match path.rsplit_once('/') {
            Some((parent, name)) => format!("{parent}/.wh.{name}"),
            None => format!(".wh.{path}"),
        }
    }

    fn is_whiteout(&self, p: VPath) -> Result<bool, i32> {
        let wh = self.whiteout_path(p.rel);
        Ok(self.upper.getattr(VPath::new(p.root, &wh))?.is_some())
    }

    /// True if `p` itself, or any ancestor directory, has been whited out —
    /// a whiteout on a base directory hides its whole subtree.
    fn hidden_by_whiteout(&self, p: VPath) -> Result<bool, i32> {
        if self.is_whiteout(p)? {
            return Ok(true);
        }
        let mut cur = p.rel;
        while let Some((parent, _)) = cur.rsplit_once('/') {
            if self.is_whiteout(VPath::new(p.root, parent))? {
                return Ok(true);
            }
            cur = parent;
        }
        Ok(false)
    }

    fn clear_whiteout(&self, p: VPath) -> Result<(), i32> {
        let wh = self.whiteout_path(p.rel);
        match self.upper.remove(VPath::new(p.root, &wh)) {
            Ok(()) => Ok(()),
            Err(e) if e == not_found() => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn write_whiteout(&self, p: VPath) -> Result<(), i32> {
        let wh = self.whiteout_path(p.rel);
        let (h, _, _) = self.upper.open(
            VPath::new(p.root, &wh),
            OPEN_WRITE | OPEN_CREATE | OPEN_TRUNC,
        )?;
        self.upper.close(h)
    }

    /// Copy the whole base file at `p` into upper if it is not already there.
    /// A no-op if `p` is absent from base too, or is a directory (directories
    /// are represented implicitly, never copied). Guarded by `copying` so two
    /// concurrent callers for the same path copy exactly once: whoever loses
    /// the race waits for the winner's slot to clear, then re-checks upper
    /// before ever touching base.
    fn copy_up_if_needed(&self, p: VPath) -> Result<(), i32> {
        if self.upper.getattr(p)?.is_some() {
            return Ok(());
        }
        let Some(stat) = self.base.getattr(p)? else {
            return Ok(());
        };
        if stat.kind != KIND_FILE {
            return Ok(());
        }

        let path = p.rel.to_string();
        loop {
            let mut inflight = self.copying.lock().map_err(|_| map_io_err())?;
            if inflight.insert(path.clone()) {
                break;
            }
            drop(inflight);
            std::thread::yield_now();
        }
        let _guard = CopyGuard {
            copying: &self.copying,
            path: &path,
        };

        // Re-check: another thread may have finished the copy between our
        // first getattr above and winning the slot just now.
        if self.upper.getattr(p)?.is_some() {
            return Ok(());
        }
        self.copy_file_up(p)
    }

    fn copy_file_up(&self, p: VPath) -> Result<(), i32> {
        let (bh, size, _) = self.base.open(p, OPEN_READ)?;
        let copied = self.copy_bytes(bh, size, p);
        let _ = self.base.close(bh);
        copied
    }

    fn copy_bytes(&self, bh: Handle, size: u64, dest: VPath) -> Result<(), i32> {
        let (uh, _, _) = self
            .upper
            .open(dest, OPEN_WRITE | OPEN_CREATE | OPEN_TRUNC)?;
        let result = self.copy_loop(bh, uh, size);
        let _ = self.upper.close(uh);
        result
    }

    fn copy_loop(&self, bh: Handle, uh: Handle, size: u64) -> Result<(), i32> {
        let mut buf = [0u8; 65536];
        let mut off = 0u64;
        while off < size {
            let n = self.base.read_at(bh, off, &mut buf)?;
            if n == 0 {
                break;
            }
            self.upper.write_at(uh, off, &buf[..n])?;
            off += n as u64;
        }
        Ok(())
    }

    fn open_for_write(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        if self.upper.getattr(p)?.is_none() {
            if self.hidden_by_whiteout(p)? {
                if flags & OPEN_CREATE == 0 {
                    return Err(not_found());
                }
                // OPEN_CREATE explicitly asks to (re)create over a whiteout;
                // clear it so the new file is genuinely visible afterward.
                self.clear_whiteout(p)?;
            } else {
                self.copy_up_if_needed(p)?;
            }
        }
        let (uh, size, is_dir) = self.upper.open(p, flags)?;
        let h = self.track(Layer::Upper, uh)?;
        Ok((h, size, is_dir))
    }
}

impl Provider for OverlayProvider {
    fn capabilities(&self) -> Capabilities {
        // A writable upper makes the stack writable regardless of the base,
        // and a stack you can write to is by definition not immutable —
        // declaring otherwise would be a promise a caching layer would act
        // on. `slow` and `preferred_block` still combine across both
        // children.
        Capabilities {
            access: Access::ReadWrite,
            immutable: false,
            ..Capabilities::weakest([self.base.capabilities(), self.upper.capabilities()])
        }
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        if p.rel.is_empty() {
            return Ok(Some(Stat {
                kind: KIND_DIR,
                size: 0,
                mtime: 0,
            }));
        }
        if self.hidden_by_whiteout(p)? {
            return Ok(None);
        }
        if let Some(st) = self.upper.getattr(p)? {
            return Ok(Some(st));
        }
        self.base.getattr(p)
    }

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        let path = p.rel;
        let mut map: HashMap<String, DirEntry> = HashMap::new();
        let mut upper_is_dir = false;

        if !self.hidden_by_whiteout(p)? {
            match self.base.readdir(p) {
                Ok(entries) => {
                    for e in entries {
                        map.insert(e.name.to_ascii_lowercase(), e);
                    }
                }
                Err(e) if e == not_found() => {}
                Err(e) => return Err(e),
            }
        }

        match self.upper.readdir(p) {
            Ok(entries) => {
                upper_is_dir = true;
                for e in entries {
                    if let Some(target) = e.name.strip_prefix(".wh.") {
                        map.remove(&target.to_ascii_lowercase());
                        continue;
                    }
                    map.insert(e.name.to_ascii_lowercase(), e);
                }
            }
            Err(e) if e == not_found() => {}
            Err(e) => return Err(e),
        }

        if !upper_is_dir && !path.is_empty() && self.getattr(p)?.is_none() {
            return Err(not_found());
        }

        let mut out: Vec<DirEntry> = map.into_values().collect();
        out.sort_by_key(|a| a.name.to_ascii_lowercase());
        Ok(out)
    }

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        if flags & OPEN_WRITE != 0 {
            return self.open_for_write(p, flags);
        }
        if self.hidden_by_whiteout(p)? {
            return Err(not_found());
        }
        match self.upper.open(p, flags) {
            Ok((uh, size, is_dir)) => {
                let h = self.track(Layer::Upper, uh)?;
                Ok((h, size, is_dir))
            }
            Err(e) if e == not_found() => {
                let (bh, size, is_dir) = self.base.open(p, flags)?;
                let h = self.track(Layer::Base, bh)?;
                Ok((h, size, is_dir))
            }
            Err(e) => Err(e),
        }
    }

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let (layer, inner) = self.lookup(h)?;
        match layer {
            Layer::Upper => self.upper.read_at(inner, offset, buf),
            Layer::Base => self.base.read_at(inner, offset, buf),
        }
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        let (layer, inner) = self
            .opens
            .lock()
            .map_err(|_| map_io_err())?
            .remove(&h)
            .ok_or_else(bad_fh)?;
        match layer {
            Layer::Upper => self.upper.close(inner),
            Layer::Base => self.base.close(inner),
        }
    }

    fn write_at(&self, h: Handle, offset: u64, buf: &[u8]) -> Result<usize, i32> {
        match self.lookup(h)? {
            (Layer::Upper, inner) => self.upper.write_at(inner, offset, buf),
            (Layer::Base, _) => Err(not_supported()),
        }
    }

    fn set_len(&self, h: Handle, len: u64) -> Result<(), i32> {
        match self.lookup(h)? {
            (Layer::Upper, inner) => self.upper.set_len(inner, len),
            (Layer::Base, _) => Err(not_supported()),
        }
    }

    fn flush(&self, h: Handle) -> Result<(), i32> {
        match self.lookup(h)? {
            (Layer::Upper, inner) => self.upper.flush(inner),
            (Layer::Base, _) => Err(not_supported()),
        }
    }

    fn mkdir(&self, p: VPath) -> Result<(), i32> {
        self.clear_whiteout(p)?;
        self.upper.mkdir(p)
    }

    fn remove(&self, p: VPath) -> Result<(), i32> {
        if self.hidden_by_whiteout(p)? {
            return Err(not_found());
        }
        let in_upper = self.upper.getattr(p)?.is_some();
        let in_base = self.base.getattr(p)?.is_some();
        if !in_upper && !in_base {
            return Err(not_found());
        }
        // A path copied up earlier and then removed must not let the base
        // version resurface: delete the upper copy (if any) *and* whiteout
        // the base version (if any) rather than treating the two as
        // mutually exclusive.
        if in_upper {
            self.upper.remove(p)?;
        }
        if in_base {
            self.write_whiteout(p)?;
        }
        Ok(())
    }

    fn rename(&self, from: VPath, to: VPath) -> Result<(), i32> {
        if from.root != to.root {
            return Err(bad_request());
        }
        if self.hidden_by_whiteout(from)? {
            return Err(not_found());
        }
        self.copy_up_if_needed(from)?;
        let from_in_base = self.base.getattr(from)?.is_some();
        self.upper.rename(from, to)?;
        // The destination may have been whited out by an earlier remove;
        // the rename just gave it real content again.
        self.clear_whiteout(to)?;
        if from_in_base {
            self.write_whiteout(from)?;
        }
        Ok(())
    }

    fn set_attr(&self, p: VPath, attr: SetAttr) -> Result<(), i32> {
        if self.hidden_by_whiteout(p)? {
            return Err(not_found());
        }
        self.copy_up_if_needed(p)?;
        self.upper.set_attr(p, attr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InlineProvider;
    use vfs_provider::{OPEN_EXCL, OPEN_READ};

    /// Slow and immutable, but sequential-only — exercises both the
    /// pass-through fields and the forced access/immutable overrides at once.
    struct SlowSeqBase;

    impl Provider for SlowSeqBase {
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                access: vfs_provider::Access::SeqRead,
                immutable: true,
                slow: true,
                preferred_block: Some(4096),
            }
        }
        fn getattr(&self, _p: VPath) -> Result<Option<Stat>, i32> {
            Ok(None)
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
        fn read_at(&self, _h: Handle, _o: u64, _b: &mut [u8]) -> Result<usize, i32> {
            Ok(0)
        }
    }

    /// A genuinely empty in-memory `ReadWrite` provider, for use as an
    /// overlay upper. Deliberately NOT `vfs_provider::RwMemFixture`: that one
    /// is a conformance fixture and is permanently obligated to serve
    /// `FIXTURE_FILES` so it can pass the suite on its own — which means an
    /// overlay built on it would pass its tests even while ignoring its base
    /// entirely. An overlay's upper must start empty.
    #[derive(Default)]
    struct MemUpper {
        files: Mutex<HashMap<String, Vec<u8>>>,
        dirs: Mutex<HashSet<String>>,
        next: AtomicU64,
        opens: Mutex<HashMap<Handle, String>>,
    }

    impl Provider for MemUpper {
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                access: Access::ReadWrite,
                immutable: false,
                slow: false,
                preferred_block: None,
            }
        }

        fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
            if p.rel.is_empty() {
                return Ok(Some(Stat {
                    kind: KIND_DIR,
                    size: 0,
                    mtime: 0,
                }));
            }
            if let Some(body) = self.files.lock().unwrap().get(p.rel) {
                return Ok(Some(Stat {
                    kind: KIND_FILE,
                    size: body.len() as u64,
                    mtime: 0,
                }));
            }
            if self.dirs.lock().unwrap().contains(p.rel) {
                return Ok(Some(Stat {
                    kind: KIND_DIR,
                    size: 0,
                    mtime: 0,
                }));
            }
            Ok(None)
        }

        fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
            let prefix = if p.rel.is_empty() {
                String::new()
            } else {
                format!("{}/", p.rel)
            };
            let mut seen: HashMap<String, DirEntry> = HashMap::new();
            for (rel, body) in self.files.lock().unwrap().iter() {
                let Some(rest) = rel.strip_prefix(prefix.as_str()) else {
                    continue;
                };
                if rest.is_empty() || rest.contains('/') {
                    continue;
                }
                seen.insert(
                    rest.to_string(),
                    DirEntry {
                        name: rest.to_string(),
                        stat: Stat {
                            kind: KIND_FILE,
                            size: body.len() as u64,
                            mtime: 0,
                        },
                    },
                );
            }
            for d in self.dirs.lock().unwrap().iter() {
                let Some(rest) = d.strip_prefix(prefix.as_str()) else {
                    continue;
                };
                if rest.is_empty() || rest.contains('/') {
                    continue;
                }
                seen.entry(rest.to_string()).or_insert(DirEntry {
                    name: rest.to_string(),
                    stat: Stat {
                        kind: KIND_DIR,
                        size: 0,
                        mtime: 0,
                    },
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
            let mut files = self.files.lock().unwrap();
            let exists = files.contains_key(p.rel);
            if flags & OPEN_WRITE == 0 {
                if !exists {
                    return Err(not_found());
                }
            } else {
                if flags & OPEN_EXCL != 0 && exists {
                    return Err(bad_request());
                }
                if flags & OPEN_CREATE != 0 {
                    files.entry(p.rel.to_string()).or_default();
                } else if !exists {
                    return Err(not_found());
                }
                if flags & OPEN_TRUNC != 0 {
                    files.insert(p.rel.to_string(), Vec::new());
                }
            }
            let size = files.get(p.rel).map(|b| b.len()).unwrap_or(0) as u64;
            drop(files);
            let h = self.next.fetch_add(1, Ordering::Relaxed);
            self.opens.lock().unwrap().insert(h, p.rel.to_string());
            Ok((h, size, false))
        }

        fn close(&self, h: Handle) -> Result<(), i32> {
            self.opens.lock().unwrap().remove(&h).ok_or_else(bad_fh)?;
            Ok(())
        }

        fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
            let path = self
                .opens
                .lock()
                .unwrap()
                .get(&h)
                .cloned()
                .ok_or_else(bad_fh)?;
            let files = self.files.lock().unwrap();
            let body = files.get(&path).ok_or_else(bad_fh)?;
            let start = (offset as usize).min(body.len());
            let n = (body.len() - start).min(buf.len());
            buf[..n].copy_from_slice(&body[start..start + n]);
            Ok(n)
        }

        fn write_at(&self, h: Handle, offset: u64, buf: &[u8]) -> Result<usize, i32> {
            let path = self
                .opens
                .lock()
                .unwrap()
                .get(&h)
                .cloned()
                .ok_or_else(bad_fh)?;
            let mut files = self.files.lock().unwrap();
            let body = files.entry(path).or_default();
            let end = offset as usize + buf.len();
            if body.len() < end {
                body.resize(end, 0);
            }
            body[offset as usize..end].copy_from_slice(buf);
            Ok(buf.len())
        }

        fn set_len(&self, h: Handle, len: u64) -> Result<(), i32> {
            let path = self
                .opens
                .lock()
                .unwrap()
                .get(&h)
                .cloned()
                .ok_or_else(bad_fh)?;
            self.files
                .lock()
                .unwrap()
                .entry(path)
                .or_default()
                .resize(len as usize, 0);
            Ok(())
        }

        fn flush(&self, _h: Handle) -> Result<(), i32> {
            Ok(())
        }

        fn mkdir(&self, p: VPath) -> Result<(), i32> {
            self.dirs.lock().unwrap().insert(p.rel.to_string());
            Ok(())
        }

        fn remove(&self, p: VPath) -> Result<(), i32> {
            let had_file = self.files.lock().unwrap().remove(p.rel).is_some();
            let had_dir = self.dirs.lock().unwrap().remove(p.rel);
            if had_file || had_dir {
                Ok(())
            } else {
                Err(not_found())
            }
        }

        fn rename(&self, from: VPath, to: VPath) -> Result<(), i32> {
            if from.root != to.root {
                return Err(bad_request());
            }
            let mut files = self.files.lock().unwrap();
            let body = files.remove(from.rel).ok_or_else(not_found)?;
            files.insert(to.rel.to_string(), body);
            Ok(())
        }

        fn set_attr(&self, _p: VPath, _attr: SetAttr) -> Result<(), i32> {
            Ok(())
        }
    }

    #[test]
    fn overlay_reports_read_write_and_is_never_immutable() {
        let ov = OverlayProvider::new(Arc::new(SlowSeqBase), MemUpper::default()).unwrap();
        let caps = ov.capabilities();
        assert_eq!(
            caps.access,
            vfs_provider::Access::ReadWrite,
            "a writable upper makes the whole stack writable regardless of the base"
        );
        // A stack you can write to is by definition not immutable: claiming
        // otherwise would be a promise a caching layer would act on, and
        // Capabilities::validate() rejects ReadWrite + immutable as the
        // self-contradiction it is. Do not "fix" this back to true.
        assert!(!caps.immutable, "a writable stack can never be immutable");
        assert!(caps.slow, "slow still derives from the children");
        assert_eq!(caps.preferred_block, Some(4096));
    }

    #[test]
    fn overlay_over_the_fixture_tree_with_an_empty_upper_passes_conformance() {
        let base: Arc<dyn Provider> = Arc::new(InlineProvider::from_files(
            vfs_provider::FIXTURE_FILES.iter().copied(),
        ));
        let p: Arc<dyn Provider> =
            Arc::new(OverlayProvider::new(base, MemUpper::default()).unwrap());
        vfs_provider::assert_conformance(p);
    }

    #[test]
    fn upper_wins_and_whiteout_hides() {
        let base = Arc::new(InlineProvider::from_files([
            ("a.txt", b"BASE".as_slice()),
            ("gone.txt", b"X".as_slice()),
        ]));
        let upper = MemUpper::default();
        let (h, _, _) = upper
            .open(VPath::at_default("a.txt"), OPEN_WRITE | OPEN_CREATE)
            .unwrap();
        upper.write_at(h, 0, b"UPPER").unwrap();
        upper.close(h).unwrap();
        let (h, _, _) = upper
            .open(VPath::at_default(".wh.gone.txt"), OPEN_WRITE | OPEN_CREATE)
            .unwrap();
        upper.close(h).unwrap();
        let ov = OverlayProvider::new(base, upper).unwrap();

        let st = ov.getattr(VPath::at_default("a.txt")).unwrap().unwrap();
        assert_eq!(st.size, 5);
        assert!(ov.getattr(VPath::at_default("gone.txt")).unwrap().is_none());

        let (h, _, _) = ov.open(VPath::at_default("a.txt"), OPEN_READ).unwrap();
        let mut buf = [0u8; 8];
        let n = ov.read_at(h, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"UPPER");
        ov.close(h).unwrap();
    }

    #[test]
    fn overlay_declares_read_write_over_a_read_only_base() {
        use vfs_provider::{Access, Provider};
        let base = Arc::new(InlineProvider::from_files(
            vfs_provider::FIXTURE_FILES.iter().copied(),
        ));
        let ov = OverlayProvider::new(base, MemUpper::default()).unwrap();
        assert_eq!(ov.capabilities().access, Access::ReadWrite);
    }

    #[test]
    fn overlay_rejects_a_read_only_upper_at_construction() {
        let base = Arc::new(InlineProvider::from_files(
            vfs_provider::FIXTURE_FILES.iter().copied(),
        ));
        let upper = InlineProvider::from_files(std::iter::empty::<(&str, &[u8])>());
        assert!(
            OverlayProvider::new(base, upper).is_err(),
            "a read-only upper must be refused at construction, not at first write"
        );
    }

    #[test]
    fn writing_a_base_file_copies_it_up_and_leaves_base_untouched() {
        use vfs_provider::{Provider, VPath, OPEN_READ, OPEN_WRITE};
        let base = Arc::new(InlineProvider::from_files([("a.txt", b"BASE".as_slice())]));
        let ov = OverlayProvider::new(base.clone(), MemUpper::default()).unwrap();

        let f = VPath::at_default("a.txt");
        let (h, _, _) = ov.open(f, OPEN_WRITE).expect("open for write copies up");
        ov.write_at(h, 0, b"UP").expect("write");
        ov.close(h).expect("close");

        let (h, _, _) = ov.open(f, OPEN_READ).expect("reopen");
        let mut buf = [0u8; 8];
        let n = ov.read_at(h, 0, &mut buf).expect("read");
        assert_eq!(&buf[..n], b"UPSE", "copy-up must preserve the untouched tail");
        ov.close(h).expect("close");

        // The base is never mutated.
        let (bh, _, _) = base.open(f, OPEN_READ).expect("base open");
        let n = base.read_at(bh, 0, &mut buf).expect("base read");
        assert_eq!(&buf[..n], b"BASE", "copy-up mutated the base");
        base.close(bh).unwrap();
    }

    #[test]
    fn removing_a_base_file_writes_a_whiteout() {
        use vfs_provider::{Provider, VPath};
        let base = Arc::new(InlineProvider::from_files([("a.txt", b"BASE".as_slice())]));
        let ov = OverlayProvider::new(base, MemUpper::default()).unwrap();
        let f = VPath::at_default("a.txt");
        ov.remove(f).expect("remove");
        assert!(
            ov.getattr(f).expect("getattr").is_none(),
            "whiteout did not hide the base file"
        );
        assert!(
            !ov.readdir(VPath::at_default(""))
                .expect("readdir")
                .iter()
                .any(|e| e.name == "a.txt"),
            "whiteout did not hide the entry from readdir"
        );
    }

    #[test]
    fn concurrent_opens_copy_up_exactly_once() {
        use std::sync::Arc as StdArc;
        use vfs_provider::{Provider, VPath, OPEN_WRITE};
        let base = Arc::new(InlineProvider::from_files([("a.txt", b"BASE".as_slice())]));
        let ov: StdArc<OverlayProvider> =
            StdArc::new(OverlayProvider::new(base, MemUpper::default()).unwrap());

        let mut hs = Vec::new();
        for _ in 0..8 {
            let ov = StdArc::clone(&ov);
            hs.push(std::thread::spawn(move || {
                let (h, _, _) = ov.open(VPath::at_default("a.txt"), OPEN_WRITE).expect("open");
                ov.close(h).expect("close");
            }));
        }
        for h in hs {
            h.join().expect("thread");
        }
        // Content must still be the base content, not a truncated or doubled copy.
        let (h, size, _) = ov.open(VPath::at_default("a.txt"), vfs_provider::OPEN_READ).unwrap();
        assert_eq!(size, 4, "concurrent copy-up corrupted the file");
        ov.close(h).unwrap();
    }

    #[test]
    fn overlay_passes_write_conformance() {
        let base = Arc::new(InlineProvider::from_files(
            vfs_provider::FIXTURE_FILES.iter().copied(),
        ));
        let ov: Arc<dyn vfs_provider::Provider> =
            Arc::new(OverlayProvider::new(base, MemUpper::default()).unwrap());
        vfs_provider::assert_conformance(ov);
    }
}

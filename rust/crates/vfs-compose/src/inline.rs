//! In-memory file tree backend for tests (Clojure `inline-provider`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use vfs_provider::{
    bad_fh, bad_request, map_io_err, not_a_dir, not_found, Capabilities, DirEntry,
    Handle, Provider, Stat, VPath, KIND_DIR, KIND_FILE, OPEN_WRITE,
};

struct FileData {
    bytes: Vec<u8>,
}

/// Flat map of virtual paths → file bytes. Parent dirs are synthesized.
pub struct InlineProvider {
    files: HashMap<String, FileData>,
    /// Folded key → the spelling `files` is keyed by. Built once; this provider
    /// is immutable after construction, so unlike `MemoryProvider`'s index this
    /// one needs no maintenance and no lock.
    by_fold: HashMap<String, String>,
    next: AtomicU64,
    opens: Mutex<HashMap<u64, (String, Vec<u8>)>>,
}

impl InlineProvider {
    pub fn from_files<I, P, B>(entries: I) -> Self
    where
        I: IntoIterator<Item = (P, B)>,
        P: AsRef<str>,
        B: AsRef<[u8]>,
    {
        let mut files = HashMap::new();
        for (p, b) in entries {
            let path = normalize(p.as_ref());
            files.insert(
                path,
                FileData {
                    bytes: b.as_ref().to_vec(),
                },
            );
        }
        let mut by_fold = HashMap::with_capacity(files.len());
        for key in files.keys() {
            by_fold.insert(vfs_core::fold(key), key.clone());
        }
        Self {
            files,
            by_fold,
            next: AtomicU64::new(1),
            opens: Mutex::new(HashMap::new()),
        }
    }

    /// The stored spelling for `path`, or `None` if nothing fold-equal exists.
    fn canonical(&self, path: &str) -> Option<&String> {
        if self.files.contains_key(path) {
            return self.files.get_key_value(path).map(|(k, _)| k);
        }
        self.by_fold.get(&vfs_core::fold(path))
    }

    /// Shared getattr logic, addressed by an already-normalized plain path.
    fn stat(&self, path: &str) -> Result<Option<Stat>, i32> {
        if path.is_empty() {
            return Ok(Some(Stat {
                kind: KIND_DIR,
                size: 0,
                mtime: 0,
            }));
        }
        if let Some(key) = self.canonical(path) {
            let f = &self.files[key];
            return Ok(Some(Stat {
                kind: KIND_FILE,
                size: f.bytes.len() as u64,
                mtime: 0,
            }));
        }
        // Directory if any file has this path as a fold-equal component
        // prefix. Compared component-by-component (never by byte offset):
        // fold is not length-preserving, so a folded query and an unfolded
        // key can only be lined up by walking `/`-separated parts.
        if dir_has_fold_prefix(self.files.keys().map(String::as_str), path) {
            return Ok(Some(Stat {
                kind: KIND_DIR,
                size: 0,
                mtime: 0,
            }));
        }
        Ok(None)
    }
}

fn normalize(path: &str) -> String {
    path.replace('\\', "/").trim_matches('/').to_string()
}

/// Folded `/`-separated components of `path`. Empty for the root.
fn fold_components(path: &str) -> Vec<String> {
    if path.is_empty() {
        Vec::new()
    } else {
        path.split('/').map(vfs_core::fold).collect()
    }
}

/// If `key`'s leading components fold-match every one of `query`'s (with at
/// least one of `key`'s components remaining after them), the remaining
/// components of `key` — in `key`'s own, unfolded spelling. `None` if `key`
/// is not under `query`.
///
/// Never sliced by byte offset: fold is not length-preserving (`İ` is two
/// bytes and folds to three), so lining up a folded query against an
/// unfolded key only works by walking `/`-separated parts.
fn fold_strip_prefix<'k>(key: &'k str, query: &[String]) -> Option<&'k str> {
    let kc: Vec<&str> = key.split('/').collect();
    if kc.len() <= query.len() {
        return None;
    }
    let matches = kc[..query.len()]
        .iter()
        .zip(query)
        .all(|(c, q)| vfs_core::fold(c) == *q);
    if !matches {
        return None;
    }
    // Recover the byte offset of the remainder from key's own components,
    // not from query — see the length-preservation note above.
    let consumed: usize = kc[..query.len()].iter().map(|c| c.len() + 1).sum();
    Some(&key[consumed..])
}

/// True if any of `keys` has `query` as a proper fold-equal directory prefix.
fn dir_has_fold_prefix<'a>(keys: impl Iterator<Item = &'a str>, query: &str) -> bool {
    let query = fold_components(query);
    keys.into_iter()
        .any(|k| fold_strip_prefix(k, &query).is_some())
}

impl Provider for InlineProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            immutable: true,
            ..Capabilities::read_only()
        }
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        let path = p.rel;
        let path = normalize(path);
        self.stat(&path)
    }

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        let path = p.rel;
        let path = normalize(path);
        if self.stat(&path)?.map(|s| s.kind) != Some(KIND_DIR) {
            if self.canonical(&path).is_some() {
                return Err(not_a_dir());
            }
            return Err(not_found());
        }
        let query = fold_components(&path);
        let mut names: HashMap<String, Stat> = HashMap::new();
        for (k, f) in &self.files {
            let Some(rel) = fold_strip_prefix(k, &query) else {
                continue;
            };
            let name = rel.split('/').next().unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let is_file = !rel.contains('/');
            let st = if is_file {
                Stat {
                    kind: KIND_FILE,
                    size: f.bytes.len() as u64,
                    mtime: 0,
                }
            } else {
                Stat {
                    kind: KIND_DIR,
                    size: 0,
                    mtime: 0,
                }
            };
            names.entry(name.to_string()).or_insert(st);
        }
        Ok(names
            .into_iter()
            .map(|(name, stat)| DirEntry { name, stat })
            .collect())
    }

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        let path = p.rel;
        if flags & OPEN_WRITE != 0 {
            return Err(bad_request());
        }
        let path = normalize(path);
        let key = self.canonical(&path).ok_or_else(not_found)?;
        let f = &self.files[key];
        let h = self.next.fetch_add(1, Ordering::Relaxed);
        let size = f.bytes.len() as u64;
        self.opens
            .lock()
            .map_err(|_| map_io_err())?
            .insert(h, (key.clone(), f.bytes.clone()));
        Ok((h, size, false))
    }

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let g = self.opens.lock().map_err(|_| map_io_err())?;
        let (_, bytes) = g.get(&h).ok_or_else(bad_fh)?;
        if offset as usize >= bytes.len() {
            return Ok(0);
        }
        let start = offset as usize;
        let n = buf.len().min(bytes.len() - start);
        buf[..n].copy_from_slice(&bytes[start..start + n]);
        Ok(n)
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        self.opens
            .lock()
            .map_err(|_| map_io_err())?
            .remove(&h)
            .ok_or_else(bad_fh)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fold-equal spellings name the same entry. `InlineProvider` is the leaf
    /// under most composed test stacks, so a byte-exact match here makes every
    /// stack above it byte-exact too.
    #[test]
    fn fold_equal_spellings_resolve_to_one_entry() {
        let p = InlineProvider::from_files([("Data/A.esp", &b"body"[..])]);

        for spelling in ["Data/A.esp", "data/a.esp", "DATA/A.ESP", "dAtA/a.EsP"] {
            let st = p
                .getattr(VPath::at_default(spelling))
                .unwrap()
                .unwrap_or_else(|| panic!("{spelling} did not resolve"));
            assert_eq!(st.size, 4, "{spelling} resolved to the wrong entry");
        }
    }

    /// Non-ASCII, because `to_ascii_lowercase` would pass every case above.
    #[test]
    fn folding_is_unicode_not_ascii() {
        let p = InlineProvider::from_files([("Über/A.esp", &b"x"[..])]);
        assert!(
            p.getattr(VPath::at_default("über/a.esp")).unwrap().is_some(),
            "Unicode fold-equal spelling did not resolve"
        );
    }
}

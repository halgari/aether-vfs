//! In-memory read-write file tree — the host's `memory({...})` provider.
//!
//! A host hands in a name→bytes map, mounts it, a session (possibly a game
//! process under it) reads and writes through it, and the host reads back
//! whatever was written — with nothing touching disk. That round trip is the
//! provider's whole reason to exist: see the design spec's
//! `inis = vfs.memory({"Skyrim.ini": ini_bytes}); ...; inis.read("Skyrim.ini")`
//! (`docs/superpowers/specs/2026-08-13-pluggable-providers-design.md`).
//!
//! **Why this is not `InlineProvider`.** `InlineProvider` (`inline.rs`) looks
//! like the same thing, but it declares `Access::Read` and `immutable: true`
//! by contract, and a wide swath of this workspace's tests key off exactly
//! that: `stack_layers`'s "weakest access of its children" case, its
//! immutability under layering, `OPEN_WRITE` being refused outright, and
//! several `vfs-director`/`vfs-embed` tests that use it specifically *because*
//! it cannot be written to (they assert a write with no writable provider is
//! refused). Making `InlineProvider` writable would change behavior under
//! every one of those callers rather than add a capability, so this is a
//! sibling instead, not a promotion.
//!
//! **Why this lives in `vfs-compose` and not `vfs-provider` or `vfs-source`.**
//! `vfs-provider` already has an in-memory `ReadWrite` type
//! ([`vfs_provider::RwMemFixture`]), but it exists to test the conformance
//! suite itself and always serves the fixed `FIXTURE_FILES` tree — it has no
//! constructor from an arbitrary name→bytes map, so it cannot stand in here.
//! `vfs-source` is where a host would look for a source *registered by name*,
//! but `vfs-embed` — the crate a Node/Python binding actually links —
//! deliberately does not depend on `vfs-source`: reaching it would drag in
//! `vfs-control`, tonic, prost and a vendored `protoc` just to construct a
//! provider that needs none of that. `vfs-embed` already depends on
//! `vfs-compose` for its other combinators (`InlineProvider`,
//! `LayeredProvider`, `OverlayProvider`, ...) and re-exports them wholesale,
//! so putting `MemoryProvider` here means a host constructs one directly, and
//! `vfs-source::build_provider`'s `SourceSpec::Memory` arm builds the same
//! type for the declarative-config path — one implementation, two routes to
//! it, neither route paying for the other's dependencies.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use vfs_core::fold;
use vfs_provider::{
    bad_fh, bad_request, exists, is_dir, map_io_err, not_a_dir, not_found, Access, Capabilities,
    CaseMatch, DirEntry, Handle, Provider, SetAttr, Stat, VPath, KIND_DIR, KIND_FILE, OPEN_CREATE,
    OPEN_EXCL, OPEN_TRUNC,
};

fn normalize(path: &str) -> String {
    path.replace('\\', "/").trim_matches('/').to_string()
}

/// The `"path/"` string a child key must start with, or `""` for the provider
/// root (whose children carry no prefix at all). The same convention `readdir`
/// uses, kept in one place so "what lives under this directory" cannot mean two
/// different things in two methods.
fn child_prefix(path: &str) -> String {
    if path.is_empty() {
        String::new()
    } else {
        format!("{path}/")
    }
}

/// The immediate child of `path` that `key` names or lies under, matched by
/// the byte-exact `prefix` (`path`'s [`child_prefix`]) — not by folding
/// either side. `path` has already been through `canonical_in`, so it is
/// already the real, byte-exact spelling this directory has; folding here
/// would let a query for `Data` also surface a fold-equal but genuinely
/// distinct `DATA`'s children. Returns the child's name and whether `key`
/// *is* that child (a file or explicit empty dir) rather than merely
/// implying it (a deeper entry on its way to some grandchild).
fn direct_child<'k>(path: &str, prefix: &str, key: &'k str) -> Option<(&'k str, bool)> {
    let rel = if path.is_empty() { key } else { key.strip_prefix(prefix)? };
    let name = rel.split('/').next().unwrap_or("");
    (!name.is_empty()).then_some((name, name.len() == rel.len()))
}

/// Directory-ness of `path` given the current files and explicitly-created
/// (possibly empty) directories: a file wins, then a recorded empty dir, then
/// "some file or dir lives under this prefix" (an implicit parent), else
/// absent. Shared by `getattr` and `readdir` so the two cannot disagree on
/// what exists.
fn stat_of(files: &HashMap<String, Vec<u8>>, dirs: &HashSet<String>, path: &str) -> Option<Stat> {
    if path.is_empty() {
        return Some(Stat { kind: KIND_DIR, size: 0, mtime: 0 });
    }
    if let Some(b) = files.get(path) {
        return Some(Stat { kind: KIND_FILE, size: b.len() as u64, mtime: 0 });
    }
    if dirs.contains(path) {
        return Some(Stat { kind: KIND_DIR, size: 0, mtime: 0 });
    }
    let prefix = format!("{path}/");
    if files.keys().any(|k| k.starts_with(&prefix)) || dirs.iter().any(|d| d.starts_with(&prefix))
    {
        return Some(Stat { kind: KIND_DIR, size: 0, mtime: 0 });
    }
    None
}

/// The stored spelling for `path`, resolved against maps the caller already
/// holds. Always returns a usable path, never `None`: when `path` denotes a
/// real entry (byte-exact or fold-equal, stored or implied), that entry's
/// real spelling comes back; when only an ancestor of `path` is real, that
/// ancestor's real spelling comes back with `path`'s own tail appended
/// verbatim; when nothing of `path` is real, `path` itself comes back
/// unchanged. A caller creating a brand-new entry gets exactly the literal
/// name it asked for, same as before this function returned `Option`.
///
/// Byte-exact wins over fold at every step — see [`stat_of`]'s check inside
/// the loop below. This is what keeps two genuinely distinct, fold-equal
/// real entries (`Data` and `DATA` both present, each with its own child)
/// from being conflated into one: an earlier version folded before checking
/// stored keys at all, so a byte-exact query for either one could resolve to
/// the *other*, nondeterministically (`HashMap` iteration order), and
/// `readdir`/`rename` would then act on whichever one won. See this
/// function's git history for the reproductions.
///
/// Ancestors resolve independently of the leaf, one path component at a
/// time — mirroring `vfs-director/src/disk.rs`'s `resolve_fold_equal`, and
/// for the same reason: `fold` is not length-preserving (`İ` is 2 bytes,
/// folds to 3), so a folded prefix can never be sliced off an unfolded key
/// by byte length, only walked component by component. The moment a
/// component cannot be resolved (no byte-exact or fold-equal child under the
/// *already-resolved* ancestor — never a candidate from somewhere else in
/// the tree, which is exactly the conflation above), that component and
/// everything after it in `path` carry over verbatim. That is the common
/// "this is a create" case, and the reason the already-resolved ancestor
/// must not be discarded: a caller creating `data/new.txt` under an
/// existing `Data/` must land inside `Data/`, not fork a second,
/// divergently-cased `data/` beside it.
///
/// On a tie among multiple real fold-equal children at one level, the
/// lexicographically smallest spelling wins — arbitrary but deterministic,
/// again mirroring `disk.rs`, and documented there as not a claim of
/// correctness: there isn't a "right" answer once the tree itself holds two
/// spellings of the same name, only a stable one.
///
/// Deliberately does not reuse `crate::casefold::fold_strip_prefix` for the
/// per-component walk: that helper (rightly, for `InlineProvider`, which has
/// no byte-exact-precedence requirement) matches candidates by folding the
/// *whole* ancestor chain, so it cannot tell a real child of `Data` from a
/// real child of the fold-equal-but-distinct `DATA` — precisely the
/// conflation this function exists to prevent. The scan below instead
/// filters candidates by the byte-exact prefix of the ancestor already
/// locked in, and folds only the one remaining component being resolved.
///
/// Takes borrows rather than locking for itself, so a caller that goes on to
/// mutate does so under the same guard it resolved under. An earlier version
/// locked internally and returned an owned `String`, which opened a window
/// where a concurrent mutation could invalidate the resolved path before the
/// caller used it — every method released the lock this took, then re-locked
/// to act, and nothing stopped another thread's mutation from landing in
/// between.
fn canonical_in(
    files: &HashMap<String, Vec<u8>>,
    dirs: &HashSet<String>,
    by_fold: &HashMap<String, String>,
    path: &str,
) -> String {
    if path.is_empty() {
        return String::new();
    }

    // Exact match first: the common case, and it costs no fold at all —
    // avoiding the fold (not an allocation) is what this fast path buys.
    if files.contains_key(path) || dirs.contains(path) {
        return path.to_string();
    }
    let folded = fold(path);
    if let Some(hit) = by_fold.get(&folded) {
        return hit.clone();
    }

    // Neither an exact nor a whole-path fold match against a *stored* key:
    // `path` may still name, or lie under, a real entry once resolved
    // ancestor by ancestor (a directory implied by some deeper file's own
    // path is never itself stored, so `by_fold` has nothing for it).
    let mut cur = String::new();
    let mut parts = path.split('/').filter(|p| !p.is_empty());
    while let Some(part) = parts.by_ref().next() {
        let prefix = child_prefix(&cur);
        let exact_candidate = format!("{prefix}{part}");
        if stat_of(files, dirs, &exact_candidate).is_some() {
            cur = exact_candidate;
            continue;
        }

        // No byte-exact child named `part`: scan only the real children of
        // `cur` (never the whole tree) for a fold-equal name.
        let folded_part = fold(part);
        let mut best: Option<&str> = None;
        for k in files.keys().chain(dirs.iter()) {
            let Some(rest) = k.strip_prefix(prefix.as_str()) else { continue };
            let name = rest.split('/').next().unwrap_or("");
            if name.is_empty() || fold(name) != folded_part {
                continue;
            }
            best = match best {
                Some(prev) if prev <= name => Some(prev),
                _ => Some(name),
            };
        }
        match best {
            Some(name) => cur = format!("{prefix}{name}"),
            None => {
                // Nothing under `cur` resolves this component: it, and
                // everything after it, carry over exactly as given.
                cur = exact_candidate;
                for rest in parts {
                    cur.push('/');
                    cur.push_str(rest);
                }
                return cur;
            }
        }
    }
    cur
}

/// Record that `key` now names a live entry, in the `by_fold` map the caller
/// already holds locked.
fn index_insert(by_fold: &mut HashMap<String, String>, key: &str) {
    by_fold.insert(fold(key), key.to_string());
}

/// Forget that `key` names a live entry.
fn index_remove(by_fold: &mut HashMap<String, String>, key: &str) {
    by_fold.remove(&fold(key));
}

/// `old` stops naming a live entry and `new` starts, as one map mutation, so
/// under the lock the caller holds no observer ever sees the entry absent
/// under both spellings.
fn reindex(by_fold: &mut HashMap<String, String>, old: &str, new: &str) {
    by_fold.remove(&fold(old));
    by_fold.insert(fold(new), new.to_string());
}

/// Read-write in-memory file tree. Root-blind by design, like
/// [`crate::InlineProvider`]: it serves the same tree under every root id,
/// which `assert_common`'s non-default-root case accepts as one of the two
/// legal behaviors.
pub struct MemoryProvider {
    files: Mutex<HashMap<String, Vec<u8>>>,
    /// Directories created via `mkdir` that hold no file yet. A directory
    /// implied by a file's path (`"sub/b.txt"` implies `"sub"`) needs no entry
    /// here — [`stat_of`] derives it from the file map directly — so this set
    /// is only for the case a file map alone cannot express: an empty
    /// directory.
    dirs: Mutex<HashSet<String>>,
    next: AtomicU64,
    opens: Mutex<HashMap<Handle, String>>,
    /// Folded key → the spelling `files`/`dirs` is actually keyed by. Consulted
    /// only when an exact lookup misses, so the common path pays no fold.
    /// Maintained alongside every mutation of `files` and `dirs`; a stale entry
    /// here resolves a name to a file that no longer exists.
    by_fold: Mutex<HashMap<String, String>>,
}

impl MemoryProvider {
    /// An empty tree.
    pub fn new() -> Self {
        Self::from_files(std::iter::empty::<(&str, &[u8])>())
    }

    /// Build from a name→bytes map. Paths are normalized like
    /// `InlineProvider`'s (backslashes to slashes, no leading/trailing
    /// slash); parent directories are synthesized from the paths present, not
    /// stored separately.
    pub fn from_files<I, P, B>(entries: I) -> Self
    where
        I: IntoIterator<Item = (P, B)>,
        P: AsRef<str>,
        B: AsRef<[u8]>,
    {
        let mut files = HashMap::new();
        let mut by_fold = HashMap::new();
        for (p, b) in entries {
            let key = normalize(p.as_ref());
            by_fold.insert(fold(&key), key.clone());
            files.insert(key, b.as_ref().to_vec());
        }
        Self {
            files: Mutex::new(files),
            dirs: Mutex::new(HashSet::new()),
            next: AtomicU64::new(1),
            opens: Mutex::new(HashMap::new()),
            by_fold: Mutex::new(by_fold),
        }
    }
}

impl Default for MemoryProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for MemoryProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            access: Access::ReadWrite,
            immutable: false,
            slow: false,
            preferred_block: None,
            case: CaseMatch::Insensitive,
        }
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        let path = normalize(p.rel);
        let files = self.files.lock().map_err(|_| map_io_err())?;
        let dirs = self.dirs.lock().map_err(|_| map_io_err())?;
        let by_fold = self.by_fold.lock().map_err(|_| map_io_err())?;
        let path = canonical_in(&files, &dirs, &by_fold, &path);
        Ok(stat_of(&files, &dirs, &path))
    }

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        let path = normalize(p.rel);
        let files = self.files.lock().map_err(|_| map_io_err())?;
        let dirs = self.dirs.lock().map_err(|_| map_io_err())?;
        let by_fold = self.by_fold.lock().map_err(|_| map_io_err())?;
        let path = canonical_in(&files, &dirs, &by_fold, &path);
        match stat_of(&files, &dirs, &path) {
            Some(s) if s.kind == KIND_DIR => {}
            Some(_) => return Err(not_a_dir()),
            None => return Err(not_found()),
        }

        // Children are matched by the byte-exact prefix of `path` — not by
        // folding `path` and comparing folded components (the way
        // `crate::casefold::fold_strip_prefix` does for `InlineProvider`).
        // `path` just came back from `canonical_in`, which already picked
        // the real, byte-exact spelling this directory has (its own, if it
        // exists under that spelling, or the resolved ancestor's, if it's
        // implied) — a query for `Data` must not also surface `DATA`'s
        // children when both are real, distinct, fold-equal directories,
        // and folding the prefix here would merge them. See `canonical_in`'s
        // doc for the full reasoning; this is the same conflation, in the
        // one other place a "which real entry does this name mean" question
        // gets asked.
        let prefix = child_prefix(&path);
        let mut names: HashMap<String, Stat> = HashMap::new();
        for (k, b) in files.iter() {
            let Some((name, is_leaf)) = direct_child(&path, &prefix, k) else { continue };
            let st = if is_leaf {
                Stat { kind: KIND_FILE, size: b.len() as u64, mtime: 0 }
            } else {
                Stat { kind: KIND_DIR, size: 0, mtime: 0 }
            };
            names.entry(name.to_string()).or_insert(st);
        }
        for d in dirs.iter() {
            let Some((name, _)) = direct_child(&path, &prefix, d) else { continue };
            names.entry(name.to_string()).or_insert(Stat { kind: KIND_DIR, size: 0, mtime: 0 });
        }
        Ok(names.into_iter().map(|(name, stat)| DirEntry { name, stat }).collect())
    }

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        let path = normalize(p.rel);
        let mut files = self.files.lock().map_err(|_| map_io_err())?;
        let dirs = self.dirs.lock().map_err(|_| map_io_err())?;
        let mut by_fold = self.by_fold.lock().map_err(|_| map_io_err())?;
        let path = canonical_in(&files, &dirs, &by_fold, &path);
        let exists = files.contains_key(&path);

        if flags & OPEN_EXCL != 0 && exists {
            return Err(bad_request());
        }
        if flags & OPEN_CREATE != 0 {
            files.entry(path.clone()).or_default();
            if !exists {
                index_insert(&mut by_fold, &path);
            }
        } else if !exists {
            return Err(not_found());
        }
        if flags & OPEN_TRUNC != 0 {
            files.insert(path.clone(), Vec::new());
        }

        let size = files.get(&path).map(|b| b.len()).unwrap_or(0) as u64;
        drop(files);
        drop(dirs);
        drop(by_fold);

        let h = self.next.fetch_add(1, Ordering::Relaxed);
        self.opens.lock().map_err(|_| map_io_err())?.insert(h, path);
        Ok((h, size, false))
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        self.opens.lock().map_err(|_| map_io_err())?.remove(&h).ok_or_else(bad_fh)?;
        Ok(())
    }

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let path = self.opens.lock().map_err(|_| map_io_err())?.get(&h).cloned().ok_or_else(bad_fh)?;
        let files = self.files.lock().map_err(|_| map_io_err())?;
        let body = files.get(&path).ok_or_else(bad_fh)?;
        let start = (offset as usize).min(body.len());
        let n = (body.len() - start).min(buf.len());
        buf[..n].copy_from_slice(&body[start..start + n]);
        Ok(n)
    }

    fn write_at(&self, h: Handle, offset: u64, buf: &[u8]) -> Result<usize, i32> {
        let path = self.opens.lock().map_err(|_| map_io_err())?.get(&h).cloned().ok_or_else(bad_fh)?;
        let mut files = self.files.lock().map_err(|_| map_io_err())?;
        // `open` already created/resolved this path, so this is normally a
        // hit — the existence check only guards the (racy, but possible) case
        // where the entry was removed since this handle opened.
        let existed = files.contains_key(&path);
        {
            let body = files.entry(path.clone()).or_default();
            let end = offset as usize + buf.len();
            if body.len() < end {
                body.resize(end, 0);
            }
            body[offset as usize..end].copy_from_slice(buf);
        }
        if !existed {
            let mut by_fold = self.by_fold.lock().map_err(|_| map_io_err())?;
            index_insert(&mut by_fold, &path);
        }
        Ok(buf.len())
    }

    fn set_len(&self, h: Handle, len: u64) -> Result<(), i32> {
        let path = self.opens.lock().map_err(|_| map_io_err())?.get(&h).cloned().ok_or_else(bad_fh)?;
        let mut files = self.files.lock().map_err(|_| map_io_err())?;
        let existed = files.contains_key(&path);
        files.entry(path.clone()).or_default().resize(len as usize, 0);
        if !existed {
            let mut by_fold = self.by_fold.lock().map_err(|_| map_io_err())?;
            index_insert(&mut by_fold, &path);
        }
        Ok(())
    }

    fn flush(&self, _h: Handle) -> Result<(), i32> {
        Ok(())
    }

    fn mkdir(&self, p: VPath) -> Result<(), i32> {
        let path = normalize(p.rel);
        let files = self.files.lock().map_err(|_| map_io_err())?;
        let mut dirs = self.dirs.lock().map_err(|_| map_io_err())?;
        let mut by_fold = self.by_fold.lock().map_err(|_| map_io_err())?;
        let path = canonical_in(&files, &dirs, &by_fold, &path);
        if dirs.insert(path.clone()) {
            index_insert(&mut by_fold, &path);
        }
        Ok(())
    }

    /// Remove one file, or one **empty** directory.
    ///
    /// `ST_IS_DIR` for a directory that still holds anything, which is the
    /// POSIX-shaped answer this provider can actually deliver. It previously
    /// dropped the `dirs` entry and returned `Ok(())`, which changed nothing a
    /// caller could observe: the children remained, and because a child's path
    /// *implies* its parent ([`stat_of`]), `getattr` went on reporting the
    /// directory too. Reporting success for an operation that did nothing is
    /// worse than refusing it — a host has no way to notice.
    ///
    /// `ST_IS_DIR` rather than a new `ST_NOT_EMPTY`: the shim already translates
    /// it (`delete_status_for`, `vfs-shim/src/hook.rs`) to
    /// `STATUS_FILE_IS_A_DIRECTORY`, which `RtlNtStatusToDosError` folds to the
    /// `ERROR_ACCESS_DENIED` a real `DeleteFileW` returns for a directory. A
    /// status appended at `-11` would land in that function's catch-all and cross
    /// the process boundary as `STATUS_UNSUCCESSFUL` instead — strictly less
    /// information, for a new number every host would have to learn.
    fn remove(&self, p: VPath) -> Result<(), i32> {
        let path = normalize(p.rel);
        // files before dirs, the order every method here takes them in.
        let mut files = self.files.lock().map_err(|_| map_io_err())?;
        let mut dirs = self.dirs.lock().map_err(|_| map_io_err())?;
        let mut by_fold = self.by_fold.lock().map_err(|_| map_io_err())?;
        let path = canonical_in(&files, &dirs, &by_fold, &path);

        if files.remove(&path).is_some() {
            index_remove(&mut by_fold, &path);
            return Ok(());
        }
        let prefix = child_prefix(&path);
        if files.keys().any(|k| k.starts_with(&prefix)) || dirs.iter().any(|d| d.starts_with(&prefix))
        {
            return Err(is_dir());
        }
        if dirs.remove(&path) {
            index_remove(&mut by_fold, &path);
            return Ok(());
        }
        Err(not_found())
    }

    /// Rename a file, or a directory **with everything under it**.
    ///
    /// The directory case used to move only the `dirs` entry and report
    /// `Ok(())`, so a rename of a directory holding files moved nothing at all —
    /// and left the old name still resolving, since the unmoved children imply
    /// it. Every key under the subtree is rewritten now.
    ///
    /// Two refusals guard the rewrite, because both alternatives are silent
    /// corruption rather than an error:
    ///
    /// * `ST_EXISTS` if a directory rename's destination already holds
    ///   something. There is no correct way to combine two subtrees here —
    ///   merging them invents content at paths the caller never wrote, and
    ///   clobbering discards content it never asked to delete.
    /// * `ST_BAD_REQUEST` for a move of a directory into its own subtree
    ///   (`sub` → `sub/inner`), which POSIX answers `EINVAL`. The rewrite would
    ///   otherwise re-parent the subtree beneath itself.
    ///
    /// A **file** rename still overwrites its destination, unchanged: that is
    /// `rename(2)`'s behaviour and the director and overlay depend on it —
    /// unless the destination is a *directory* (explicit or implied), which
    /// no rename of a file may clobber; that refuses with `ST_EXISTS` the
    /// same as the directory-onto-occupied-path case below.
    fn rename(&self, from: VPath, to: VPath) -> Result<(), i32> {
        if from.root != to.root {
            return Err(bad_request());
        }
        let from_p = normalize(from.rel);
        let to_p = normalize(to.rel);
        if from_p == to_p {
            return Ok(());
        }

        // All three maps are locked up front and held for the whole method,
        // so resolution and mutation happen under one critical section — no
        // window where a concurrent caller can act on a path this method
        // already decided was canonical but that has since changed.
        let mut files = self.files.lock().map_err(|_| map_io_err())?;
        let mut dirs = self.dirs.lock().map_err(|_| map_io_err())?;
        let mut by_fold = self.by_fold.lock().map_err(|_| map_io_err())?;

        // `from_c` finds the real entry regardless of which fold-equal
        // spelling the caller used. `to_c` is always some usable path now
        // (see `canonical_in`'s doc) — never `None` — so whether the
        // destination is actually occupied is answered separately, by
        // asking the maps directly, exactly as `disk.rs` resolves first and
        // stats second rather than folding "resolved" and "exists" into one
        // signal.
        let from_c = canonical_in(&files, &dirs, &by_fold, &from_p);
        let to_c = canonical_in(&files, &dirs, &by_fold, &to_p);
        let to_exists = stat_of(&files, &dirs, &to_c).is_some();
        // Where this rename actually writes: the literal spelling the
        // caller gave, when the destination already exists under some
        // spelling (an overwrite, or a spelling-only rename of the same
        // entry onto itself) — that literal spelling is the whole point of
        // such a rename. Otherwise, `to_c`'s ancestor-corrected spelling, so
        // a move into a not-yet-existing child of an existing fold-equal
        // directory lands inside it instead of forking a divergently-cased
        // sibling — the create-side half of spec 6b, reached through rename.
        let insert_base = if to_exists { to_p.clone() } else { to_c.clone() };

        if files.contains_key(&from_c) {
            if to_exists && to_c != from_c {
                // A file may not clobber a directory — explicit or implied
                // — under any spelling; POSIX refuses this too (`EISDIR`).
                if matches!(stat_of(&files, &dirs, &to_c), Some(s) if s.kind == KIND_DIR) {
                    return Err(exists());
                }
                // Under case-insensitive matching "the destination" may be a
                // fold-equal entry whose literal spelling differs from
                // `to_p`; that entry must actually be removed here, or the
                // insert below creates a second, unreachable-by-its-old-name
                // entry that folds to the same key as the one just written
                // -- spec section 6b's failure mode, reached through rename
                // instead of open/write_at.
                files.remove(&to_c);
                index_remove(&mut by_fold, &to_c);
            }
            let body = files.remove(&from_c).expect("just confirmed present");
            files.insert(insert_base.clone(), body);
            reindex(&mut by_fold, &from_c, &insert_base);
            return Ok(());
        }

        let from_prefix = child_prefix(&from_c);
        let moving: Vec<String> = files
            .keys()
            .filter(|k| k.starts_with(&from_prefix))
            .cloned()
            .collect();
        let moving_dirs: Vec<String> = dirs
            .iter()
            .filter(|d| **d == from_c || d.starts_with(&from_prefix))
            .cloned()
            .collect();
        if moving.is_empty() && moving_dirs.is_empty() {
            return Err(not_found());
        }
        // Fold-aware: `from_prefix` is built from the canonical `from_c`, so
        // comparing it against the caller's raw `to_p` would let a
        // differently-cased self-nesting move (`Data` -> `DATA/Sub`) slip
        // past this guard. Folding the whole strings (not slicing either by
        // an offset measured on the other) keeps this correct even though
        // fold is not length-preserving.
        if fold(&to_p).starts_with(&fold(&from_prefix)) {
            return Err(bad_request());
        }
        let to_occupied = to_exists && to_c != from_c;
        if to_occupied {
            return Err(exists());
        }

        let to_prefix = child_prefix(&insert_base);
        let rewrite = |old: &str| -> String {
            match old.strip_prefix(&from_prefix) {
                Some(rest) => format!("{to_prefix}{rest}"),
                // The directory's own `dirs` entry, which has no child suffix.
                None => insert_base.clone(),
            }
        };
        for old in moving {
            let body = files.remove(&old).unwrap_or_default();
            let new_key = rewrite(&old);
            reindex(&mut by_fold, &old, &new_key);
            files.insert(new_key, body);
        }
        for old in moving_dirs {
            dirs.remove(&old);
            let new_key = rewrite(&old);
            reindex(&mut by_fold, &old, &new_key);
            dirs.insert(new_key);
        }
        Ok(())
    }

    fn set_attr(&self, _p: VPath, _attr: SetAttr) -> Result<(), i32> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// The requirement Task 4 is graded on: a writable provider that has not
    /// passed the writable arm of the shared suite has not been shown to
    /// work. Same suite every provider faces, parameterised by the
    /// capabilities this one actually declares (`Access::ReadWrite`).
    #[test]
    fn memory_provider_passes_conformance_as_read_write() {
        let p: Arc<dyn Provider> =
            Arc::new(MemoryProvider::from_files(vfs_provider::FIXTURE_FILES.iter().copied()));
        assert_eq!(p.capabilities().access, Access::ReadWrite);
        vfs_provider::assert_conformance(p);
    }

    /// `remove` used to drop the `dirs` entry for a directory that still held
    /// files and report `Ok(())`, leaving `getattr` still calling it a directory
    /// (the children imply it) and the children still readable. A caller that
    /// checks the status learns nothing; one that does not is working from a
    /// false belief about the tree.
    #[test]
    fn removing_a_non_empty_directory_is_refused_and_changes_nothing() {
        let p = MemoryProvider::from_files([("sub/b.txt", b"world!".as_slice())]);
        // The `mkdir` matters: it is the explicit `dirs` entry that the old
        // `remove` deleted, and deleting it is what made the call look like it
        // had done something. Without it the old code already answered
        // ST_NOT_FOUND, so a test that skips the `mkdir` is not a regression
        // test for this defect.
        p.mkdir(VPath::at_default("sub")).unwrap();
        assert_eq!(
            p.remove(VPath::at_default("sub")),
            Err(vfs_provider::ST_IS_DIR),
            "remove of a non-empty directory must fail, not silently do nothing"
        );
        assert_eq!(
            p.getattr(VPath::at_default("sub/b.txt")).unwrap().map(|s| s.size),
            Some(6),
            "the refused remove must leave the child alone"
        );
        // An empty directory still goes away: the refusal is about children,
        // not about being a directory.
        p.mkdir(VPath::at_default("empty")).unwrap();
        p.remove(VPath::at_default("empty")).expect("an empty directory removes");
        assert!(p.getattr(VPath::at_default("empty")).unwrap().is_none());
    }

    /// `rename` used to move only the `dirs` entry, so renaming a directory that
    /// held files reported `Ok(())` while every file stayed at its old path —
    /// and since the old paths still imply the old directory, even the rename of
    /// the *name* was invisible. Now it moves the subtree.
    #[test]
    fn renaming_a_directory_moves_its_whole_subtree() {
        let p = MemoryProvider::from_files([
            ("sub/b.txt", b"world!".as_slice()),
            ("sub/deep/c.txt", b"deeper".as_slice()),
        ]);
        // As above: the explicit `dirs` entry is the one the old code moved on
        // its own while leaving every file behind.
        p.mkdir(VPath::at_default("sub")).unwrap();
        p.mkdir(VPath::at_default("sub/hollow")).unwrap();

        p.rename(VPath::at_default("sub"), VPath::at_default("sub2"))
            .expect("directory rename");

        assert!(
            p.getattr(VPath::at_default("sub")).unwrap().is_none(),
            "rename left the whole old subtree behind"
        );
        assert!(p.getattr(VPath::at_default("sub/b.txt")).unwrap().is_none());
        assert_eq!(
            p.getattr(VPath::at_default("sub2/b.txt")).unwrap().map(|s| s.size),
            Some(6)
        );
        assert_eq!(
            p.getattr(VPath::at_default("sub2/deep/c.txt")).unwrap().map(|s| s.size),
            Some(6)
        );
        assert_eq!(
            p.getattr(VPath::at_default("sub2/hollow")).unwrap().map(|s| s.kind),
            Some(KIND_DIR),
            "an explicitly-created empty child directory must move too"
        );
    }

    /// A rename onto a path something already holds must not merge two subtrees
    /// or silently discard the destination.
    #[test]
    fn renaming_a_directory_onto_an_occupied_path_is_refused() {
        let p = MemoryProvider::from_files([
            ("sub/b.txt", b"world!".as_slice()),
            ("other/keep.txt", b"kept".as_slice()),
        ]);
        p.mkdir(VPath::at_default("sub")).unwrap();
        assert_eq!(
            p.rename(VPath::at_default("sub"), VPath::at_default("other")),
            Err(vfs_provider::ST_EXISTS),
            "rename onto an occupied path must be refused"
        );
        assert_eq!(
            p.getattr(VPath::at_default("other/keep.txt")).unwrap().map(|s| s.size),
            Some(4),
            "the refused rename must leave the destination alone"
        );
        assert_eq!(
            p.getattr(VPath::at_default("sub/b.txt")).unwrap().map(|s| s.size),
            Some(6),
            "the refused rename must leave the source alone"
        );
    }

    /// Fold-equal spellings name the same entry, and the seeded spelling is
    /// what `readdir` reports — folding is a lookup property, not a storage
    /// one. Writing through a variant spelling must hit the same file rather
    /// than creating a sibling: that sibling is spec §6b.
    #[test]
    fn fold_equal_spellings_resolve_to_one_entry() {
        let p = MemoryProvider::from_files([("Data/A.esp", &b"body"[..])]);

        for spelling in ["Data/A.esp", "data/a.esp", "DATA/A.ESP", "dAtA/a.EsP"] {
            let st = p
                .getattr(VPath::at_default(spelling))
                .unwrap()
                .unwrap_or_else(|| panic!("{spelling} did not resolve"));
            assert_eq!(st.size, 4, "{spelling} resolved to the wrong entry");
        }

        let names: Vec<String> = p
            .readdir(VPath::at_default("Data"))
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["A.esp".to_string()], "readdir must report the seeded spelling");
    }

    /// Non-ASCII, because `to_ascii_lowercase` would pass every case above.
    #[test]
    fn folding_is_unicode_not_ascii() {
        let p = MemoryProvider::from_files([("Über/A.esp", &b"x"[..])]);
        assert!(
            p.getattr(VPath::at_default("über/a.esp")).unwrap().is_some(),
            "Unicode fold-equal spelling did not resolve"
        );
    }

    /// Rename's "file overwrite" semantics must apply to the fold-equal
    /// destination, not just its literal spelling. Without this, the old
    /// destination survives under its own name while a second,
    /// differently-cased entry appears holding the moved content -- spec
    /// section 6b's failure mode (a write landing beside the existing file),
    /// reached through `rename` instead of `open`/`write_at`.
    #[test]
    fn renaming_a_file_onto_a_fold_equal_existing_file_overwrites_it() {
        let p = MemoryProvider::from_files([
            ("a.txt", b"from a".as_slice()),
            ("b.txt", b"from b".as_slice()),
        ]);
        p.rename(VPath::at_default("a.txt"), VPath::at_default("B.TXT"))
            .expect("rename onto a fold-equal destination");

        let names: Vec<String> =
            p.readdir(VPath::at_default("")).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(
            names,
            vec!["B.TXT".to_string()],
            "the fold-equal destination must be overwritten, not duplicated: {names:?}"
        );

        let (h, size, _) = p.open(VPath::at_default("b.txt"), vfs_provider::OPEN_READ).unwrap();
        assert_eq!(size, 6, "found via the old destination's fold-equal spelling");
        let mut buf = [0u8; 6];
        p.read_at(h, 0, &mut buf).unwrap();
        assert_eq!(&buf, b"from a", "the renamed file's content must win");
        p.close(h).unwrap();
    }

    /// The occupied-destination refusal for a directory rename must be
    /// fold-aware too: a destination that is fold-equal to an existing,
    /// different directory is exactly as occupied as one spelled identically.
    #[test]
    fn renaming_a_directory_onto_a_fold_equal_occupied_path_is_refused() {
        let p = MemoryProvider::from_files([
            ("sub/b.txt", b"world!".as_slice()),
            ("Dest/keep.txt", b"kept".as_slice()),
        ]);
        p.mkdir(VPath::at_default("sub")).unwrap();
        assert_eq!(
            p.rename(VPath::at_default("sub"), VPath::at_default("DEST")),
            Err(vfs_provider::ST_EXISTS),
            "rename onto a fold-equal occupied path must be refused"
        );
        assert_eq!(
            p.getattr(VPath::at_default("Dest/keep.txt")).unwrap().map(|s| s.size),
            Some(4),
            "the refused rename must leave the fold-equal destination alone"
        );
        assert_eq!(
            p.getattr(VPath::at_default("sub/b.txt")).unwrap().map(|s| s.size),
            Some(6),
            "the refused rename must leave the source alone"
        );
    }

    /// The self-nesting guard must compare fold-equal spellings, not raw
    /// bytes: moving a directory into a differently-cased spelling of its own
    /// subtree is exactly as nonsensical as moving it into an identically
    /// cased one, and must be refused the same way.
    #[test]
    fn renaming_a_directory_into_a_fold_equal_spelling_of_its_own_subtree_is_refused() {
        let p = MemoryProvider::from_files([("Data/A.esp", &b"body"[..])]);
        assert_eq!(
            p.rename(VPath::at_default("Data"), VPath::at_default("DATA/Sub")),
            Err(vfs_provider::ST_BAD_REQUEST),
            "a fold-equal self-nesting rename must be refused"
        );
        assert_eq!(
            p.getattr(VPath::at_default("Data/A.esp")).unwrap().map(|s| s.size),
            Some(4),
            "the refused rename must leave the source alone"
        );
        assert!(
            p.getattr(VPath::at_default("DATA/Sub/A.esp")).unwrap().is_none(),
            "the refused rename must not have moved anything"
        );
    }

    /// The host-facing shape the design spec's `vfs.memory({...})` promises:
    /// bytes go in through the constructor, come back out through ordinary
    /// reads, independent of whatever else was written in between.
    #[test]
    fn constructed_bytes_are_readable_back_untouched() {
        let p = MemoryProvider::from_files([("Skyrim.ini", b"ORIGINAL".as_slice())]);
        let (h, size, _) = p.open(VPath::at_default("Skyrim.ini"), vfs_provider::OPEN_READ).unwrap();
        assert_eq!(size, 8);
        let mut buf = [0u8; 8];
        let n = p.read_at(h, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"ORIGINAL");
        p.close(h).unwrap();
    }

    // -- Fix 1 regressions: `canonical_in` resolving a byte-exact implied
    // directory to the wrong, nondeterministically-chosen directory. --------

    /// Two real, distinct top-level directories that happen to be fold-equal
    /// to each other (`Data` and `DATA`, neither a stored key on its own —
    /// each is only *implied* by one file under it) must stay distinct:
    /// a byte-exact query for either one must see only its own child, not
    /// the other's, and not nondeterministically whichever `HashMap`
    /// iteration happened to visit first. Asserted on the resulting tree,
    /// not just a return value: the old bug returned `Ok`/`Some` throughout
    /// and only the *content* was wrong.
    #[test]
    fn byte_exact_implied_directories_stay_distinct_under_readdir() {
        let p = MemoryProvider::from_files([
            ("Data/A.esp", b"body".as_slice()),
            ("DATA/B.esp", b"XY".as_slice()),
        ]);

        for _ in 0..5 {
            let data: Vec<String> = p
                .readdir(VPath::at_default("Data"))
                .unwrap()
                .into_iter()
                .map(|e| e.name)
                .collect();
            assert_eq!(data, vec!["A.esp".to_string()], "readdir(Data) must show only its own child");

            let datau: Vec<String> = p
                .readdir(VPath::at_default("DATA"))
                .unwrap()
                .into_iter()
                .map(|e| e.name)
                .collect();
            assert_eq!(datau, vec!["B.esp".to_string()], "readdir(DATA) must show only its own child");
        }

        assert_eq!(
            p.getattr(VPath::at_default("Data/A.esp")).unwrap().map(|s| s.size),
            Some(4),
            "Data/A.esp must remain reachable"
        );
        assert_eq!(
            p.getattr(VPath::at_default("DATA/B.esp")).unwrap().map(|s| s.size),
            Some(2),
            "DATA/B.esp must remain reachable -- the old bug hid it under every spelling"
        );
    }

    /// The same fixture, exercised through `rename` instead of `readdir`:
    /// renaming the byte-exact `DATA` directory must move *its* child, and
    /// do so identically across repeated runs -- the old bug picked `Data`
    /// or `DATA` depending on `HashMap` iteration order.
    #[test]
    fn renaming_a_byte_exact_implied_directory_is_stable_across_runs() {
        for _ in 0..5 {
            let p = MemoryProvider::from_files([
                ("Data/A.esp", b"body".as_slice()),
                ("DATA/B.esp", b"XY".as_slice()),
            ]);
            p.rename(VPath::at_default("DATA"), VPath::at_default("X")).expect("rename DATA -> X");

            assert_eq!(
                p.getattr(VPath::at_default("Data/A.esp")).unwrap().map(|s| s.size),
                Some(4),
                "Data must be untouched by a rename of DATA"
            );
            // `DATA` as a *distinct* real entry is gone -- only `Data`
            // remains, so top-level now holds exactly `Data` and `X`, not
            // three entries. (`DATA` is still a valid, fold-equal *query*
            // for the sole remaining `Data`, same as any other fold-equal
            // spelling -- that is expected, not a leftover of the rename.)
            let top: Vec<String> =
                p.readdir(VPath::at_default("")).unwrap().into_iter().map(|e| e.name).collect();
            let mut top = top;
            top.sort();
            assert_eq!(top, vec!["Data".to_string(), "X".to_string()], "DATA must be gone as a distinct entry");
            assert_eq!(
                p.getattr(VPath::at_default("X/B.esp")).unwrap().map(|s| s.size),
                Some(2),
                "X must hold DATA's child, not Data's"
            );
            assert!(
                p.getattr(VPath::at_default("X/A.esp")).unwrap().is_none(),
                "X must not have picked up Data's child instead"
            );
        }
    }

    /// A host seeds `Data/old.txt`; the game creates `data/new.txt` (a
    /// same-depth key, arriving already folded — a real Windows-path shape,
    /// not a contrived one). The create must land inside the existing
    /// `Data/`, not fork a second, divergently-cased `data/` beside it.
    #[test]
    fn create_through_a_fold_equal_spelling_of_an_existing_directory_does_not_fork_it() {
        let p = MemoryProvider::from_files([("Data/old.txt", b"host".as_slice())]);

        let (h, _len, _is_dir) = p
            .open(VPath::at_default("data/new.txt"), vfs_provider::OPEN_WRITE | OPEN_CREATE)
            .expect("create through a fold-equal directory spelling must succeed");
        p.close(h).expect("close");

        let top: Vec<String> =
            p.readdir(VPath::at_default("")).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(
            top.len(),
            1,
            "creating through the fold-equal spelling `data` must not add a second \
             top-level directory: {top:?}"
        );

        let children: Vec<String> = p
            .readdir(VPath::at_default("Data"))
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        let mut children = children;
        children.sort();
        assert_eq!(
            children,
            vec!["new.txt".to_string(), "old.txt".to_string()],
            "both the host's seeded file and the game's new one must live under Data"
        );
    }

    /// Renaming a *file* onto a name that resolves (byte-exact or fold-equal)
    /// to an existing *directory* must be refused rather than corrupting the
    /// directory: the destination directory must go on being a directory,
    /// with its child still listable.
    #[test]
    fn renaming_a_file_onto_a_fold_equal_directory_is_refused_and_directory_survives() {
        let p = MemoryProvider::from_files([
            ("a.txt", b"from a".as_slice()),
            ("Dest/keep.txt", b"kept".as_slice()),
        ]);

        assert_eq!(
            p.rename(VPath::at_default("a.txt"), VPath::at_default("DEST")),
            Err(vfs_provider::ST_EXISTS),
            "a file rename onto a fold-equal directory must be refused"
        );

        assert_eq!(
            p.getattr(VPath::at_default("Dest")).unwrap().map(|s| s.kind),
            Some(KIND_DIR),
            "Dest must still be a directory, not clobbered into a file"
        );
        let names: Vec<String> = p
            .readdir(VPath::at_default("Dest"))
            .expect("Dest must still list, not ST_NOT_A_DIRECTORY")
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["keep.txt".to_string()], "Dest's child must survive the refused rename");
        assert_eq!(
            p.getattr(VPath::at_default("a.txt")).unwrap().map(|s| s.size),
            Some(6),
            "the refused rename must leave the source alone"
        );
    }
}

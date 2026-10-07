//! Path normalization for the userspace FUSE kernel.

/// A path that could not be normalized: it escaped the root via `..`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathError;

/// Normalize to `/`-separated, no leading slash, no `.` / `..` segments.
pub fn normalize(raw: &str) -> Result<String, PathError> {
    let s = raw.replace('\\', "/");
    let s = s.trim_matches('/');
    if s.is_empty() {
        return Ok(String::new());
    }
    let mut out: Vec<&str> = Vec::new();
    for part in s.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            if out.pop().is_none() {
                return Err(PathError);
            }
            continue;
        }
        out.push(part);
    }
    Ok(out.join("/"))
}

/// Strip a mount prefix from a normalized path. Returns relative path inside the mount.
///
/// Matching folds both sides at compare time with [`vfs_core::fold`] — shim
/// vpaths arrive already folded by that same function (the shim's `RootMap`
/// folds every surviving component before the vpath crosses the ring), but a
/// mount's configured prefix (e.g. `Data/SomeMod`) is stored as-authored so
/// its original spelling survives for diagnostics and error messages. The
/// returned relative path is sliced out of `path` (not `prefix`), preserving
/// whatever case the query actually used.
///
/// Comparison walks path components rather than slicing at
/// `prefix.len()`: `fold` is Unicode, and a Unicode fold is not
/// length-preserving in UTF-8 (`İ` is two bytes and folds to three), so a
/// byte offset taken from the unfolded prefix is not an offset into the
/// folded path. Walking components sidesteps the question entirely and
/// keeps the segment-boundary rule (`data2/a` must not match `Data`) as a
/// consequence of the walk rather than a separate check.
pub fn strip_prefix(path: &str, prefix: &str) -> Option<String> {
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() {
        return Some(path.to_string());
    }
    let mut rest = path;
    for want in prefix.split('/').filter(|c| !c.is_empty()) {
        let (head, tail) = match rest.split_once('/') {
            Some((h, t)) => (h, t),
            // Last component of `path`: the prefix may end here, but if it
            // has more components left they have nothing to match against.
            None => (rest, ""),
        };
        if vfs_core::fold(head) != vfs_core::fold(want) {
            return None;
        }
        rest = tail;
    }
    Some(rest.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_basic() {
        assert_eq!(normalize(r"Data\Foo.esp").unwrap(), "Data/Foo.esp");
        assert_eq!(normalize("/a/./b/../c").unwrap(), "a/c");
        assert_eq!(normalize("").unwrap(), "");
        assert!(normalize("..").is_err());
    }

    #[test]
    fn strip_prefix_root() {
        assert_eq!(strip_prefix("a/b", "").unwrap(), "a/b");
        assert_eq!(strip_prefix("mod/a", "mod").unwrap(), "a");
        assert_eq!(strip_prefix("mod", "mod").unwrap(), "");
        assert!(strip_prefix("other", "mod").is_none());
    }

    #[test]
    fn strip_prefix_is_case_insensitive() {
        // Shim vpaths are always lowercased; a mount configured with mixed
        // case (as Mod Organizer style configs do) must still match.
        assert_eq!(
            strip_prefix("data/somemod/a", "Data/SomeMod").unwrap(),
            "a"
        );
        assert_eq!(strip_prefix("data/somemod", "Data/SomeMod").unwrap(), "");
        assert!(strip_prefix("data/othermod", "Data/SomeMod").is_none());
        // Boundary must still be a full path segment, not a substring match.
        assert!(strip_prefix("dat", "Data").is_none());
        assert!(strip_prefix("data2/a", "Data").is_none());
    }

    #[test]
    fn strip_prefix_folds_the_same_way_the_shim_does() {
        // The shim folds vpath components with `vfs_core::fold` (Unicode)
        // before they cross the ring, so a prefix whose case only Unicode
        // knows how to lower must still match. An ASCII-only compare here
        // silently answers `None` and the mount becomes unreachable.
        assert_eq!(
            strip_prefix(&vfs_core::fold("Data/ÜBER"), "Data/ÜBER").unwrap(),
            ""
        );
        assert_eq!(
            strip_prefix(&vfs_core::fold("Data/Мод/a.esp"), "Data/Мод").unwrap(),
            "a.esp"
        );
        // A Unicode fold can change a string's UTF-8 length (`İ` is two bytes,
        // folds to three), so prefix matching must not slice `path` at
        // `prefix.len()`.
        assert_eq!(vfs_core::fold("İ").len(), 3);
        assert_eq!("İ".len(), 2);
        assert_eq!(strip_prefix(&vfs_core::fold("İ/a"), "İ").unwrap(), "a");
    }
}

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
/// Matching folds ASCII case on both sides at compare time — shim vpaths are
/// always lowercased, but a mount's configured prefix (e.g. `Data/SomeMod`)
/// is stored as-authored so its original spelling survives for diagnostics
/// and error messages. The returned relative path is sliced out of `path`
/// (not `prefix`), preserving whatever case the query actually used.
pub fn strip_prefix(path: &str, prefix: &str) -> Option<String> {
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() {
        return Some(path.to_string());
    }
    if path.eq_ignore_ascii_case(prefix) {
        return Some(String::new());
    }
    let plen = prefix.len();
    // `get` (not raw slicing) so a `plen` that doesn't land on a char
    // boundary in `path` returns `None` instead of panicking.
    let head = path.get(..plen)?;
    if path.as_bytes().get(plen) != Some(&b'/') {
        return None;
    }
    if !head.eq_ignore_ascii_case(prefix) {
        return None;
    }
    Some(path[plen + 1..].to_string())
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
}

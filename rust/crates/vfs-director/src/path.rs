//! Path normalization for the userspace FUSE kernel.

/// Normalize to `/`-separated, no leading slash, no `.` / `..` segments.
pub fn normalize(raw: &str) -> Result<String, ()> {
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
                return Err(());
            }
            continue;
        }
        out.push(part);
    }
    Ok(out.join("/"))
}

/// Strip a mount prefix from a normalized path. Returns relative path inside the mount.
pub fn strip_prefix(path: &str, prefix: &str) -> Option<String> {
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() {
        return Some(path.to_string());
    }
    if path == prefix {
        return Some(String::new());
    }
    let pfx = format!("{prefix}/");
    path.strip_prefix(&pfx).map(|s| s.to_string())
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
}

//! Virtual path normalization.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    EscapesRoot,
    /// The path normalizes to the root itself (zero components) where a
    /// non-root path was required — e.g. a declared managed root, which must
    /// name a real subtree rather than "everything". See
    /// `vfs_redirect::RootMap::with_capacity`, which is the caller that
    /// matters: a root with zero components would match every path handed to
    /// it, with the whole path left over as the remainder.
    EmptyRoot,
}

/// Normalize a root-relative virtual path to canonical `/`-separated form.
/// `""` denotes the root. Deeper NT concerns (`\Device\…`, RootDirectory-relative
/// opens, 8.3 short names) are edge/shim concerns and out of scope here.
pub fn normalize_vpath(raw: &str) -> Result<String, PathError> {
    // Strip known NT/DOS long-path prefixes first (either slash form).
    let mut s = raw;
    for prefix in [r"\??\", r"\\?\", "/??/", "//?/"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest;
            break;
        }
    }

    let mut out: Vec<&str> = Vec::new();
    for comp in s.split(['/', '\\']) {
        match comp {
            "" | "." => continue,
            ".." => {
                if out.pop().is_none() {
                    return Err(PathError::EscapesRoot);
                }
            }
            other => out.push(other),
        }
    }
    Ok(out.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_separators_and_trims() {
        assert_eq!(normalize_vpath("data\\meshes\\a.nif").unwrap(), "data/meshes/a.nif");
        assert_eq!(normalize_vpath("/data/").unwrap(), "data");
        assert_eq!(normalize_vpath("data//meshes").unwrap(), "data/meshes");
    }

    #[test]
    fn empty_and_dot_are_root() {
        assert_eq!(normalize_vpath("").unwrap(), "");
        assert_eq!(normalize_vpath(".").unwrap(), "");
        assert_eq!(normalize_vpath("/").unwrap(), "");
    }

    #[test]
    fn resolves_dotdot() {
        assert_eq!(normalize_vpath("data/x/../y").unwrap(), "data/y");
        assert_eq!(normalize_vpath("a/b/../..").unwrap(), "");
    }

    #[test]
    fn dotdot_escaping_root_errors() {
        assert_eq!(normalize_vpath("..").unwrap_err(), PathError::EscapesRoot);
        assert_eq!(normalize_vpath("data/../..").unwrap_err(), PathError::EscapesRoot);
    }

    #[test]
    fn strips_nt_and_dos_prefixes() {
        assert_eq!(normalize_vpath(r"\??\data\a").unwrap(), "data/a");
        assert_eq!(normalize_vpath(r"\\?\data\a").unwrap(), "data/a");
    }
}

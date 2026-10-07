//! Virtual path normalization and splitting.
//!
//! Three kinds of helper live here, and they differ in what they do with a
//! `.` or `..` component:
//!
//! - **Resolving**: [`normalize_vpath`] walks `..` back up the path and
//!   fails with [`PathError::EscapesRoot`] if it climbs past the root. It is
//!   for paths that arrive from the guest and must name something inside a
//!   root.
//! - **Refusing**: [`rel_components`] and [`normalize_rel`] fail with
//!   [`BadComponent`] on any `.` or `..`, so a provider never has to decide
//!   what such a name means.
//! - **Neutral**: [`trim_rel`] and [`split_parent`] only handle separators
//!   and treat every component, `.` and `..` included, as an ordinary name.
//!   They are for callers that hand the path on unchanged (or look it up in
//!   a table where `..` simply is not found).
//!
//! All of them accept `/` and `\` as separators.

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

/// A `.` or `..` component where the caller refuses both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadComponent;

/// Separators unified to `/` and leading and trailing slashes removed.
/// Nothing else changes: empty, `.` and `..` components in the middle stay as
/// they are (the neutral rule in the module docs).
pub fn trim_rel(raw: &str) -> String {
    raw.replace('\\', "/").trim_matches('/').to_string()
}

/// The components of a relative path, in their original spelling. Either
/// separator splits; empty components are dropped; `.` and `..` are refused
/// (the refusing rule in the module docs).
pub fn rel_components(raw: &str) -> Result<Vec<&str>, BadComponent> {
    let mut out = Vec::new();
    for c in raw.split(['/', '\\']).filter(|c| !c.is_empty()) {
        if c == "." || c == ".." {
            return Err(BadComponent);
        }
        out.push(c);
    }
    Ok(out)
}

/// `raw` as `/`-joined [`rel_components`]: the canonical spelling of a path
/// that refuses `.` and `..`. `""` is the root.
pub fn normalize_rel(raw: &str) -> Result<String, BadComponent> {
    let mut out = String::with_capacity(raw.len());
    for c in rel_components(raw)? {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(c);
    }
    Ok(out)
}

/// `(parent, name)` of a `/`-separated path. A single component has the empty
/// parent. Neutral about `.` and `..`, and does not trim, so pass a path
/// that has been through [`trim_rel`] or [`normalize_rel`] when it may carry
/// outer slashes.
pub fn split_parent(rel: &str) -> (&str, &str) {
    rel.rsplit_once('/').unwrap_or(("", rel))
}

/// Normalize a root-relative virtual path to canonical `/`-separated form,
/// resolving `..` (the resolving rule in the module docs).
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
    fn trim_rel_is_neutral_about_dots() {
        assert_eq!(trim_rel("\\a\\b/"), "a/b");
        assert_eq!(trim_rel("/"), "");
        assert_eq!(trim_rel("a//b/../c/."), "a//b/../c/.");
    }

    #[test]
    fn rel_components_refuse_dots_and_drop_empties() {
        assert_eq!(rel_components("/a\\\\b//c/").unwrap(), ["a", "b", "c"]);
        assert_eq!(rel_components("").unwrap(), Vec::<&str>::new());
        assert_eq!(rel_components("a/./b"), Err(BadComponent));
        assert_eq!(rel_components("a\\..\\b"), Err(BadComponent));
        assert_eq!(rel_components(".hidden/..x").unwrap(), [".hidden", "..x"]);
    }

    #[test]
    fn normalize_rel_joins() {
        assert_eq!(normalize_rel("\\a//b\\c/").unwrap(), "a/b/c");
        assert_eq!(normalize_rel("/").unwrap(), "");
        assert_eq!(normalize_rel(".."), Err(BadComponent));
    }

    #[test]
    fn split_parent_edges() {
        assert_eq!(split_parent("a/b/c"), ("a/b", "c"));
        assert_eq!(split_parent("a"), ("", "a"));
        assert_eq!(split_parent(""), ("", ""));
        assert_eq!(split_parent("/a"), ("", "a"));
        assert_eq!(split_parent("a/"), ("a", ""));
    }

    #[test]
    fn strips_nt_and_dos_prefixes() {
        assert_eq!(normalize_vpath(r"\??\data\a").unwrap(), "data/a");
        assert_eq!(normalize_vpath(r"\\?\data\a").unwrap(), "data/a");
    }
}

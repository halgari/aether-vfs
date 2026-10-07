//! Request paths: split and folded.

use vfs_core::fold;
use vfs_provider::bad_request;

/// A request path, split into components in their original spelling.
pub(super) struct LPath {
    pub(super) parts: Vec<String>,
    pub(super) folded: String,
}

impl LPath {
    /// Backslashes as slashes, empty components dropped; `.` and `..` are
    /// refused rather than walked.
    pub(super) fn parse(rel: &str) -> Result<Self, i32> {
        let s = rel.replace('\\', "/");
        let parts: Vec<String> = s
            .split('/')
            .filter(|c| !c.is_empty())
            .map(str::to_owned)
            .collect();
        if parts.iter().any(|c| c == "." || c == "..") {
            return Err(bad_request());
        }
        let folded = fold(&parts.join("/"));
        Ok(LPath { parts, folded })
    }

    pub(super) fn is_root(&self) -> bool {
        self.parts.is_empty()
    }

    pub(super) fn name(&self) -> &str {
        self.parts.last().map_or("", String::as_str)
    }
}

/// The folded path [`LPath::parse`] gives for `rel`, without the components:
/// what a lookup that creates nothing needs. `getattr` runs for every
/// metadata question the overlay above passes down — nearly always for a
/// path this layer does not hold — so it is kept to two allocations.
pub(super) fn folded_path(rel: &str) -> Result<String, i32> {
    let mut joined = String::with_capacity(rel.len());
    for c in rel.split(['/', '\\']).filter(|c| !c.is_empty()) {
        if c == "." || c == ".." {
            return Err(bad_request());
        }
        if !joined.is_empty() {
            joined.push('/');
        }
        joined.push_str(c);
    }
    Ok(fold(&joined))
}

//! Root-scoped addressing. Every path handed to a provider is a `(root,
//! relative path)` pair, so one provider instance can serve several roots and
//! still tell `[1, "foo/bar"]` from `[0, "foo/bar"]`.

/// Identifies one virtualized filesystem location within a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RootId(pub u32);

impl RootId {
    /// The root every single-root session and every Stage-1 call site uses.
    pub const DEFAULT: RootId = RootId(0);
}

/// A path as a provider sees it: normalized, forward-slash separated, no
/// leading slash, provider root is `""`.
///
/// **Case is the caller's, and a provider must not depend on it.** The shim
/// folds a vpath before sending it (`vfs-redirect`'s `match_canonical`), while
/// host-side callers — `vfs-embed`, `vfs-node`, this crate's conformance suite —
/// send the original spelling. A provider therefore resolves fold-equal names
/// identically unless it declares [`crate::CaseMatch::Sensitive`]. An earlier
/// version of this comment said "original case preserved", which was true of
/// only one of the two paths and is what spec §6b was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VPath<'a> {
    pub root: RootId,
    pub rel: &'a str,
}

impl<'a> VPath<'a> {
    pub fn new(root: RootId, rel: &'a str) -> Self {
        VPath { root, rel }
    }

    /// Address under [`RootId::DEFAULT`].
    pub fn at_default(rel: &'a str) -> Self {
        VPath { root: RootId::DEFAULT, rel }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_relative_path_under_two_roots_differs() {
        assert_ne!(VPath::new(RootId(0), "foo/bar"), VPath::new(RootId(1), "foo/bar"));
    }

    #[test]
    fn at_default_uses_root_zero() {
        assert_eq!(VPath::at_default("a").root, RootId::DEFAULT);
        assert_eq!(RootId::DEFAULT, RootId(0));
    }

    #[test]
    fn the_provider_root_is_the_empty_string() {
        assert_eq!(VPath::at_default("").rel, "");
    }
}

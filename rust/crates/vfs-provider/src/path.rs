//! Root-scoped addressing. Every path handed to a provider is a `(root,
//! relative path)` pair, so one provider instance can serve several roots and
//! still tell `[1, "foo/bar"]` from `[0, "foo/bar"]`.

use std::path::{Path, PathBuf};

/// Identifies one virtualized filesystem location within a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RootId(pub u32);

impl RootId {
    /// The root every single-root session and every Stage-1 call site uses.
    pub const DEFAULT: RootId = RootId(0);
}

/// A path as a provider sees it: normalized, forward-slash separated, no
/// leading slash, provider root is `""`, original case preserved.
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

/// The physical subdirectory an [`Overlay`] rooted at `overlay_root` uses for
/// `root`'s writes — [`Overlay::root_dir`] calls this too, so it is the one
/// place the naming scheme is defined.
///
/// Exposed (re-exported at the crate root) because the shim's local overlay
/// is not the only thing that reads this directory: a host-side session can
/// separately mount a read layer (e.g. a `DiskProvider`) over the same
/// physical directory so the director sees what the overlay writes, without
/// the shim and the director ever talking to each other about it — the
/// filesystem is the shared state. That caller needs the exact subtree the
/// overlay actually uses, not a re-derived or hardcoded guess at it. See
/// `vfs-director::Session::overlay_layer_dir` and its caller in
/// `vfs-directord/src/bin/skyrim-live.rs`, which mounts
/// `overlay_layer_dir(&overrides, RootId::DEFAULT)` instead of `&overrides`
/// itself for exactly this reason.
///
/// **It lives in `vfs-provider` rather than in the shim** because the director
/// needs it and must not depend on Windows code to get it. Reaching it through
/// `vfs-shim` pulled `retour` — and therefore the C x86 disassembler
/// `libudis86-sys` — into the kernel's dependency graph, for two lines of path
/// joining. `vfs-provider` defines the `RootId` in the signature and has no
/// dependencies of its own, so the helper adds no edge anywhere.
pub fn overlay_layer_dir(overlay_root: &Path, root: RootId) -> PathBuf {
    overlay_root.join(format!("root-{}", root.0))
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

#[cfg(test)]
mod overlay_layer_dir_tests {
    use super::*;

    /// The naming scheme is `root-<n>` under the overlay root, and it is the
    /// contract between two processes that never talk to each other: the shim
    /// writes here and a host-side session mounts the same directory. A change
    /// to this string is a change to that contract.
    #[test]
    fn layer_dir_is_root_n_under_the_overlay_root() {
        let base = std::path::Path::new("/tmp/ov");
        assert_eq!(overlay_layer_dir(base, RootId::DEFAULT), base.join("root-0"));
        assert_eq!(overlay_layer_dir(base, RootId(1)), base.join("root-1"));
        assert_eq!(overlay_layer_dir(base, RootId(42)), base.join("root-42"));
    }

    /// Distinct roots never share a layer directory — that separation is the
    /// whole reason the helper takes a RootId.
    #[test]
    fn distinct_roots_get_distinct_directories() {
        let base = std::path::Path::new("/tmp/ov");
        assert_ne!(overlay_layer_dir(base, RootId(0)), overlay_layer_dir(base, RootId(1)));
    }
}

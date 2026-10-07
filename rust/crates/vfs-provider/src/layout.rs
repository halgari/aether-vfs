//! Physical, on-disk layout conventions.
//!
//! Unlike `path`'s virtual `(root, relative path)` addressing, this module
//! defines how roots map onto real filesystem paths — a convention shared
//! between processes that never talk to each other and only agree on it by
//! both reading this code.

use std::path::{Path, PathBuf};

use crate::path::RootId;

/// The physical subdirectory an overlay (`vfs_shim::Overlay`) rooted at
/// `overlay_root` uses for `root`'s writes — `vfs_shim::Overlay::root_dir`
/// calls this too, so it is the one place the naming scheme is defined.
///
/// Exposed (re-exported at the crate root) because the shim's local overlay
/// is not the only thing that reads this directory: a host-side session can
/// separately mount a read layer (e.g. a `DiskProvider`) over the same
/// physical directory so the director sees what the overlay writes, without
/// the shim and the director ever talking to each other about it — the
/// filesystem is the shared state. That caller needs the exact subtree the
/// overlay actually uses, not a re-derived or hardcoded guess at it. See
/// `vfs-director::Session::overlay_layer_dir` and its caller in
/// `vfs-bench/src/bin/skyrim-live.rs`, which mounts
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

/// The prefix of a whiteout marker: an overlay hides `<name>` of its base by
/// writing `.wh.<name>` next to where it would be, in the overlay's upper.
///
/// The marker convention lives here, with the rest of the on-disk layout, so
/// the overlay that writes the markers (`vfs-compose`) and whatever reads an
/// upper without it (`vfs-storage`'s layer export) cannot drift apart.
pub const WHITEOUT_PREFIX: &str = ".wh.";

/// The prefix of a copy-up staging file, `.cu.<n>.<name>`: the half-finished
/// copy an overlay writes before renaming it over `<name>`. One a crash left
/// behind is never served.
pub const COPY_UP_PREFIX: &str = ".cu.";

/// The whiteout marker that hides `name`: `.wh.<name>`.
pub fn whiteout_name(name: &str) -> String {
    format!("{WHITEOUT_PREFIX}{name}")
}

/// The staging name for copying `name` up, as the `n`th attempt:
/// `.cu.<n>.<name>`.
pub fn copy_up_name(n: u64, name: &str) -> String {
    format!("{COPY_UP_PREFIX}{n}.{name}")
}

/// Whether `name` (one path component) is an overlay marker rather than real
/// content: a whiteout or a copy-up staging file.
pub fn is_overlay_marker(name: &str) -> bool {
    name.starts_with(WHITEOUT_PREFIX) || name.starts_with(COPY_UP_PREFIX)
}

#[cfg(test)]
mod marker_tests {
    use super::*;

    #[test]
    fn names_round_trip_through_the_prefixes() {
        assert_eq!(whiteout_name("a.txt"), ".wh.a.txt");
        assert_eq!(copy_up_name(7, "a.txt"), ".cu.7.a.txt");
        assert_eq!(whiteout_name("a.txt").strip_prefix(WHITEOUT_PREFIX), Some("a.txt"));
        assert!(is_overlay_marker(&whiteout_name("x")));
        assert!(is_overlay_marker(&copy_up_name(1, "x")));
        assert!(!is_overlay_marker("wh.x"));
        assert!(!is_overlay_marker("a.wh.x"));
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
        let base = std::path::Path::new("/srv/ov");
        assert_eq!(overlay_layer_dir(base, RootId::DEFAULT), base.join("root-0"));
        assert_eq!(overlay_layer_dir(base, RootId(1)), base.join("root-1"));
        assert_eq!(overlay_layer_dir(base, RootId(42)), base.join("root-42"));
    }

    /// Distinct roots never share a layer directory — that separation is the
    /// whole reason the helper takes a RootId.
    #[test]
    fn distinct_roots_get_distinct_directories() {
        let base = std::path::Path::new("/srv/ov");
        assert_ne!(overlay_layer_dir(base, RootId(0)), overlay_layer_dir(base, RootId(1)));
    }
}

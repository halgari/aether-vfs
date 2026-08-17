//! Incremental per-root source bookkeeping: what a host records when it
//! learns about its sources one at a time, and how that record becomes the
//! mount list [`crate::Session::set_root_mounts`] takes.

use std::sync::Arc;

use vfs_compose::stack_layers;
use vfs_provider::Provider;

/// One root's sibling mounts, as [`crate::Session::set_root_mounts`] takes
/// them: `(mount prefix, provider)` in precedence order.
pub type RootMounts = Vec<(String, Arc<dyn Provider>)>;

/// One root's sources, accumulated, so its composed provider can be rebuilt
/// from scratch whenever a new one arrives.
///
/// `Director` holds exactly **one** provider per root, so there is no
/// incremental mount to append to — every new source rebuilds the whole root.
/// A host therefore has to keep the source list somewhere, and this is that
/// somewhere. It was `SessionRegistry`'s private `RootBuild`; it is public
/// here because the daemon is not the only host that adds sources one at a
/// time, and the alternative was every host reinventing the two rules below.
///
/// **The two rules it encodes:**
///
/// 1. Sources at the root (`""`, `"/"`, `"\"`) are **layers**: they merge, in
///    ascending `layer` order, via [`vfs_compose::stack_layers`] — directory
///    listings union and later layers win per entry. This is what a flat list
///    of mod directories means.
/// 2. Sources with a real mount prefix are **siblings** within the root: each
///    keeps its own prefix and a `MountGraph` routes a path to whichever one
///    owns it.
///
/// The layer stack is collapsed into a single `""` mount rather than handed
/// over as several, because the two compose differently on a shared path: a
/// `MountGraph` picks the last mount that *owns* the path, while a layered
/// stack merges the whole stack.
///
/// The write layer is **not** here. It is not one of these sources — it sits
/// above all of them as an overlay upper, which is the only composition that
/// makes a write to read-only content copy up. See
/// [`crate::Session::set_write_layer_at`] and [`crate::compose_root`].
#[derive(Default)]
pub struct RootSources {
    /// Root-mounted sources, bottom→top for rebuild via `stack_layers`.
    layers: Vec<(i32, Arc<dyn Provider>)>,
    /// Sources with a non-root mount prefix within this root, composed
    /// alongside the layered root by `MountGraph`.
    prefix_mounts: Vec<(String, Arc<dyn Provider>)>,
}

impl RootSources {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one source. `mount` is its prefix within the root — empty,
    /// `"/"` or `"\"` (in any amount of surrounding whitespace) all mean "the
    /// whole root", which is the layered case; anything else is a prefix
    /// mount. `layer` orders the layered case only, ascending, with insertion
    /// order preserved among equal values.
    pub fn add(&mut self, mount: &str, layer: i32, provider: Arc<dyn Provider>) {
        let mount_norm = mount.trim();
        let is_root = mount_norm.is_empty() || mount_norm == "/" || mount_norm == "\\";
        if is_root {
            self.layers.push((layer, provider));
            // Stable order for equal layers: preserve insertion order.
            self.layers.sort_by_key(|a| a.0);
        } else {
            self.prefix_mounts.push((mount.to_string(), provider));
        }
    }

    /// Whether anything has been recorded for this root yet.
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty() && self.prefix_mounts.is_empty()
    }

    /// This root's sibling mounts as [`crate::Session::set_root_mounts`] wants them:
    /// the root-mounted sources collapsed into one layered provider at `""`,
    /// then each prefixed source at its own prefix.
    pub fn mounts(&self) -> Result<RootMounts, String> {
        let mut mounts: RootMounts = Vec::new();
        if !self.layers.is_empty() {
            let stack: Vec<Arc<dyn Provider>> =
                self.layers.iter().map(|(_, b)| Arc::clone(b)).collect();
            mounts.push((String::new(), stack_layers(stack).map_err(|e| e.to_string())?));
        }
        for (pfx, be) in &self.prefix_mounts {
            mounts.push((pfx.clone(), Arc::clone(be)));
        }
        Ok(mounts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs_compose::InlineProvider;
    use vfs_provider::{RootId, VPath, OPEN_READ};

    fn inline(name: &str, bytes: &[u8]) -> Arc<dyn Provider> {
        Arc::new(InlineProvider::from_files([(name, bytes)]))
    }

    fn read(p: &Arc<dyn Provider>, root: RootId, rel: &str) -> Vec<u8> {
        let (h, size, _) = p.open(VPath::new(root, rel), OPEN_READ).unwrap();
        let mut buf = vec![0u8; size as usize];
        let n = p.read_at(h, 0, &mut buf).unwrap();
        buf.truncate(n);
        p.close(h).unwrap();
        buf
    }

    /// Rule 1: root-mounted sources *merge*, they do not shadow one another
    /// wholesale, and the highest layer wins per entry.
    #[test]
    fn root_mounted_sources_layer_by_ascending_layer_number() {
        let mut rs = RootSources::new();
        rs.add("/", 10, inline("shared.txt", b"TOP"));
        rs.add("", 0, inline("shared.txt", b"BOTTOM"));
        rs.add("\\", 5, inline("only-mid.txt", b"MID"));

        let mounts = rs.mounts().unwrap();
        assert_eq!(mounts.len(), 1, "layered sources collapse to one root mount");
        assert_eq!(mounts[0].0, "", "collapsed at the root prefix");
        assert_eq!(read(&mounts[0].1, RootId::DEFAULT, "shared.txt"), b"TOP");
        assert_eq!(
            read(&mounts[0].1, RootId::DEFAULT, "only-mid.txt"),
            b"MID",
            "a lower layer's exclusive content must survive the merge"
        );
    }

    /// Rule 2: a prefixed source is a sibling, kept separate, and does not
    /// join the layer stack.
    #[test]
    fn a_prefixed_source_stays_its_own_mount() {
        let mut rs = RootSources::new();
        rs.add("", 0, inline("a.txt", b"ROOT"));
        rs.add("/Data", 0, inline("b.txt", b"DATA"));

        let mounts = rs.mounts().unwrap();
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0].0, "");
        assert_eq!(mounts[1].0, "/Data");
    }

    /// A root with only prefixed sources must not produce an empty `""` mount
    /// — `stack_layers` rejects an empty stack, so the layered entry has to be
    /// omitted entirely rather than built from nothing.
    #[test]
    fn prefixed_sources_alone_produce_no_root_mount() {
        let mut rs = RootSources::new();
        assert!(rs.is_empty());
        rs.add("/Data", 0, inline("b.txt", b"DATA"));
        assert!(!rs.is_empty());

        let mounts = rs.mounts().unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].0, "/Data");
    }
}

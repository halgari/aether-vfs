//! Composition: how a root's mounts and write layer become the one provider
//! the director serves for it.

use std::collections::BTreeMap;
#[cfg(feature = "zip")]
use std::path::Path;
use std::sync::Arc;

use vfs_compose::MountGraph;
use vfs_provider::{bad_request, exists, map_io_err, Access, Provider, RootId};

use super::Session;

/// Build the single provider one root serves: its sibling mounts as a
/// [`MountGraph`], with the writable layer (if any) composed **over** the
/// whole graph as an [`vfs_compose::OverlayProvider`] upper.
///
/// The one place in the workspace that turns "these sources, that write
/// layer" into a provider. Every surface funnels through it —
/// [`Session::mount`], the daemon's `SessionRegistry`, and the config →
/// graph builder — because the two halves compose in a way neither
/// `MountGraph` nor `stack_layers` can express: an overlay upper is what
/// makes a write to content only a read-only source holds **copy up** rather
/// than fail. A surface that composes its own graph instead gets a session
/// that reads correctly and cannot be written to.
///
/// `ST_BAD_REQUEST` if the upper is not `Access::ReadWrite`, if any mount
/// declares `Access::SeqRead` (see [`reject_sequential`]), or if a mount prefix
/// does not normalize.
pub fn compose_root(
    mounts: Vec<(String, Arc<dyn Provider>)>,
    write_layer: Option<Arc<dyn Provider>>,
) -> Result<Arc<dyn Provider>, i32> {
    // The funnel's own copy of the gate. `Session::mount_at` and
    // `Session::set_root_mounts` both check before they record, so they never
    // reach here with one; this catches the **third** route, which does not go
    // through `Session` at all: `compose_root` is public and re-exported, and
    // both `vfs-directord`'s `SessionRegistry::compose` and `skyrim-live` call
    // it directly and hand the result to `Director::mount`.
    reject_sequential(mounts.iter().map(|(_, p)| p))?;
    let graph: Arc<dyn Provider> = Arc::new(MountGraph::new(mounts)?);
    match write_layer {
        Some(upper) => Ok(Arc::new(
            vfs_compose::OverlayProvider::from_arcs(graph, upper).map_err(|_| bad_request())?,
        )),
        None => Ok(graph),
    }
}

/// Refuse any provider declaring `Access::SeqRead`, with `ST_BAD_REQUEST`.
///
/// Spec §6's mount-time flag table calls an unwrapped `SeqRead` provider a
/// **hard error**, and it is one: the director's read path is
/// `read_at(handle, offset, buf)`, which a forward-only provider answers
/// `ST_NOT_SUPPORTED` to. Such a mount composes cleanly and serves `getattr` and
/// `readdir` correctly, then fails every actual read — inside an injected
/// process, where the symptom is a game that will not load and the cause is
/// nowhere near it. [`crate::SeekableProvider`] is what a caller wraps it in.
///
/// **One function because there is more than one way in, and the check has to
/// mean the same thing through all of them.** It previously lived inline in
/// `Session::mount_at` only, so `Session::set_root_mounts` accepted what
/// `mount_at` refused — and `set_root_mounts` is the path
/// `vfs-directord`'s `SessionRegistry::add_source` takes for *every* source, so
/// the daemon had no gate at all. Callers: [`Session::mount_at`],
/// [`Session::set_root_mounts`], and [`compose_root`].
fn reject_sequential<'a>(
    providers: impl IntoIterator<Item = &'a Arc<dyn Provider>>,
) -> Result<(), i32> {
    for p in providers {
        if p.capabilities().access == vfs_provider::Access::SeqRead {
            return Err(bad_request());
        }
    }
    Ok(())
}

/// Everything one root composes into, before it becomes the single provider
/// `Director` holds for that root.
///
/// The two halves are **not** interchangeable, and that distinction is the
/// whole point of this type: `mounts` are siblings (a `MountGraph` routes a
/// path to whichever of them owns it, later wins), while `write_layer` sits
/// *above* all of them as an overlay upper, which is what makes copy-on-write
/// possible — see [`Session::set_write_layer`].
#[derive(Default, Clone)]
pub(super) struct RootComposition {
    /// The staged launch directory, if this root has one — see
    /// [`Session::stage_launch`].
    ///
    /// **A separate slot, composed below `mounts`, and that is the whole
    /// point of it.** Staging is a point-in-time copy of what the graph
    /// already said, written out only because `CreateProcess` needs a real
    /// file; it must lose to curated content on every path both serve, and it
    /// must survive a host rebuilding `mounts` wholesale via
    /// [`Session::set_root_mounts`]. Keeping it out of `mounts` is what buys
    /// both: [`Session::recompose`] always puts it first in the `MountGraph`,
    /// and a `MountGraph` resolves by walking its mounts in **reverse**, so
    /// first means last-tried means lowest precedence.
    ///
    /// The daemon expressed the same rule as `STAGING_LAYER = i32::MIN` inside
    /// a `stack_layers` stack, where ascending layer order makes the first
    /// entry the bottom. The two orderings are opposite, which is exactly why
    /// this is a named slot rather than "just mount it and rely on ordering":
    /// relocating that code as a plain `mount_at` inverts it, and the symptom
    /// — a stale staged copy shadowing curated content — is a silent wrong
    /// answer, not a failure.
    pub(super) staging: Option<Arc<dyn Provider>>,
    /// Every `(prefix, provider)` accumulated for this root, in registration
    /// order (later wins on an overlapping path).
    mounts: Vec<(String, Arc<dyn Provider>)>,
    /// The writable upper this root's writes copy up into, if one is set.
    write_layer: Option<Arc<dyn Provider>>,
}

impl Session {
    /// Accumulates: later mounts override earlier for the same path, exactly
    /// as `Director`'s own mount list used to. Each call recomposes the full
    /// accumulated list into one `MountGraph` and replaces `RootId::DEFAULT`'s
    /// provider wholesale, since `Director` holds only one provider per root.
    ///
    /// Root 0's convenience form of [`Session::mount_at`].
    pub fn mount(&self, prefix: &str, backend: Arc<dyn Provider>) -> Result<(), i32> {
        self.mount_at(RootId::DEFAULT, prefix, backend)
    }

    /// [`Session::mount`] for a specific root. Appends one mount to `root`'s
    /// accumulated list and recomposes that root; every other root is
    /// untouched.
    ///
    /// **`ST_BAD_REQUEST` for a provider declaring `Access::SeqRead`** — see
    /// [`reject_sequential`], which is the same gate
    /// [`Session::set_root_mounts`] and [`compose_root`] apply.
    ///
    /// Checked here rather than in each host: the binding that has a friendly
    /// message for it is not the only surface that can reach `mount_at`.
    pub fn mount_at(
        &self,
        root: RootId,
        prefix: &str,
        backend: Arc<dyn Provider>,
    ) -> Result<(), i32> {
        reject_sequential([&backend])?;
        {
            let mut roots = self.roots.lock().map_err(|_| map_io_err())?;
            self.claim(&mut roots, root)?
                .mounts
                .push((prefix.to_string(), backend));
        }
        self.recompose(root)
    }

    /// Begin (or continue) composing `root`, refusing to take over a root
    /// something mounted on `Director` **directly**.
    ///
    /// A root this session has never composed, which the director already
    /// serves, belongs to a caller that built its own provider — the shape
    /// `Director::mount`'s doc describes, e.g. `skyrim-live`'s counter-wrapped
    /// root 1. Composing it here would rebuild it from this session's own
    /// (empty) inputs and replace that provider wholesale: the counters, the
    /// overlay, or both would vanish, silently, with reads still working
    /// against the wrong graph. `ST_EXISTS` says so instead; a caller that
    /// really means to take the root over unmounts it first.
    ///
    /// Roots this session already composes pass straight through — this is a
    /// check about *ownership*, not about re-composition.
    pub(super) fn claim<'m>(
        &self,
        roots: &'m mut BTreeMap<u32, RootComposition>,
        root: RootId,
    ) -> Result<&'m mut RootComposition, i32> {
        if !roots.contains_key(&root.0) && self.kernel.serves(root)? {
            return Err(exists());
        }
        Ok(roots.entry(root.0).or_default())
    }

    /// Replace `root`'s **entire** sibling-mount list, keeping its write
    /// layer, and recompose.
    ///
    /// For a host that keeps its own record of what a root serves and rebuilds
    /// the list from scratch whenever it changes — `SessionRegistry`, which
    /// re-derives a root's layer stack on every `add_source`. Such a host must
    /// not compose the result itself and hand it to `kernel().mount`: doing so
    /// replaces the root's provider with one that has no knowledge of the
    /// write layer, silently removing copy-on-write. Going through here keeps
    /// the two halves composed by the same code path [`Session::mount`] uses.
    ///
    /// **`ST_BAD_REQUEST` for any mount declaring `Access::SeqRead`**, the same
    /// gate [`Session::mount_at`] applies — see [`reject_sequential`] for why
    /// the two agreeing is not cosmetic.
    ///
    /// Validated **before** the list is recorded, for the reason
    /// [`Session::set_write_layer_at`] gives: a rejected call must leave the
    /// session exactly as it was. Recording first and letting
    /// [`Session::recompose`] refuse would park a list that cannot compose,
    /// making every later `mount_at` on this root fail too.
    pub fn set_root_mounts(&self, root: RootId, mounts: crate::RootMounts) -> Result<(), i32> {
        reject_sequential(mounts.iter().map(|(_, p)| p))?;
        {
            let mut roots = self.roots.lock().map_err(|_| map_io_err())?;
            self.claim(&mut roots, root)?.mounts = mounts;
        }
        self.recompose(root)
    }

    /// Declare the layer root 0's **writes** land in, composed as an
    /// [`vfs_compose::OverlayProvider`] upper over everything [`Session::mount`]
    /// has accumulated. Replaces any previously set write layer; takes effect
    /// immediately and is re-applied by every later `mount`.
    ///
    /// **This is what makes copy-on-write work, and mounting the same
    /// provider as an ordinary sibling layer does not.** A `MountGraph` (and
    /// `LayeredProvider` likewise) can only *route* a write to whichever
    /// mount is willing to take it; neither can seed the destination from a
    /// lower layer first. So with the writable directory mounted as a sibling
    /// above a read-only archive, an in-place edit of archive content — the
    /// `fopen(..., "r+b")` / `CreateFile(OPEN_EXISTING, GENERIC_WRITE)` that
    /// every mod tool does — finds no writable mount holding the file and
    /// fails, either `ST_READ_ONLY` (the archive owns the path) or
    /// `ST_NOT_FOUND` (nothing writable has it). Copy-on-write over read-only
    /// layered content is the core function of a mod-manager VFS, so the
    /// composition has to be an overlay, not a sibling.
    ///
    /// The upper must declare `Access::ReadWrite`; anything else is refused
    /// here (`ST_BAD_REQUEST`) rather than at the first write.
    ///
    /// Root 0's convenience form of [`Session::set_write_layer_at`].
    pub fn set_write_layer(&self, upper: Arc<dyn Provider>) -> Result<(), i32> {
        self.set_write_layer_at(RootId::DEFAULT, upper)
    }

    /// [`Session::set_write_layer`] for a specific root. Each root has its own
    /// write layer: a session may copy up game-directory writes into one
    /// location and a second root's writes into another, or give one root a
    /// write layer and leave the rest read-only.
    ///
    /// The upper is validated **before** it is recorded, so a rejected layer
    /// leaves the session exactly as it was rather than parking an unusable
    /// provider that would make every later `mount` on this root fail too.
    ///
    /// **Never wrap the upper in a read cache** — a caching wrapper such as
    /// `aether-storage`'s `Storage::cached`. A host is expected to put slow
    /// sources behind the cache and it is natural to do that uniformly, in one
    /// loop, over everything it mounts. The write layer is the one provider in
    /// the graph whose bytes change underneath the director: a cached read of a
    /// file that was just copied up would serve the pre-write content.
    /// (`Storage::cached` returns a mutable provider unchanged, so a writable
    /// upper is not cached in practice; a named layer from `Storage::layer` is
    /// the upper as it is.)
    pub fn set_write_layer_at(&self, root: RootId, upper: Arc<dyn Provider>) -> Result<(), i32> {
        if upper.capabilities().access != Access::ReadWrite {
            return Err(bad_request());
        }
        {
            let mut roots = self.roots.lock().map_err(|_| map_io_err())?;
            self.claim(&mut roots, root)?.write_layer = Some(upper);
        }
        self.recompose(root)
    }

    /// Rebuild `root`'s single provider from its accumulated mounts plus its
    /// optional write layer, and replace whatever `Director` currently serves
    /// for it — `Director` holds exactly one provider per root, so there is no
    /// incremental mount to append to.
    pub(super) fn recompose(&self, root: RootId) -> Result<(), i32> {
        let composition = self
            .roots
            .lock()
            .map_err(|_| map_io_err())?
            .get(&root.0)
            .cloned()
            .unwrap_or_default();
        // Staging **first**, and this line is the whole precedence guarantee:
        // a `MountGraph` resolves by walking its mounts in reverse, so the
        // first entry is the last one tried and therefore the one that only
        // answers for paths nothing else serves. Appending it instead — the
        // shape `mount_at` would produce — inverts that and lets a
        // point-in-time staged copy shadow curated content. See
        // [`RootComposition::staging`].
        let mut mounts: Vec<(String, Arc<dyn Provider>)> =
            Vec::with_capacity(composition.mounts.len() + 1);
        mounts.extend(composition.staging.map(|p| (String::new(), p)));
        mounts.extend(composition.mounts);
        let composed = compose_root(mounts, composition.write_layer)?;
        self.kernel.mount(root, composed)
    }

    /// Drop all of root 0's mounts before rebuilding composition. Its write
    /// layer, if any, is dropped with them — it is part of the same
    /// composition. **Other roots are untouched**, which is why this is
    /// spelled as root 0's form of [`Session::clear_root`] rather than left
    /// looking like it clears the session: a session is multi-root now, and
    /// "clear the mounts" would be a lie about the other roots.
    pub fn clear_mounts(&self) -> Result<(), i32> {
        self.clear_root(RootId::DEFAULT)
    }

    /// Forget everything this session composes for `root` — mounts and write
    /// layer together — and stop serving it. Also the way to hand a root back
    /// so something else can mount it directly (see [`Session::mount_at`]'s
    /// ownership check).
    ///
    /// **No production caller, and neither has [`Session::clear_mounts`].**
    /// The only non-test reference to either is `clear_mounts` delegating
    /// here. Kept deliberately, not overlooked: `Session` is this crate's
    /// public composition API, `mount_at`'s ownership check documents this as
    /// the way out of it, and "stop serving a root" is not something a host
    /// should have to reach past the API to do. Do not read the absence of
    /// callers as evidence the operation is unnecessary — read it as this
    /// project not yet having a host that tears a root down mid-session.
    pub fn clear_root(&self, root: RootId) -> Result<(), i32> {
        self.roots.lock().map_err(|_| map_io_err())?.remove(&root.0);
        self.kernel.unmount(root)
    }

    /// Every root this session composes, ascending — whether it got there
    /// through [`Session::mount_at`], [`Session::set_root_mounts`] or
    /// [`Session::set_write_layer_at`].
    ///
    /// The last of those is why this exists rather than callers keeping their
    /// own list: a host that records sources per root (as `SessionRegistry`
    /// does) has no entry for a root that was given *only* a write layer, so
    /// its own bookkeeping cannot enumerate what the session actually serves.
    pub fn composed_roots(&self) -> Vec<RootId> {
        self.roots
            .lock()
            .map(|roots| roots.keys().copied().map(RootId).collect())
            .unwrap_or_default()
    }

    /// Whether `root` has a write layer — i.e. whether a write to content
    /// only a read-only source holds can copy up, or must fail. The daemon
    /// reports this per root when a session is composed, since an absent
    /// write layer is otherwise invisible until the first in-place edit
    /// fails, inside a running game.
    pub fn has_write_layer(&self, root: RootId) -> bool {
        self.roots
            .lock()
            .map(|roots| roots.get(&root.0).is_some_and(|c| c.write_layer.is_some()))
            .unwrap_or(false)
    }

    /// Mount a Stored zip archive as a content backend (later mounts win on conflicts).
    ///
    /// Requires the `zip` feature (on by default).
    #[cfg(feature = "zip")]
    pub fn mount_zip(&self, zip_path: impl AsRef<Path>) -> Result<(), String> {
        let path = zip_path.as_ref();
        let be = vfs_zip::ZipProvider::open(path)
            .map_err(|e| format!("ZipProvider {}: {e:?}", path.display()))?;
        self.mount("", Arc::new(be))
            .map_err(|st| format!("mount zip status {st}"))
    }
}

/// Who owns a root's provider — the session that composes it, or a caller
/// that mounted one on `Director` directly.
///
/// `skyrim-live` mounts root 1 by hand because its counters must wrap the
/// composed provider, which `Session` has no hook for. That is legitimate,
/// and it leaves a hazard pointing the other way: any later `mount_at` /
/// `set_write_layer_at` on that root would recompose it from the session's
/// own empty inputs and drop the hand-mounted provider — counters, overlay
/// and all — while reads kept working against the wrong graph.
#[cfg(test)]
mod root_ownership_tests {
    use super::*;
    use std::path::PathBuf;
    use vfs_compose::DiskProvider;
    use vfs_provider::ST_EXISTS;

    fn dir(tag: &str, file: &str) -> PathBuf {
        let p = crate::test_scratch::scratch_created(&format!("own-{tag}"));
        std::fs::write(p.join(file), file.as_bytes()).unwrap();
        p
    }

    #[test]
    fn a_hand_mounted_root_is_not_recomposed_away() {
        let hand = dir("hand", "hand.txt");
        let session_layer = dir("sess", "session.txt");

        let s = Session::new();
        s.kernel()
            .mount(RootId(1), Arc::new(DiskProvider::new(&hand)))
            .unwrap();

        // Both mutators must refuse: each one alone would replace root 1's
        // provider with a composition built from nothing.
        assert_eq!(
            s.mount_at(RootId(1), "", Arc::new(DiskProvider::new(&session_layer)))
                .expect_err("composing a hand-mounted root must be refused, not performed"),
            ST_EXISTS
        );
        assert_eq!(
            s.set_write_layer_at(RootId(1), Arc::new(DiskProvider::new(&session_layer)))
                .expect_err("a write layer on a hand-mounted root must be refused too"),
            ST_EXISTS
        );

        // The hand-mounted provider still serves, and the refused mount never
        // took effect — the refusal is not a half-applied change.
        assert!(
            s.kernel().getattr(RootId(1), "hand.txt").unwrap().is_some(),
            "the hand-mounted provider must still be serving root 1"
        );
        assert!(
            s.kernel()
                .getattr(RootId(1), "session.txt")
                .unwrap()
                .is_none(),
            "the refused mount must not be serving anything"
        );

        // Root 0 is unaffected: this is per-root ownership, not a session-wide
        // freeze.
        s.mount("", Arc::new(DiskProvider::new(&session_layer)))
            .unwrap();
        assert!(s
            .kernel()
            .getattr(RootId::DEFAULT, "session.txt")
            .unwrap()
            .is_some());

        // And the root can be handed over deliberately.
        s.clear_root(RootId(1)).unwrap();
        s.mount_at(RootId(1), "", Arc::new(DiskProvider::new(&session_layer)))
            .expect("an unmounted root may be taken over");
        assert!(s
            .kernel()
            .getattr(RootId(1), "session.txt")
            .unwrap()
            .is_some());
        assert!(
            s.kernel()
                .getattr(RootId(1), "hand.txt")
                .unwrap_or(None)
                .is_none(),
            "after the handover the hand-mounted provider is gone, as asked for"
        );
    }

    /// A root given only a write layer is still a root this session composes.
    ///
    /// The daemon enumerates roots through this to report whether each can
    /// copy up. Its own per-root bookkeeping is filled in by `add_source`
    /// alone, so a root declared with a write layer and no ordinary source
    /// was missing from that report entirely — silently absent from the one
    /// place that says whether writes copy up.
    #[test]
    fn composed_roots_includes_a_root_that_has_only_a_write_layer() {
        let upper = dir("only-upper", "upper.txt");
        let content = dir("with-source", "content.txt");

        let s = Session::new();
        s.mount_at(RootId(1), "", Arc::new(DiskProvider::new(&content)))
            .unwrap();
        s.set_write_layer_at(RootId(2), Arc::new(DiskProvider::new(&upper)))
            .unwrap();

        assert_eq!(
            s.composed_roots(),
            vec![RootId(1), RootId(2)],
            "a write-layer-only root must be enumerated too, ascending"
        );
        assert!(!s.has_write_layer(RootId(1)));
        assert!(s.has_write_layer(RootId(2)));
        // Root 0 was never touched, so it is not composed and must not appear.
        assert!(!s.composed_roots().contains(&RootId::DEFAULT));
    }

    /// The check is about ownership, not about recomposition: a root the
    /// session already composes keeps composing, however many times.
    #[test]
    fn a_session_composed_root_recomposes_as_often_as_asked() {
        let first = dir("first", "first.txt");
        let second = dir("second", "second.txt");

        let s = Session::new();
        s.mount_at(RootId(2), "", Arc::new(DiskProvider::new(&first)))
            .unwrap();
        s.mount_at(RootId(2), "", Arc::new(DiskProvider::new(&second)))
            .unwrap();
        s.set_write_layer_at(RootId(2), Arc::new(DiskProvider::new(&second)))
            .unwrap();
        s.set_root_mounts(
            RootId(2),
            vec![(String::new(), Arc::new(DiskProvider::new(&first)))],
        )
        .unwrap();

        assert!(s
            .kernel()
            .getattr(RootId(2), "first.txt")
            .unwrap()
            .is_some());
        assert!(
            s.has_write_layer(RootId(2)),
            "the write layer must survive a later set_root_mounts"
        );
        assert!(
            s.kernel()
                .open(RootId(2), "first.txt", vfs_provider::OPEN_WRITE)
                .is_ok(),
            "with a write layer, an in-place edit of the read side must copy up"
        );

        // Nothing leaked into root 0, which this session never composed.
        assert!(
            s.kernel()
                .getattr(RootId::DEFAULT, "first.txt")
                .unwrap()
                .is_none(),
            "an uncomposed root must answer for nothing, not for another root's content"
        );
    }
}

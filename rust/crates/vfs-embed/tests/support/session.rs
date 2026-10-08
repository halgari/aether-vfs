//! A composed, served session, built the way a host that learns its sources
//! one at a time builds one — the shape the removed daemon's
//! `SessionRegistry` gave every session these tests used to drive over gRPC.

use std::collections::BTreeMap;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;

use vfs_embed::{Provider, RootId, RootSources, Session};

/// A served [`Session`] with its own scratch directories, and the per-root
/// source lists its roots are composed from.
///
/// Derefs to the session. Held in an `Arc` so a launch can run on a thread of
/// its own (see `super::launch::launch_bounded`); the methods that need the
/// session mutably ([`LiveSession::declare_root`]) are for setup, before any
/// launch.
pub struct LiveSession {
    // Declared before `_dirs`, so the session (its ring, its staged launch
    // directory) is dropped before the directories it lives in are removed.
    session: Arc<Session>,
    roots: BTreeMap<u32, RootSources>,
    _dirs: vfs_testkit::Scratch,
}

impl LiveSession {
    /// A new session named `name`, serving. Its root, overlay and state
    /// directories (`<base>/root`, `<base>/overlay`, `<base>/state`) are
    /// fresh and empty: a session that inherited a previous run's
    /// `overlay/root-0` would fail the "the overlay is empty afterwards"
    /// check the write-path tests use to detect a write that bypassed the
    /// director.
    pub fn create(name: &str) -> Self {
        let dirs = vfs_testkit::scratch_dir(&format!("vfs-session-{name}"));
        let mut session = Session::new();
        session.set_root(dirs.join("root"));
        session.set_overlay(dirs.join("overlay"));
        session.set_state_dir(dirs.join("state"));
        session
            .serve()
            .unwrap_or_else(|e| panic!("serve session {name}: {e}"));
        LiveSession {
            session: Arc::new(session),
            roots: BTreeMap::new(),
            _dirs: dirs,
        }
    }

    /// The session, for a launch on another thread.
    pub fn shared(&self) -> &Arc<Session> {
        &self.session
    }

    /// Add one source to `root` at `mount` (`"/"` is the whole root; sources
    /// there layer in ascending `layer` order), and recompose that root.
    /// Sources are siblings: copy-on-write over them is
    /// [`LiveSession::set_write_layer`].
    pub fn add_source(
        &mut self,
        root: u32,
        mount: &str,
        layer: i32,
        provider: Arc<dyn Provider>,
    ) -> Result<(), String> {
        let sources = self.roots.entry(root).or_default();
        sources.add(mount, layer, provider);
        let mounts = sources.mounts()?;
        self.session
            .set_root_mounts(RootId(root), mounts)
            .map_err(|st| format!("mount root {root} status {st}"))
    }

    /// Declare the layer `root`'s writes land in.
    pub fn set_write_layer(&self, root: u32, upper: Arc<dyn Provider>) -> Result<(), String> {
        self.session
            .set_write_layer_at(RootId(root), upper)
            .map_err(|st| format!("set write layer for root {root}: status {st}"))
    }

    /// Declare where root `id` is, as `Session::declare_root`. Setup only:
    /// panics if a launch still holds the session.
    pub fn declare_root(&mut self, id: u32, path: impl Into<PathBuf>) {
        Arc::get_mut(&mut self.session)
            .expect("declare roots before launching")
            .declare_root(id, path);
    }

    /// Root 0's location, as the launched program sees it: the declared one,
    /// else the default — the session's own root directory on Windows, a
    /// `C:\…` path inside the Wine prefix on unix.
    pub fn root(&self) -> PathBuf {
        self.session
            .root_locations()
            .into_iter()
            .find(|r| r.id == 0)
            .map(|r| PathBuf::from(r.location))
            .expect("root 0 always has a location")
    }
}

impl Deref for LiveSession {
    type Target = Session;
    fn deref(&self) -> &Session {
        &self.session
    }
}

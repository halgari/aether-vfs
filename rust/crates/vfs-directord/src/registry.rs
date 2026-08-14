//! Live session registry: id → host [`Session`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use vfs_cache::{BlockCache, CacheConfig, CachingProvider};
use vfs_compose::stack_layers;
use vfs_director::stage::{ImageSource, StagedDir};
use vfs_director::{DiskProvider, LaunchOpts, Provider, Session};

/// Layer for the disk provider mounted over a staged launch directory (see
/// [`SessionRegistry::stage_launch`]).
///
/// Staging exists only so `CreateProcess`/the loader can find a real image
/// before the shim does; the bytes it writes are a point-in-time copy of
/// whatever the provider graph already said. Mounting it at the lowest
/// possible layer means a real, curated content or mod layer always wins on
/// a shared path — the staged copy only answers for paths nothing else
/// serves (e.g. a launcher's runtime DLL, or an import-closure DLL pulled
/// from a fallback dir rather than the VFS).
const STAGING_LAYER: i32 = i32::MIN;

/// Arguments for [`SessionRegistry::stage_launch`], bundled to stay under
/// clippy's argument-count limit — see [`vfs_director::stage::stage_launch_with`]
/// for what each field means.
pub struct StageLaunchOpts<'a> {
    pub exe_vpath: &'a str,
    pub also: &'a [&'a str],
    pub stage_root: &'a Path,
    pub tag: &'a str,
    pub fallback_dirs: &'a [PathBuf],
}

/// One live host session plus metadata returned on ListSessions.
pub struct LiveSession {
    pub id: String,
    pub name: String,
    pub root: PathBuf,
    pub session: Session,
    next_source_id: AtomicU64,
    /// Mounted backends bottom→top for rebuild (same mount "/" composition).
    layers: Vec<(i32, Arc<dyn Provider>)>,
    /// Sources with non-root mount prefixes (director path mounts).
    prefix_mounts: Vec<(String, Arc<dyn Provider>)>,
}

impl LiveSession {
    pub fn next_source_id(&self) -> u64 {
        self.next_source_id.fetch_add(1, Ordering::Relaxed)
    }
}

/// Process-wide sequence for session base-directory naming — see the comment
/// in [`SessionRegistry::create`] for why this must be independent of any one
/// registry's own session-id counter.
static SESSION_BASE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Process-wide multi-session table owned by the daemon.
#[derive(Clone)]
pub struct SessionRegistry {
    inner: Arc<Mutex<HashMap<String, LiveSession>>>,
    next_id: Arc<AtomicU64>,
    cache: Arc<BlockCache>,
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::with_cache(Arc::new(BlockCache::new(CacheConfig::default())))
    }
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_cache(cache: Arc<BlockCache>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(0)),
            cache,
        }
    }

    pub fn cache(&self) -> &Arc<BlockCache> {
        &self.cache
    }

    /// Number of live sessions.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn len(&self) -> usize {
        self.inner.lock().map(|g| g.len()).unwrap_or(0)
    }

    pub fn create(&self, name: String) -> Result<SessionSummary, String> {
        let id = format!("s{}", self.next_id.fetch_add(1, Ordering::Relaxed) + 1);
        // `base_seq` is process-wide, deliberately independent of `id`/`next_id`
        // (which are per-registry): two `SessionRegistry`s in the same process —
        // e.g. two `#[tokio::test]`s in one test binary — each start `next_id`
        // at 1, so `id` alone repeats ("s1") across registries. Keying the base
        // directory on `id` alone let a second session's root/overlay collide
        // with the first's, physically, at the same path — cross-contaminating
        // any test that actually reads/writes bytes through a mounted
        // `DiskProvider` rather than only exercising RPC bookkeeping.
        let base_seq = SESSION_BASE_SEQ.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir()
            .join(format!("vfs-daemon-{}-{base_seq}-{id}", std::process::id()));
        let root = base.join("root");
        let overlay = base.join("overlay");
        let state = base.join("state");

        let mut session = Session::new();
        session.set_root(&root);
        session.set_overlay(&overlay);
        session.set_state_dir(&state);
        session.serve()?;

        let summary = SessionSummary {
            id: id.clone(),
            name: name.clone(),
            root: root.clone(),
        };

        let live = LiveSession {
            id: id.clone(),
            name,
            root,
            session,
            next_source_id: AtomicU64::new(1),
            layers: Vec::new(),
            prefix_mounts: Vec::new(),
        };

        self.inner
            .lock()
            .map_err(|_| "session registry poisoned".to_string())?
            .insert(id, live);
        Ok(summary)
    }

    pub fn add_source(
        &self,
        session_id: &str,
        mount: &str,
        layer: i32,
        backend: Arc<dyn Provider>,
    ) -> Result<u64, String> {
        let source_id = {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| "session registry poisoned".to_string())?;
            let live = guard
                .get_mut(session_id)
                .ok_or_else(|| format!("unknown session {session_id}"))?;
            let id = live.next_source_id();
            // Wrap with process-wide block cache.
            let cached: Arc<dyn Provider> =
                Arc::new(CachingProvider::new(backend, Arc::clone(&self.cache), id));
            let mount_norm = mount.trim();
            let is_root = mount_norm.is_empty() || mount_norm == "/" || mount_norm == "\\";
            if is_root {
                live.layers.push((layer, cached));
                // Stable order for equal layers: preserve insertion order.
                live.layers.sort_by_key(|a| a.0);
            } else {
                live.prefix_mounts.push((mount.to_string(), cached));
            }
            // Rebuild mounts from the recorded source list (layered root + prefixes).
            live.session
                .clear_mounts()
                .map_err(|st| format!("clear_mounts status {st}"))?;
            let stack: Vec<Arc<dyn Provider>> =
                live.layers.iter().map(|(_, b)| Arc::clone(b)).collect();
            if !stack.is_empty() {
                let composed = stack_layers(stack).map_err(|e| e.to_string())?;
                live.session
                    .mount("", composed)
                    .map_err(|st| format!("mount status {st}"))?;
            }
            for (pfx, be) in &live.prefix_mounts {
                live.session
                    .mount(pfx, Arc::clone(be))
                    .map_err(|st| format!("mount {pfx} status {st}"))?;
            }
            id
        };
        Ok(source_id)
    }

    /// Stage a launch image (plus import closure / companion images) onto
    /// real disk, then mount the staging directory into the session's own
    /// provider graph so every staged path resolves there too — not only
    /// via disk passthrough.
    ///
    /// `session.launch()` still needs `staged.exe()`'s literal on-disk path
    /// for `CreateProcess` (Windows cannot create a process from bytes), so
    /// staging to disk stays mandatory. What changes here is that the *same*
    /// files also become resolvable through `getattr`/`open` at their
    /// under-root vpath (e.g. `SkyrimSE.exe`), which is what a later,
    /// hook-mediated open of that same path — a launcher spawning its target
    /// by relative name beneath the managed root, say — needs once the root
    /// is fully virtual and disk passthrough is gone.
    ///
    /// Mounted at [`STAGING_LAYER`] (via [`Self::add_source`]) so real game
    /// content always wins over the staged copy on a shared path.
    pub fn stage_launch(
        &self,
        session_id: &str,
        source: &dyn ImageSource,
        opts: &StageLaunchOpts,
    ) -> Result<StagedDir, String> {
        let staged = vfs_director::stage::stage_launch_with(
            source,
            opts.exe_vpath,
            opts.also,
            opts.stage_root,
            opts.tag,
            opts.fallback_dirs,
        )?;
        let disk: Arc<dyn Provider> = Arc::new(DiskProvider::new(staged.dir()));
        self.add_source(session_id, "/", STAGING_LAYER, disk)?;
        Ok(staged)
    }

    pub fn with_session_mut<R>(
        &self,
        id: &str,
        f: impl FnOnce(&mut LiveSession) -> Result<R, String>,
    ) -> Result<R, String> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "session registry poisoned".to_string())?;
        let live = guard
            .get_mut(id)
            .ok_or_else(|| format!("unknown session {id}"))?;
        f(live)
    }

    pub fn list(&self) -> Result<Vec<SessionSummary>, String> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| "session registry poisoned".to_string())?;
        Ok(guard
            .values()
            .map(|s| SessionSummary {
                id: s.id.clone(),
                name: s.name.clone(),
                root: s.root.clone(),
            })
            .collect())
    }

    pub fn teardown(&self, id: &str) -> Result<(), String> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "session registry poisoned".to_string())?;
        let mut live = guard
            .remove(id)
            .ok_or_else(|| format!("unknown session {id}"))?;
        live.session.stop_serve();
        Ok(())
    }

    pub fn launch(&self, id: &str, opts: LaunchOpts) -> Result<i32, String> {
        self.with_session_mut(id, |live| live.session.launch(&opts))
    }
}

#[derive(Clone, Debug)]
pub struct SessionSummary {
    pub id: String,
    pub name: String,
    pub root: PathBuf,
}



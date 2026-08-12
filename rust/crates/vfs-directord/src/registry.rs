//! Live session registry: id → host [`Session`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use vfs_cache::{BlockCache, CacheConfig, CachingBackend};
use vfs_compose::stack_layers;
use vfs_director::{Backend, LaunchOpts, Session};

/// One live host session plus metadata returned on ListSessions.
pub struct LiveSession {
    pub id: String,
    pub name: String,
    pub root: PathBuf,
    pub session: Session,
    next_source_id: AtomicU64,
    /// Mounted backends bottom→top for rebuild (same mount "/" composition).
    layers: Vec<(i32, Arc<dyn Backend>)>,
    /// Sources with non-root mount prefixes (director path mounts).
    prefix_mounts: Vec<(String, Arc<dyn Backend>)>,
}

impl LiveSession {
    pub fn next_source_id(&self) -> u64 {
        self.next_source_id.fetch_add(1, Ordering::Relaxed)
    }
}

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

    pub fn len(&self) -> usize {
        self.inner.lock().map(|g| g.len()).unwrap_or(0)
    }

    pub fn create(&self, name: String) -> Result<SessionSummary, String> {
        let id = format!("s{}", self.next_id.fetch_add(1, Ordering::Relaxed) + 1);
        let base = std::env::temp_dir().join(format!("vfs-daemon-{}-{id}", std::process::id()));
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
        backend: Arc<dyn Backend>,
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
            let cached: Arc<dyn Backend> =
                Arc::new(CachingBackend::new(backend, Arc::clone(&self.cache), id));
            let mount_norm = mount.trim();
            let is_root = mount_norm.is_empty() || mount_norm == "/" || mount_norm == "\\";
            if is_root {
                live.layers.push((layer, cached));
                // Stable order for equal layers: preserve insertion order.
                live.layers.sort_by(|a, b| a.0.cmp(&b.0));
            } else {
                live.prefix_mounts.push((mount.to_string(), cached));
            }
            // Rebuild mounts from the recorded source list (layered root + prefixes).
            live.session
                .clear_mounts()
                .map_err(|st| format!("clear_mounts status {st}"))?;
            let stack: Vec<Arc<dyn Backend>> =
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



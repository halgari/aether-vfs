//! Live session registry: id → host [`Session`].

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use vfs_cache::{BlockCache, CacheConfig, CachingProvider};
use vfs_compose::stack_layers;
use vfs_director::stage::{ImageSource, StagedDir};
use vfs_director::{DiskProvider, LaunchOpts, Provider, Session};
use vfs_protocol::RootId;

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

/// Build the composed provider each root in a [`vfs_control::SessionConfig`]
/// serves — the config → provider-graph half of stage 2b's "one provider per
/// root" (design spec §6).
///
/// A source with no explicit `root` defaults to root `0`; sources sharing a
/// root are combined with [`stack_layers`] in declaration order (later wins),
/// exactly the documented flat-`[[source]]`-list sugar — generalized here to
/// however many roots the config declares rather than assuming there is only
/// one.
///
/// Validates the config first (see [`vfs_control::SessionConfig::validate_roots`]):
/// a duplicate `[[root]]` id, or a source naming an undeclared root, is
/// rejected here rather than silently producing a provider keyed by a number
/// nothing documents.
///
/// This builds provider objects and does **not** mount anything into a live
/// [`Session`]/`Director` — that is [`SessionRegistry::add_source`]'s job,
/// called once per source over the RPC path (`apply_session_config`). The
/// providers this function returns are used directly by its own tests,
/// addressed via [`vfs_protocol::VPath`], not through a session's ring/IPC
/// path.
pub fn build_provider_graph(
    cfg: &vfs_control::SessionConfig,
) -> Result<BTreeMap<RootId, Arc<dyn Provider>>, String> {
    cfg.validate_roots()?;
    let mut by_root: BTreeMap<u32, Vec<Arc<dyn Provider>>> = BTreeMap::new();
    for entry in &cfg.sources {
        let backend = vfs_source::build_provider(&entry.spec).map_err(|e| e.to_string())?;
        by_root.entry(entry.root).or_default().push(backend);
    }
    let mut graph = BTreeMap::new();
    for (root, stack) in by_root {
        let composed = stack_layers(stack).map_err(|e| e.to_string())?;
        graph.insert(RootId(root), composed);
    }
    Ok(graph)
}

/// Everything recorded for one declared root, so its composed provider can
/// be rebuilt from scratch whenever a new source targeting it arrives.
#[derive(Default)]
struct RootBuild {
    /// Root-mounted ("/") sources, bottom→top for rebuild via `stack_layers`.
    layers: Vec<(i32, Arc<dyn Provider>)>,
    /// Sources with a non-root mount prefix within this root (director path
    /// mounts), composed alongside the layered root via `MountGraph`.
    prefix_mounts: Vec<(String, Arc<dyn Provider>)>,
}

/// One live host session plus metadata returned on ListSessions.
pub struct LiveSession {
    pub id: String,
    pub name: String,
    pub root: PathBuf,
    pub session: Session,
    next_source_id: AtomicU64,
    next_stage_tag: AtomicU64,
    /// Per-declared-root bookkeeping for rebuild. Keyed by the raw `u32` a
    /// `SourceEntry`/`AddSourceReq` names — `RootId` wraps this only at the
    /// `Director` boundary.
    roots: HashMap<u32, RootBuild>,
    /// The most recent launch's staged directory, kept alive here so its
    /// `Drop` (directory removal) does not race the child it was staged for.
    /// Dropped (and thus cleaned up) when the session is torn down, or
    /// replaced on the next relaunch. See [`SessionRegistry::launch`].
    staged: Option<StagedDir>,
}

impl LiveSession {
    pub fn next_source_id(&self) -> u64 {
        self.next_source_id.fetch_add(1, Ordering::Relaxed)
    }

    fn next_stage_tag(&self) -> u64 {
        self.next_stage_tag.fetch_add(1, Ordering::Relaxed)
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
            next_stage_tag: AtomicU64::new(1),
            roots: HashMap::new(),
            staged: None,
        };

        self.inner
            .lock()
            .map_err(|_| "session registry poisoned".to_string())?
            .insert(id, live);
        Ok(summary)
    }

    /// Add one source to `session_id`, targeting `root` (`0` for every
    /// caller that predates stage 2b — the CLI and every existing config).
    /// Rebuilds only `root`'s composed provider and re-mounts it at
    /// `RootId(root)` in the live `Director`; other roots are untouched.
    pub fn add_source(
        &self,
        session_id: &str,
        root: u32,
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
            let build = live.roots.entry(root).or_default();
            if is_root {
                build.layers.push((layer, cached));
                // Stable order for equal layers: preserve insertion order.
                build.layers.sort_by_key(|a| a.0);
            } else {
                build.prefix_mounts.push((mount.to_string(), cached));
            }
            // Rebuild this root's composed provider from its recorded source
            // list (layered root sources + non-root prefix mounts), then
            // replace whatever `Director` currently serves for this root
            // wholesale — `Director` holds exactly one provider per root, so
            // there is no incremental mount to append to.
            let mut mounts: Vec<(String, Arc<dyn Provider>)> = Vec::new();
            if !build.layers.is_empty() {
                let stack: Vec<Arc<dyn Provider>> =
                    build.layers.iter().map(|(_, b)| Arc::clone(b)).collect();
                let composed = stack_layers(stack).map_err(|e| e.to_string())?;
                mounts.push((String::new(), composed));
            }
            for (pfx, be) in &build.prefix_mounts {
                mounts.push((pfx.clone(), Arc::clone(be)));
            }
            let graph = vfs_director::MountGraph::new(mounts)
                .map_err(|st| format!("build provider graph for root {root}: status {st}"))?;
            live.session
                .kernel()
                .mount(RootId(root), Arc::new(graph))
                .map_err(|st| format!("mount root {root} status {st}"))?;
            id
        };
        Ok(source_id)
    }

    /// Declare the host directory a non-zero root virtualizes, so the
    /// injected shim recognises paths under it as belonging to `root` rather
    /// than to no one.
    ///
    /// The companion to [`Self::add_source`], and deliberately not folded
    /// into it: `add_source` says *what a root serves*, this says *where the
    /// game will look for it*. A config's `[[root]] path` is the source of
    /// truth for this; `add_source` never sees it, because `AddSourceReq`
    /// carries a root id and no path.
    ///
    /// Root 0 is the session's own root (`SessionSummary::root`) and is
    /// rejected here rather than silently ignored — repointing it would
    /// desynchronise the shim from the directory the daemon actually created.
    pub fn declare_root(&self, session_id: &str, root: u32, path: &Path) -> Result<(), String> {
        if root == 0 {
            return Err("root 0 is the session's own root and cannot be re-declared".to_string());
        }
        self.with_session_mut(session_id, |live| {
            live.session.declare_root(root, path);
            Ok(())
        })
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
        // Staging always concerns the launched image — root 0 (the game
        // directory) in every session this registry builds today.
        self.add_source(session_id, 0, "/", STAGING_LAYER, disk)?;
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

    /// The production launch entrypoint (`DirectorService::launch` → here,
    /// the same path `vfs launch --exec` and scenario-TOML `[launch] exec =`
    /// drive). `opts.image` here names a VFS vpath, not an already-staged
    /// disk path — same as any other content path a client asks the director
    /// about. Stage it (and mount the staging directory into the provider
    /// graph) before handing the resulting real, on-disk path to
    /// `Session::launch`, so the same file is answerable through
    /// `getattr`/`open` afterward too, not only reachable via the literal
    /// path `CreateProcess` used.
    ///
    /// An absolute `opts.image` — an already-staged path (as
    /// `skyrim-live.rs` builds and passes directly to `Session::launch`,
    /// bypassing the registry), or a test-fixture binary that was never VFS
    /// content — is left untouched, mirroring `Session::launch`'s own
    /// absolute/relative split.
    pub fn launch(&self, id: &str, opts: LaunchOpts) -> Result<i32, String> {
        let opts = self.stage_relative_launch_image(id, opts)?;
        self.with_session_mut(id, |live| live.session.launch(&opts))
    }

    /// See [`Self::launch`]. Split out so the staging step (fallible, does
    /// file IO, and briefly re-enters the registry lock via
    /// [`Self::stage_launch`]) stays separate from the single
    /// `with_session_mut` call that performs the actual launch.
    fn stage_relative_launch_image(
        &self,
        id: &str,
        opts: LaunchOpts,
    ) -> Result<LaunchOpts, String> {
        if Path::new(&opts.image).is_absolute() {
            return Ok(opts);
        }

        /// Reads whole files out of a session's own composed provider graph,
        /// for [`vfs_director::stage`]. Mirrors `skyrim-live.rs`'s
        /// `KernelSource` — kept private here since `stage.rs` deliberately
        /// stays independent of how content is served.
        struct KernelSource(Arc<vfs_director::Director>);
        impl ImageSource for KernelSource {
            fn read(&self, vpath: &str) -> Option<Vec<u8>> {
                // Staging always concerns the launched image, which lives in
                // the game-directory root — root 0 in every session this
                // registry builds today.
                let (fh, size, is_dir) =
                    self.0.open(RootId::DEFAULT, vpath, vfs_director::OPEN_READ).ok()?;
                if is_dir {
                    let _ = self.0.close(fh);
                    return None;
                }
                let mut buf = vec![0u8; size as usize];
                let mut off = 0usize;
                while off < buf.len() {
                    match self.0.read(fh, off as u64, &mut buf[off..]) {
                        Ok(0) => break,
                        Ok(n) => off += n,
                        Err(_) => {
                            let _ = self.0.close(fh);
                            return None;
                        }
                    }
                }
                let _ = self.0.close(fh);
                buf.truncate(off);
                Some(buf)
            }
        }

        let (kernel, stage_root, tag) = self.with_session_mut(id, |live| {
            Ok((
                Arc::clone(live.session.kernel()),
                live.session.state_dir().join("stage"),
                live.next_stage_tag(),
            ))
        })?;

        let source = KernelSource(kernel);
        let staged = self.stage_launch(
            id,
            &source,
            &StageLaunchOpts {
                exe_vpath: &opts.image,
                // Generic registry launches carry no game-specific knowledge
                // of launcher/spawn-target chains or redistributable fallback
                // dirs (that lives in game-specific tooling, e.g.
                // `skyrim-live.rs`); a caller that needs either can call
                // `stage_launch` directly before `launch`.
                also: &[],
                stage_root: &stage_root,
                tag: &tag.to_string(),
                fallback_dirs: &[],
            },
        )?;

        let mut opts = opts;
        opts.image = staged.exe().to_string_lossy().into_owned();
        self.with_session_mut(id, |live| {
            live.staged = Some(staged);
            Ok(())
        })?;
        Ok(opts)
    }
}

#[derive(Clone, Debug)]
pub struct SessionSummary {
    pub id: String,
    pub name: String,
    pub root: PathBuf,
}

#[cfg(test)]
mod root_graph_tests {
    use super::*;
    use vfs_control::{SessionConfig, SourceEntry, SourceSpec};
    use vfs_director::OPEN_READ;
    use vfs_protocol::VPath;

    fn read_whole(p: &Arc<dyn Provider>, root: RootId, rel: &str) -> Vec<u8> {
        let (h, size, is_dir) = p.open(VPath::new(root, rel), OPEN_READ).unwrap();
        assert!(!is_dir);
        let mut buf = vec![0u8; size as usize];
        let mut off = 0usize;
        while off < buf.len() {
            let n = p.read_at(h, off as u64, &mut buf[off..]).unwrap();
            if n == 0 {
                break;
            }
            off += n;
        }
        p.close(h).unwrap();
        buf
    }

    /// Stage 2b task 2, step 1: a config declaring two roots with one
    /// provider each parses, and the resulting graph resolves the same
    /// relative path to different bytes under each root.
    #[test]
    fn two_roots_with_one_provider_each_resolve_independently() {
        let game_dir = tempfile::tempdir().unwrap();
        let docs_dir = tempfile::tempdir().unwrap();
        std::fs::write(game_dir.path().join("same.txt"), b"GAME-BYTES").unwrap();
        std::fs::write(docs_dir.path().join("same.txt"), b"DOCS-BYTES").unwrap();

        let toml = format!(
            r#"
[[root]]
id   = 0
name = "game"
path = {}

[[root]]
id   = 1
name = "docs"
path = {}

[[source]]
type = "disk"
path = {}
root = 0

[[source]]
type = "disk"
path = {}
root = 1
"#,
            toml_quote(&game_dir.path().to_string_lossy()),
            toml_quote(&docs_dir.path().to_string_lossy()),
            toml_quote(&game_dir.path().to_string_lossy()),
            toml_quote(&docs_dir.path().to_string_lossy()),
        );
        let cfg: SessionConfig = toml::from_str(&toml).expect("parse two-root config");
        assert_eq!(cfg.roots.len(), 2);

        let graph = build_provider_graph(&cfg).expect("build provider graph");
        assert_eq!(graph.len(), 2, "one provider per declared root");

        let game = graph.get(&RootId(0)).expect("root 0 provider");
        let docs = graph.get(&RootId(1)).expect("root 1 provider");
        assert_eq!(read_whole(game, RootId(0), "same.txt"), b"GAME-BYTES");
        assert_eq!(read_whole(docs, RootId(1), "same.txt"), b"DOCS-BYTES");
    }

    /// The flat `[[source]]` sugar (no `[[root]]` table, no `root` on any
    /// source) must still desugar to "layered of these, mounted at root 0"
    /// — the single-root behaviour every existing config relies on.
    #[test]
    fn flat_source_list_sugar_desugars_to_layered_root_zero() {
        let base = tempfile::tempdir().unwrap();
        let mod_dir = tempfile::tempdir().unwrap();
        std::fs::write(base.path().join("shared.txt"), b"BASE").unwrap();
        std::fs::write(mod_dir.path().join("shared.txt"), b"MOD-WINS").unwrap();

        let cfg = SessionConfig {
            sources: vec![
                SourceEntry {
                    spec: SourceSpec::Disk {
                        path: base.path().to_string_lossy().into_owned(),
                    },
                    mount: "/".into(),
                    root: 0,
                },
                SourceEntry {
                    spec: SourceSpec::Disk {
                        path: mod_dir.path().to_string_lossy().into_owned(),
                    },
                    mount: "/".into(),
                    root: 0,
                },
            ],
            ..Default::default()
        };

        let graph = build_provider_graph(&cfg).expect("build provider graph");
        assert_eq!(graph.len(), 1, "the flat list is a single root");
        let root0 = graph.get(&RootId(0)).unwrap();
        assert_eq!(
            read_whole(root0, RootId(0), "shared.txt"),
            b"MOD-WINS",
            "later declaration order wins, same as the old default-layer ordering"
        );
    }

    /// Task 3 review, Finding 2: every other multi-root test here (including
    /// `two_roots_with_one_provider_each_resolve_independently` above) goes
    /// through `build_provider_graph`, a pure function that never touches
    /// `Director` — it does not prove the *live* path the whole stage rests
    /// on. This one goes through `SessionRegistry::add_source` (the
    /// gRPC-backed path `apply_session_config`/`AddSourceReq.root` actually
    /// drives) into the live `Session`'s `Director`, and reads back through
    /// `Director::open`/`RootId`, not the graph builder.
    #[test]
    fn two_roots_resolve_independently_through_the_live_director() {
        let game_dir = tempfile::tempdir().unwrap();
        let docs_dir = tempfile::tempdir().unwrap();
        std::fs::write(game_dir.path().join("same.txt"), b"GAME-BYTES").unwrap();
        std::fs::write(docs_dir.path().join("same.txt"), b"DOCS-BYTES").unwrap();

        let reg = SessionRegistry::new();
        let summary = reg.create("two-root-live".into()).unwrap();
        reg.add_source(&summary.id, 0, "/", 0, Arc::new(DiskProvider::new(game_dir.path())))
            .unwrap();
        reg.add_source(&summary.id, 1, "/", 0, Arc::new(DiskProvider::new(docs_dir.path())))
            .unwrap();

        reg.with_session_mut(&summary.id, |live| {
            let kernel = live.session.kernel();
            let (fh, size, _) = kernel.open(RootId(0), "same.txt", OPEN_READ).unwrap();
            let mut buf = [0u8; 32];
            let n = kernel.read(fh, 0, &mut buf).unwrap();
            assert_eq!(&buf[..n], b"GAME-BYTES");
            assert_eq!(size as usize, n);
            kernel.close(fh).unwrap();

            let (fh, size, _) = kernel.open(RootId(1), "same.txt", OPEN_READ).unwrap();
            let mut buf = [0u8; 32];
            let n = kernel.read(fh, 0, &mut buf).unwrap();
            assert_eq!(&buf[..n], b"DOCS-BYTES");
            assert_eq!(size as usize, n);
            kernel.close(fh).unwrap();
            Ok(())
        })
        .unwrap();
    }

    fn toml_quote(s: &str) -> String {
        format!("{:?}", s)
    }
}


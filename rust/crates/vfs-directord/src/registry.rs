//! Live session registry: id → host [`Session`].

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

// Only `vfs_embed`, for everything about a session, its graph and its launch:
// this daemon is a *host* over the embeddable API (design spec §8, "one
// session-lifecycle implementation, two callers"), and a host that reaches
// into the engine is evidence the seam is in the wrong place. The cache and
// composition primitives below are re-exports from `vfs-embed`'s own catalog,
// not a second route to the same crates. `daemon_names_only_the_embed_api`
// (bottom of this file) keeps it that way.
use vfs_embed::{
    stack_layers, BlockCache, CacheConfig, CachingProvider, LaunchOpts, Provider, RootId,
    RootSources, Session,
};

/// Build the composed provider each root in a [`vfs_control::SessionConfig`]
/// serves — the config → provider-graph half of stage 2b's "one provider per
/// root" (design spec §6).
///
/// **Stays in the daemon on purpose.** This is the adapter from *this host's*
/// config format — `vfs-control`'s TOML/proto `SessionConfig` — onto the
/// embeddable API, and a host that composes its graph from code has no use
/// for it (spec §6: "config is a serialization of the graph, not the other
/// way round"). Moving it into `vfs-embed` would put `vfs-control`'s tonic /
/// prost / `protoc` build chain into the crate a language binding links,
/// buying that binding nothing. What *is* reusable — the rule below — lives
/// in `vfs-embed` already, as [`vfs_embed::RootSources`] and
/// [`vfs_embed::compose_root`].
///
/// A source with no explicit `root` defaults to root `0`; sources sharing a
/// root are combined with [`stack_layers`] in declaration order (later wins),
/// exactly the documented flat-`[[source]]`-list sugar — generalized here to
/// however many roots the config declares rather than assuming there is only
/// one. A source flagged `write_layer` is not one of those layers: it becomes
/// the root's writable upper via [`vfs_embed::compose_root`], the same
/// composition [`SessionRegistry::set_write_layer`] performs on the live
/// path, so a config built here and the same config applied to a running
/// session cannot disagree about whether writes copy up.
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
/// addressed via [`vfs_embed::VPath`], not through a session's ring/IPC
/// path.
pub fn build_provider_graph(
    cfg: &vfs_control::SessionConfig,
) -> Result<BTreeMap<RootId, Arc<dyn Provider>>, String> {
    cfg.validate_roots()?;
    let mut by_root: BTreeMap<u32, Vec<Arc<dyn Provider>>> = BTreeMap::new();
    let mut write_layers: BTreeMap<u32, Arc<dyn Provider>> = BTreeMap::new();
    for entry in &cfg.sources {
        let backend = vfs_source::build_provider(&entry.spec).map_err(|e| e.to_string())?;
        if entry.write_layer {
            // `validate_roots` already refused a second one for this root.
            write_layers.insert(entry.root, backend);
        } else {
            by_root.entry(entry.root).or_default().push(backend);
        }
    }
    let mut graph = BTreeMap::new();
    for root in by_root
        .keys()
        .chain(write_layers.keys())
        .copied()
        .collect::<std::collections::BTreeSet<u32>>()
    {
        let mounts = match by_root.remove(&root) {
            Some(stack) => vec![(String::new(), stack_layers(stack).map_err(|e| e.to_string())?)],
            None => Vec::new(),
        };
        let composed = vfs_embed::compose_root(mounts, write_layers.remove(&root))
            .map_err(|st| format!("compose root {root}: status {st}"))?;
        graph.insert(RootId(root), composed);
    }
    Ok(graph)
}

/// Clear whatever a previous run left at a session's base directory, so the
/// new session starts empty — see [`SessionRegistry::create`], which is the
/// only caller and explains why the path can be inherited at all.
///
/// Best-effort: a directory that cannot be removed (a live handle in it, say)
/// leaves the session running on top of it, which is what happened before this
/// existed. Failing session creation outright would be worse — the litter is
/// another process's, and it is not this session's job to be blocked by it.
fn prepare_session_base(base: &Path) {
    let _ = std::fs::remove_dir_all(base);
}

/// One live host session: the [`Session`] and what this host keeps beside
/// it. Reached through [`SessionRegistry::with_session_mut`], which holds
/// **only this session's** lock — see [`SessionEntry`].
pub struct LiveSession {
    pub id: String,
    pub name: String,
    pub session: Session,
    next_source_id: AtomicU64,
    /// Per-declared-root bookkeeping for rebuild. Keyed by the raw `u32` a
    /// `SourceEntry`/`AddSourceReq` names — `RootId` wraps this only at the
    /// `Director` boundary. The accumulate-and-recompose rule itself is
    /// [`vfs_embed::RootSources`]; what is daemon-specific is only *which*
    /// session it belongs to.
    ///
    /// Staging is **not** in here. It used to be — one more source at
    /// `i32::MIN` — and it is now the session's own, because it is not a
    /// source this host declared but a consequence of launching (Task 4b; see
    /// [`vfs_embed::Session::stage_launch`]).
    roots: HashMap<u32, RootSources>,
}

/// What `ListSessions`, `{Name}` expansion and name lookup read, kept
/// **outside** the session's own lock so none of them waits for a launch.
struct SessionMeta {
    /// Root 0's location — what the session summary reports as `root`.
    root: PathBuf,
    /// Each declared root's `[[root]] name`, for `{Name}` launch paths
    /// ([`SessionRegistry::expand_root_name`]). A root declared without a
    /// name has no entry.
    root_names: BTreeMap<u32, String>,
    /// Each declared root's location exactly as declared — what a `{Name}`
    /// expands to.
    root_locs: BTreeMap<u32, String>,
}

/// One registry slot. The map holds these behind an `Arc` so that a
/// long-running operation on one session — above all a waited launch, which
/// on Linux lasts as long as the game runs — clones the `Arc` out, **drops the
/// map lock**, and holds only `live`'s lock while it works. Every other
/// session, and every command that only reads the registry (`health`,
/// `sessions`, `stats`, name lookup), proceeds meanwhile.
///
/// Lock order, where both are held: map, then `meta` or `live` — never the
/// map while holding `live`.
struct SessionEntry {
    id: String,
    name: String,
    meta: Mutex<SessionMeta>,
    live: Mutex<LiveSession>,
}

impl LiveSession {
    pub fn next_source_id(&self) -> u64 {
        self.next_source_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Say, per root, whether that root can copy up — at **launch**, the
    /// moment its composition is final and a real process is about to write
    /// through it.
    ///
    /// Not at `add_source`: sources and the write layer arrive in config
    /// order, so a session that declares its layer last would be warned about
    /// and then immediately corrected, which trains readers to ignore the
    /// line.
    ///
    /// A missing write layer is otherwise **invisible until it bites**, and
    /// what it does then is not "the root is read-only" — that would at least
    /// be obvious. Creates still succeed: a layered stack routes them to the
    /// topmost writable source, so a game's new files land in whichever mod
    /// directory happened to be declared last. Only an in-place edit of
    /// content a read-only source holds fails, in-game, hours later, with
    /// nothing naming the flag that would have fixed it.
    fn report_write_layers(&self) {
        // Every root the *session* composes, not every root `add_source`
        // recorded: `set_write_layer` never touches `self.roots`, so a root
        // declared with a write layer and no ordinary source would have been
        // missing from both branches below — absent from the very report
        // meant to make write-layer state visible.
        for root in self.session.composed_roots() {
            let root = root.0;
            if self.session.has_write_layer(RootId(root)) {
                eprintln!(
                    "vfs: session {} root {root}: writes copy up into its write layer",
                    self.id
                );
            } else {
                eprintln!(
                    "vfs: session {} root {root}: NO write layer — writes cannot copy up. \
                     An in-place edit of content a read-only source (zip/remote) holds will \
                     fail, and new files land in the topmost writable source rather than a \
                     directory of your choosing. Declare one with `write_layer = true` on a \
                     disk source, or `vfs launch --write-layer <dir>`.",
                    self.id
                );
            }
        }
    }
}

/// How [`SessionRegistry::create`]'s refusal of an already-live name begins,
/// so the gRPC layer can answer it as `AlreadyExists` rather than guessing
/// from prose.
pub const DUPLICATE_NAME: &str = "a live session is already named";

/// Process-wide sequence for session base-directory naming — see the comment
/// in [`SessionRegistry::create`] for why this must be independent of any one
/// registry's own session-id counter.
static SESSION_BASE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Process-wide multi-session table owned by the daemon.
#[derive(Clone)]
pub struct SessionRegistry {
    inner: Arc<Mutex<HashMap<String, Arc<SessionEntry>>>>,
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

    /// Create a session named `name` (empty: unnamed).
    ///
    /// A non-empty name already held by a live session is refused, naming
    /// that session's id (the message starts with [`DUPLICATE_NAME`]): a
    /// second live `demo` would make every later `--session demo` ambiguous,
    /// and on Linux both would use the one persistent prefix
    /// `$VFS_HOME/sessions/demo`. Checked before any work and again at
    /// insertion, under the map lock, so two concurrent creates cannot both
    /// win.
    pub fn create(&self, name: String) -> Result<SessionSummary, String> {
        self.refuse_live_name(
            &name,
            &*self.inner.lock().map_err(|_| "session registry poisoned".to_string())?,
        )?;
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
        // A new session starts from an empty directory. Every component of
        // that name repeats across *runs* — the OS recycles pids freely, and
        // `base_seq`/`id` both restart at zero in each new process — and
        // nothing deletes a session's directory when the process that owned
        // it dies. So a session can inherit a previous run's litter at
        // exactly the same path.
        //
        // That is not a cosmetic leak. `overlay/` is the shim-local write
        // overlay, and "the overlay is empty after this launch" is how the
        // e2e scenarios detect the write bypass this gate closes: an inherited
        // `overlay/root-0` from some earlier process fails that assertion
        // while nothing at all fell through in the run being measured. It has
        // been observed, and the directory it complained about had been
        // written by a different test binary whose pid was later reused.
        prepare_session_base(&base);
        let root = base.join("root");
        let overlay = base.join("overlay");
        let state = base.join("state");

        let mut session = Session::new();
        session.set_root(&root);
        session.set_overlay(&overlay);
        session.set_state_dir(&state);
        // A named session gets a persistent Wine prefix named after it
        // (`$VFS_HOME/sessions/<name>/`), reused across runs; an unnamed one
        // an anonymous prefix deleted when the session drops. Refused before
        // `serve`, so a name that cannot be a prefix never becomes a session.
        #[cfg(unix)]
        if !name.is_empty() {
            session
                .set_prefix_name(&name)
                .map_err(|e| format!("session name {name:?} cannot name a Wine prefix: {e}"))?;
        }
        session.serve()?;

        // The summary reports root 0's *location* — where the program sees it —
        // never its host backing directory, so one field means one thing on
        // every platform (on Windows the two are the same path).
        let root = root0_location(&session);
        let summary = SessionSummary {
            id: id.clone(),
            name: name.clone(),
            root: root.clone(),
        };

        let name_for_check = name.clone();
        let entry = SessionEntry {
            id: id.clone(),
            name: name.clone(),
            meta: Mutex::new(SessionMeta {
                root,
                root_names: BTreeMap::new(),
                root_locs: BTreeMap::new(),
            }),
            live: Mutex::new(LiveSession {
                id: id.clone(),
                name,
                session,
                next_source_id: AtomicU64::new(1),
                roots: HashMap::new(),
            }),
        };

        let mut map = self
            .inner
            .lock()
            .map_err(|_| "session registry poisoned".to_string())?;
        // A concurrent create may have taken the name since the check above.
        // `entry` (and its `Session`) is dropped on the way out, lock released
        // first.
        if let Err(e) = self.refuse_live_name(&name_for_check, &map) {
            drop(map);
            drop(entry);
            return Err(e);
        }
        map.insert(id, Arc::new(entry));
        Ok(summary)
    }

    fn refuse_live_name(
        &self,
        name: &str,
        map: &HashMap<String, Arc<SessionEntry>>,
    ) -> Result<(), String> {
        if name.is_empty() {
            return Ok(());
        }
        match map.values().find(|e| e.name == name) {
            Some(live) => Err(format!(
                "{DUPLICATE_NAME} {name:?} ({}); take it down first (`vfs down --session {}`) \
                 or pick another name",
                live.id, live.id
            )),
            None => Ok(()),
        }
    }

    /// The entry for `id`, cloned out so the caller can drop the map lock
    /// before taking the session's own.
    fn entry(&self, id: &str) -> Result<Arc<SessionEntry>, String> {
        self.inner
            .lock()
            .map_err(|_| "session registry poisoned".to_string())?
            .get(id)
            .cloned()
            .ok_or_else(|| format!("unknown session {id}"))
    }

    /// Add one source to `session_id`, targeting `root` (`0` for every
    /// caller that predates stage 2b — the CLI and every existing config).
    /// Rebuilds only `root`'s composed provider and re-mounts it at
    /// `RootId(root)` in the live `Director`; other roots are untouched.
    ///
    /// Sources added here are **siblings**: they compose into a layer stack /
    /// mount graph, which can route a write to whichever source owns the path
    /// but cannot seed a copy from a lower layer. Copy-on-write over
    /// read-only content is [`Self::set_write_layer`], and the rebuild below
    /// goes through `Session` precisely so the two compose instead of
    /// clobbering each other.
    pub fn add_source(
        &self,
        session_id: &str,
        root: u32,
        mount: &str,
        layer: i32,
        backend: Arc<dyn Provider>,
    ) -> Result<u64, String> {
        self.with_session_mut(session_id, |live| {
            let id = live.next_source_id();
            // Wrap with process-wide block cache.
            let cached: Arc<dyn Provider> =
                Arc::new(CachingProvider::new(backend, Arc::clone(&self.cache), id));
            let build = live.roots.entry(root).or_default();
            build.add(mount, layer, cached);
            // Rebuild this root's sibling-mount list from its recorded source
            // list (layered root sources + non-root prefix mounts) and hand
            // the *list* to the session, which composes it — with this root's
            // write layer, if it has one — and replaces what `Director`
            // serves. `Director` holds exactly one provider per root, so
            // there is no incremental mount to append to.
            let mounts = build.mounts()?;
            live.session
                .set_root_mounts(RootId(root), mounts)
                .map_err(|st| format!("mount root {root} status {st}"))?;
            Ok(id)
        })
    }

    /// Declare the writable layer `root`'s writes land in for `session_id` —
    /// the gRPC/TOML surface's half of [`vfs_embed::Session::set_write_layer_at`],
    /// and the only way a daemon session gets **copy-on-write**.
    ///
    /// This is deliberately not [`Self::add_source`] with a flag on the
    /// provider's own capabilities, because the two are different facts. A
    /// source says *this content is part of the root*; a write layer says
    /// *writes to the root land here, seeded from whatever the sources hold*.
    /// A modded game has several writable sources (every mod directory on
    /// disk is one) and exactly one place its writes belong — inferring the
    /// write layer from "the topmost source that happens to be writable"
    /// would scatter a game's saves and edited INIs into whichever mod folder
    /// was declared last.
    ///
    /// Unlike a source, the layer is **not** wrapped in the block cache: it
    /// is the one provider in the graph whose bytes change underneath the
    /// director, and a cached read of a just-copied-up file would serve the
    /// pre-write content.
    ///
    /// Order-independent with respect to `add_source`: whichever comes second
    /// recomposes the root from both halves. Returns a source id, so a caller
    /// can refer to the layer the same way it refers to a source.
    pub fn set_write_layer(
        &self,
        session_id: &str,
        root: u32,
        upper: Arc<dyn Provider>,
    ) -> Result<u64, String> {
        self.with_session_mut(session_id, |live| {
            let id = live.next_source_id();
            live.session
                .set_write_layer_at(RootId(root), upper)
                .map_err(|st| format!("set write layer for root {root}: status {st}"))?;
            Ok(id)
        })
    }

    /// Declare where a root is — its location — so the injected shim
    /// recognises paths under it as belonging to `root` rather than to no
    /// one.
    ///
    /// The companion to [`Self::add_source`], and deliberately not folded
    /// into it: `add_source` says *what a root serves*, this says *where the
    /// game will look for it*. A config's `[[root]] path` is the source of
    /// truth for this; `add_source` never sees it, because `AddSourceReq`
    /// carries a root id and no path.
    ///
    /// `path` is the root's **location** — where the launched program sees
    /// it (a host path on Windows, a `C:\…` path inside the Wine prefix on
    /// Linux). `name`, when non-empty, is the root's `[[root]] name`, which a
    /// launch path can then spell as `{name}\…` (see
    /// [`Self::expand_root_name`]).
    ///
    /// Root 0 may be declared too: it moves where the program sees root 0,
    /// replacing the daemon's default, and the session summary's `root`
    /// (always root 0's location) becomes the declared one. On Linux that is
    /// root 0's location in the prefix (still backed by the session's own
    /// directory); on Windows it is root 0's host directory, and the launch
    /// republishes the shim's config with it — see
    /// [`vfs_embed::Session::launch`].
    ///
    /// On unix a location `launch` could not link into the Wine prefix is
    /// refused here ([`Session::check_root_location`]), and nothing is
    /// recorded.
    pub fn declare_root(
        &self,
        session_id: &str,
        root: u32,
        path: &Path,
        name: &str,
    ) -> Result<(), String> {
        // On Linux a location is a `C:\…` path the launch links into the
        // Wine prefix; one it could not link (another drive, a host path, the
        // drive root, `..`) is refused here, at `vfs up`, rather than at the
        // first `vfs exec`.
        #[cfg(unix)]
        Session::check_root_location(&path.to_string_lossy())
            .map_err(|e| format!("root {root}: {e}"))?;
        let entry = self.entry(session_id)?;
        let mut live = entry
            .live
            .lock()
            .map_err(|_| format!("session {session_id} poisoned"))?;
        live.session.declare_root(root, path);
        let root0 = root0_location(&live.session);
        drop(live);
        let mut meta = entry
            .meta
            .lock()
            .map_err(|_| format!("session {session_id} poisoned"))?;
        if !name.is_empty() {
            meta.root_names.insert(root, name.to_string());
        }
        meta.root_locs.insert(root, path.to_string_lossy().into_owned());
        meta.root = root0;
        Ok(())
    }

    /// The id of the live session `id_or_name` names: an exact session id
    /// first, else the one live session with that name.
    ///
    /// No match lists every live session; a name two live sessions share is
    /// refused as ambiguous, listing their ids — launching into whichever one
    /// a hash map happened to yield first would be a coin flip.
    pub fn resolve_session(&self, id_or_name: &str) -> Result<String, String> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| "session registry poisoned".to_string())?;
        if guard.contains_key(id_or_name) {
            return Ok(id_or_name.to_string());
        }
        let mut named: Vec<&str> = guard
            .values()
            .filter(|s| s.name == id_or_name)
            .map(|s| s.id.as_str())
            .collect();
        named.sort();
        match named.as_slice() {
            [one] => Ok(one.to_string()),
            [] => {
                let mut live: Vec<String> =
                    guard.values().map(|s| format!("{} ({})", s.id, s.name)).collect();
                live.sort();
                let live = if live.is_empty() { "none".to_string() } else { live.join(", ") };
                Err(format!("no live session is named or numbered {id_or_name}; live: {live}"))
            }
            many => Err(format!(
                "session name {id_or_name} is ambiguous: ids {}; use an id",
                many.join(", ")
            )),
        }
    }

    /// Expand a leading `{Name}` in a launch path to that root's declared
    /// location (names match case-insensitively); anything else is returned
    /// unchanged. An unknown name is refused, listing this session's root
    /// names.
    ///
    /// The daemon's job rather than `Session`'s: names are config-level, and
    /// `Session` knows only root ids and locations. The expanded path is then
    /// resolved by `Session::launch` like any other absolute path.
    pub fn expand_root_name(&self, session_id: &str, exec: &str) -> Result<String, String> {
        let Some((name, rest)) = exec.strip_prefix('{').and_then(|t| t.split_once('}')) else {
            return Ok(exec.to_string());
        };
        let entry = self.entry(session_id)?;
        let meta = entry
            .meta
            .lock()
            .map_err(|_| format!("session {session_id} poisoned"))?;
        let Some(root) = meta
            .root_names
            .iter()
            .find(|(_, n)| n.eq_ignore_ascii_case(name))
            .map(|(root, _)| *root)
        else {
            let names: Vec<&str> = meta.root_names.values().map(String::as_str).collect();
            let names = if names.is_empty() { "none".to_string() } else { names.join(", ") };
            return Err(format!("unknown root name {name}; this session's roots: {names}"));
        };
        let location = meta
            .root_locs
            .get(&root)
            .ok_or_else(|| format!("root {root} ({name}) has no declared location"))?;
        let rest = rest.trim_start_matches(['\\', '/']);
        if rest.is_empty() {
            return Ok(location.clone());
        }
        Ok(vfs_embed::image::join_location(location, &rest.replace('\\', "/")))
    }

    /// Run `f` on session `id` holding **only that session's** lock: the
    /// registry-wide map lock is released first, so a long `f` (a waited
    /// launch) blocks nobody but a caller of this same session.
    pub fn with_session_mut<R>(
        &self,
        id: &str,
        f: impl FnOnce(&mut LiveSession) -> Result<R, String>,
    ) -> Result<R, String> {
        let entry = self.entry(id)?;
        let mut live = entry
            .live
            .lock()
            .map_err(|_| format!("session {id} poisoned"))?;
        f(&mut live)
    }

    /// Every live session's summary. Never waits for a launch: it reads
    /// only the map and each session's metadata, not the session itself.
    pub fn list(&self) -> Result<Vec<SessionSummary>, String> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| "session registry poisoned".to_string())?;
        guard
            .values()
            .map(|s| {
                let meta = s
                    .meta
                    .lock()
                    .map_err(|_| format!("session {} poisoned", s.id))?;
                Ok(SessionSummary {
                    id: s.id.clone(),
                    name: s.name.clone(),
                    root: meta.root.clone(),
                })
            })
            .collect()
    }

    /// Tear session `id` down: stop serving it and drop it.
    ///
    /// **Refused while the session is running a launch** (its lock is held),
    /// with a message saying so, rather than waiting: on Linux a launch lasts
    /// as long as the program runs, and a `vfs down` that hangs until the
    /// game exits looks exactly like a dead daemon. Stop the program, then
    /// tear the session down.
    ///
    /// The session is dropped **after** the map lock is released — dropping
    /// an anonymous-prefix session stops its `wineserver` and deletes the
    /// prefix, which must not stall every other command.
    pub fn teardown(&self, id: &str) -> Result<(), String> {
        let entry = {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| "session registry poisoned".to_string())?;
            let entry = guard
                .get(id)
                .ok_or_else(|| format!("unknown session {id}"))?;
            match entry.live.try_lock() {
                Ok(mut live) => live.session.stop_serve(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    return Err(format!(
                        "session {id} is running a launch; it can be torn down once the \
                         program exits"
                    ))
                }
                Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner().session.stop_serve(),
            }
            guard.remove(id)
        };
        drop(entry);
        Ok(())
    }

    /// Tear every live session down — the daemon's shutdown drain, so each
    /// `Session`'s `Drop` runs (on Linux: root links removed, anonymous Wine
    /// prefixes deleted) instead of being skipped by process exit. Returns
    /// how many sessions it removed.
    ///
    /// Unlike [`Self::teardown`] this never refuses: a session still running a
    /// launch is removed from the registry all the same, and is dropped by
    /// that launch's own thread when the launch returns (it holds the last
    /// reference). Sessions are dropped after the map lock is released.
    pub fn teardown_all(&self) -> usize {
        let entries: Vec<Arc<SessionEntry>> = match self.inner.lock() {
            Ok(mut map) => map.drain().map(|(_, e)| e).collect(),
            Err(p) => p.into_inner().drain().map(|(_, e)| e).collect(),
        };
        let n = entries.len();
        for entry in entries {
            if let Ok(mut live) = entry.live.try_lock() {
                live.session.stop_serve();
            }
            drop(entry);
        }
        n
    }

    /// The production launch entrypoint (`DirectorService::launch` → here,
    /// the same path `vfs launch --exec` and scenario-TOML `[launch] exec =`
    /// drive). `opts.image` here names a VFS vpath, not an already-staged
    /// disk path — same as any other content path a client asks the director
    /// about.
    ///
    /// Staging that vpath out to disk, mounting the staging directory back
    /// under the curated graph and keeping it alive for the child **used to
    /// live here** and is now [`vfs_embed::Session::launch`]'s, where the Node
    /// and Python bindings can reach it too (Task 4b). This method adds only
    /// what is this host's: the per-root write-layer report, at the one moment
    /// it is both true and actionable.
    ///
    /// An absolute `opts.image` — an already-staged path (as `skyrim-live.rs`
    /// builds and passes directly to `Session::launch`, bypassing the
    /// registry), or a test-fixture binary that was never VFS content — is
    /// launched as given; that split is `Session::launch`'s and no longer
    /// re-implemented here.
    ///
    /// Holds only this session's lock for the launch (see
    /// [`Self::with_session_mut`]); the rest of the registry stays usable
    /// while the program runs.
    pub fn launch(&self, id: &str, opts: LaunchOpts) -> Result<i32, String> {
        self.with_session_mut(id, |live| {
            // The composition is final here and about to be written through
            // by a real process — the one moment where "this root cannot copy
            // up" is both certainly true and still actionable.
            live.report_write_layers();
            live.session.launch(&opts)
        })
    }
}

/// Root 0's location as the launched program sees it: the declared one, else
/// the default — the daemon's host directory on Windows, `C:\vfs-session\root`
/// on Linux.
fn root0_location(session: &Session) -> PathBuf {
    session
        .root_locations()
        .into_iter()
        .next()
        .map(|r| PathBuf::from(r.location))
        .unwrap_or_default()
}

/// One live session as `ListSessions`/`CreateSession` report it. `root` is
/// root 0's **location** (see [`root0_location`]), not its host backing
/// directory.
#[derive(Clone, Debug)]
pub struct SessionSummary {
    pub id: String,
    pub name: String,
    pub root: PathBuf,
}

/// Spec §8: "one session-lifecycle implementation, two callers." This is the
/// first caller, and an API that only its author calls has not been shown to
/// be an API — so the claim is asserted rather than described.
///
/// Every engine crate below is fully re-exported by `vfs-embed`; naming one
/// directly is not a compile error and never will be (they are still real
/// dependencies of this crate, because `skyrim-live.rs` — stage 5's problem —
/// lives in it). So the only way "the daemon goes through the embed API" stays
/// true is to read the source text back, exactly as
/// `vfs-embed/tests/embed_api.rs` does from the other side of the seam.
///
/// `vfs-control` and `vfs-source` are deliberately **not** on the list. They
/// are this host's config format and its `SourceSpec` → provider factory —
/// properties of *this* host, kept out of `vfs-embed` on purpose so a language
/// binding does not link tonic, prost and a vendored `protoc`.
///
/// If this test fails, the fix is almost never to add a needle exception. It
/// is either to route the call through `vfs_embed`, or — if `vfs_embed` cannot
/// express it — to add it there, because whatever the daemon just needed the
/// Node and Python bindings will need next.
///
/// **The file list is read off the filesystem, not written down here.** A
/// hand-written list was the first version of this test and it already had a
/// hole: it named four files and `src/` has five, so `discovery.rs` — a
/// `pub mod` of this crate — was never checked. It happened to be clean. A
/// guard with a silent hole is worse than no guard, because it is *believed*,
/// so the enumeration has to be one a new module cannot be added outside of.
#[cfg(test)]
#[test]
fn daemon_names_only_the_embed_api() {
    // Assembled at compile time so that spelling them here does not trip the
    // check when this file is one of the ones being read. Everything below the
    // seam belongs on this list, not only the crates that happen to hold
    // providers: the shim, the injector, the ring and the Win32 wrappers are
    // all engine, and naming any of them is the same mistake.
    let needles = [
        concat!("vfs_", "director", "::"),
        concat!("vfs_", "cache", "::"),
        concat!("vfs_", "compose", "::"),
        concat!("vfs_", "protocol", "::"),
        concat!("vfs_", "provider", "::"),
        concat!("vfs_", "zip", "::"),
        concat!("vfs_", "shim", "::"),
        concat!("vfs_", "inject", "::"),
        concat!("vfs_", "ipc", "::"),
        concat!("vfs_", "core", "::"),
        concat!("vfs_", "win", "::"),
    ];

    // `src/*.rs` — every module of the daemon library plus the `vfs` binary.
    // `src/bin/` is deliberately not descended into: `skyrim-live.rs` is a
    // scenario harness that stage 5 removes from this crate, and it is a
    // legitimate direct kernel user until then.
    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&src_dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", src_dir.display()))
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    files.sort();
    assert!(
        files.iter().any(|p| p.ends_with("registry.rs")) && files.len() >= 5,
        "the enumeration must have found the daemon's sources — got {files:?}"
    );

    for path in files {
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        for needle in needles {
            assert!(
                !src.contains(needle),
                "vfs-directord/src/{name} names `{needle}` — the daemon is a host over \
                 vfs-embed and must reach the engine only through it. Route it through \
                 `vfs_embed`, or add the missing piece to vfs-embed; do not widen this list."
            );
        }
    }
}

#[cfg(test)]
mod root_graph_tests {
    use super::*;
    use vfs_control::{SessionConfig, SourceEntry, SourceSpec};
    use vfs_embed::DiskProvider;
    use vfs_embed::OPEN_READ;
    use vfs_embed::VPath;

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

    /// A session must not start on top of another run's files.
    ///
    /// Session directories are named `vfs-daemon-{pid}-{seq}-{id}` and every
    /// component repeats across runs: the OS recycles pids, and both counters
    /// restart at zero in each process. Nothing deletes a session's directory
    /// when its process dies — this workspace's own suites had left **1551**
    /// behind on the development machine, 96 with an `overlay/root-0` and 40
    /// of those holding files.
    ///
    /// That is not housekeeping. `overlay/` is the shim-local write overlay,
    /// and "the overlay is empty afterwards" is how the e2e write-path
    /// scenarios detect the bypass gate 4 closes. An inherited `root-0` fails
    /// that assertion with nothing having fallen through — observed, once,
    /// from a directory a different test binary had written before its pid was
    /// reused. The reverse is worse: a real bypass dismissed as this.
    ///
    /// Tested here rather than through `create`, whose path is derived from
    /// the pid and a process-wide counter: a test cannot arrange a collision
    /// with a *future* session without littering paths that other tests in the
    /// same binary are concurrently handed — which broke two of them when
    /// tried. `create`'s single call to this is verified by reading.
    #[test]
    fn prepare_session_base_clears_a_previous_runs_directory() {
        let base = std::env::temp_dir()
            .join(format!("vfs-prepare-base-{}-{}", std::process::id(), line!()));
        let stale = base.join("overlay").join("root-0").join("data");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("x.esp"), b"PREVIOUS-RUN").unwrap();
        assert!(stale.join("x.esp").is_file(), "litter must exist to be cleared");

        prepare_session_base(&base);

        assert!(
            !base.exists(),
            "a session's base directory must not survive into the next session that is \
             handed the same path — {base:?} still holds {:?}",
            std::fs::read_dir(&base).map(|rd| rd.flatten().map(|e| e.path()).collect::<Vec<_>>())
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The config → graph route must agree with the live route about
    /// copy-on-write, or the two drift again: a `write_layer = true` source
    /// composes as the root's writable upper here too, so a graph built from
    /// a config serves an in-place edit of read-only content instead of
    /// refusing it.
    #[test]
    fn a_write_layer_source_composes_as_the_roots_writable_upper() {
        let content = tempfile::tempdir().unwrap();
        let overwrite = tempfile::tempdir().unwrap();
        std::fs::write(content.path().join("x.esp"), b"ORIGINAL").unwrap();

        let cfg = SessionConfig {
            sources: vec![
                SourceEntry {
                    spec: SourceSpec::Disk {
                        path: content.path().to_string_lossy().into_owned(),
                    },
                    mount: "/".into(),
                    root: 0,
                    write_layer: false,
                    cache_key: None,
                },
                SourceEntry {
                    spec: SourceSpec::Disk {
                        path: overwrite.path().to_string_lossy().into_owned(),
                    },
                    mount: "/".into(),
                    root: 0,
                    write_layer: true,
                    cache_key: None,
                },
            ],
            ..Default::default()
        };

        let graph = build_provider_graph(&cfg).expect("build provider graph");
        let root0 = graph.get(&RootId(0)).unwrap();
        let (h, size, _) = root0
            .open(VPath::at_default("x.esp"), vfs_embed::OPEN_WRITE)
            .expect("the write layer must make an in-place edit copy up");
        assert_eq!(size, 8, "the handle must open onto the copied-up content");
        root0.write_at(h, 0, b"EDITED!!").unwrap();
        root0.close(h).unwrap();

        assert_eq!(
            std::fs::read(overwrite.path().join("x.esp")).ok(),
            Some(b"EDITED!!".to_vec()),
            "the edit belongs in the write layer"
        );
        assert_eq!(
            std::fs::read(content.path().join("x.esp")).unwrap(),
            b"ORIGINAL",
            "the source it copied from must be untouched"
        );
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
                    write_layer: false,
                    cache_key: None,
                },
                SourceEntry {
                    spec: SourceSpec::Disk {
                        path: mod_dir.path().to_string_lossy().into_owned(),
                    },
                    mount: "/".into(),
                    root: 0,
                    write_layer: false,
                    cache_key: None,
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

    #[test]
    fn resolve_session_by_id_then_name() {
        let reg = SessionRegistry::new();
        let a = reg.create("alpha".into()).unwrap();
        assert_eq!(reg.resolve_session(&a.id).unwrap(), a.id);
        assert_eq!(reg.resolve_session("alpha").unwrap(), a.id);
    }

    #[test]
    fn resolve_session_unknown_lists_what_exists() {
        let reg = SessionRegistry::new();
        reg.create("alpha".into()).unwrap();
        let e = reg.resolve_session("beta").unwrap_err();
        assert!(e.contains("beta") && e.contains("alpha"), "{e}");
    }

    /// `create` refuses a second live session under one name (below), so an
    /// ambiguous name cannot arise through it any more; the refusal in
    /// `resolve_session` stays as the backstop, exercised here by renaming a
    /// live entry behind `create`'s back.
    #[test]
    fn resolve_session_ambiguous_name_lists_ids() {
        let reg = SessionRegistry::new();
        let a = reg.create("dup".into()).unwrap();
        let b = reg.create("dup-2".into()).unwrap();
        {
            let mut map = reg.inner.lock().unwrap();
            let entry = Arc::try_unwrap(map.remove(&b.id).unwrap()).ok().unwrap();
            map.insert(
                b.id.clone(),
                Arc::new(SessionEntry { name: "dup".into(), ..entry }),
            );
        }
        let e = reg.resolve_session("dup").unwrap_err();
        assert!(e.contains(&a.id) && e.contains(&b.id), "{e}");
    }

    /// Two `vfs up` runs of one config must not leave two live sessions
    /// sharing a name (every later `--session NAME` would be ambiguous, and on
    /// Linux both would share `$VFS_HOME/sessions/NAME`). The second is
    /// refused, naming the live one; once that is down the name is free.
    #[test]
    fn create_refuses_a_name_that_is_already_live() {
        let reg = SessionRegistry::new();
        let first = reg.create("demo".into()).unwrap();
        let e = reg.create("demo".into()).unwrap_err();
        assert!(
            e.contains("demo") && e.contains(&first.id) && e.starts_with(DUPLICATE_NAME),
            "{e}"
        );
        assert_eq!(reg.len(), 1, "the refused session must not be registered");
        // Unnamed sessions never collide.
        reg.create(String::new()).unwrap();
        reg.create(String::new()).unwrap();
        reg.teardown(&first.id).unwrap();
        reg.create("demo".into()).expect("the name is free once its session is down");
    }

    #[test]
    fn expand_root_name_replaces_the_name_with_its_location() {
        let reg = SessionRegistry::new();
        let s = reg.create("x".into()).unwrap();
        let loc = if cfg!(windows) { r"C:\vfs-test\Games" } else { r"C:\Games\Fixture" };
        reg.declare_root(&s.id, 0, Path::new(loc), "Games").unwrap();
        assert_eq!(
            reg.expand_root_name(&s.id, r"{games}\bin\f.exe").unwrap(),
            format!(r"{loc}\bin\f.exe"),
            "names match case-insensitively"
        );
        assert_eq!(reg.expand_root_name(&s.id, r"C:\other.exe").unwrap(), r"C:\other.exe");
        let e = reg.expand_root_name(&s.id, r"{Nope}\f.exe").unwrap_err();
        assert!(e.contains("Nope") && e.contains("Games"), "{e}");
    }

    /// A daemon session's name becomes its persistent Wine prefix's directory
    /// name, so a name that is not one plain path component is refused at
    /// `create` rather than at the first launch. A space is fine.
    #[cfg(unix)]
    #[test]
    fn create_refuses_a_name_that_cannot_name_a_prefix() {
        let reg = SessionRegistry::new();
        for bad in ["a/b", ".."] {
            let e = reg.create(bad.into()).expect_err(bad);
            assert!(e.contains("cannot name a Wine prefix") && e.contains(bad), "{e}");
        }
        assert!(reg.is_empty(), "a refused session must not be registered");
        reg.create("my game".into()).expect("a space is a plain path component");
        reg.create(String::new()).expect("no name: an anonymous prefix");
    }

    /// A long operation on one session — standing in for a waited launch,
    /// which on Linux lasts as long as the game runs — must not hold the
    /// registry: `health`/`sessions` (`len`/`list`), name lookup, `{Name}`
    /// expansion and other sessions all answer meanwhile, and a teardown of
    /// the busy session is refused by name instead of hanging.
    #[test]
    fn a_long_operation_on_one_session_does_not_block_the_registry() {
        use std::time::{Duration, Instant};
        let reg = SessionRegistry::new();
        let busy = reg.create("busy".into()).unwrap();
        let loc = if cfg!(windows) { r"C:\vfs-test\Busy" } else { r"C:\Games\Busy" };
        reg.declare_root(&busy.id, 0, Path::new(loc), "Games").unwrap();
        let other = reg.create("other".into()).unwrap();

        const HOLD: Duration = Duration::from_millis(1500);
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let holder = {
            let reg = reg.clone();
            let id = busy.id.clone();
            std::thread::spawn(move || {
                reg.with_session_mut(&id, |_live| {
                    held_tx.send(()).unwrap();
                    std::thread::sleep(HOLD);
                    Ok(())
                })
                .unwrap();
            })
        };
        held_rx.recv().unwrap();

        let start = Instant::now();
        assert_eq!(reg.len(), 2);
        assert_eq!(reg.list().unwrap().len(), 2);
        assert_eq!(reg.resolve_session("busy").unwrap(), busy.id);
        assert!(reg.expand_root_name(&busy.id, r"{Games}\x.exe").unwrap().ends_with("x.exe"));
        reg.with_session_mut(&other.id, |_| Ok(())).unwrap();
        let e = reg.teardown(&busy.id).unwrap_err();
        assert!(e.contains(&busy.id) && e.contains("running a launch"), "{e}");
        let waited = start.elapsed();
        assert!(
            waited < HOLD / 3,
            "the registry waited {waited:?} on a session another thread holds"
        );

        holder.join().unwrap();
        reg.teardown(&busy.id).expect("torn down once the operation ends");
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn teardown_all_removes_and_drops_every_session() {
        let reg = SessionRegistry::new();
        let a = reg.create("drain-a".into()).unwrap();
        reg.create(String::new()).unwrap();
        // Hold a clone of one session's entry, as a launch in flight does, to
        // observe when the registry lets go of it.
        let held = reg.entry(&a.id).unwrap();
        assert_eq!(Arc::strong_count(&held), 2);
        assert_eq!(reg.teardown_all(), 2);
        assert!(reg.is_empty());
        assert!(reg.list().unwrap().is_empty());
        assert_eq!(Arc::strong_count(&held), 1, "the registry must drop its reference");
        assert_eq!(reg.teardown_all(), 0, "draining an empty registry is a no-op");
    }

    /// A root location the launch could not link is refused when it is
    /// declared (`vfs up`), not at the first `vfs exec`, and nothing of it
    /// is recorded.
    #[cfg(unix)]
    #[test]
    fn declare_root_refuses_a_location_the_prefix_cannot_hold() {
        let reg = SessionRegistry::new();
        let s = reg.create("bad-loc".into()).unwrap();
        for bad in [r"D:\Games", "/tmp/host-dir", r"C:\", r"C:\a\..\b"] {
            for root in [0, 1] {
                let e = reg.declare_root(&s.id, root, Path::new(bad), "R").unwrap_err();
                assert!(e.contains(&format!("root {root}")) && e.contains(bad), "{bad}: {e}");
            }
        }
        assert!(reg.with_session_mut(&s.id, |l| Ok(l.session.declared_roots().is_empty())).unwrap());
        assert_eq!(reg.list().unwrap()[0].root, Path::new(r"C:\vfs-session\root"));
        assert!(reg.expand_root_name(&s.id, r"{R}\x.exe").is_err(), "no name was recorded");
    }

    /// The summary's `root` is root 0's location — where the program sees it
    /// — whether declared or defaulted, never the host directory backing it.
    #[test]
    fn the_summary_root_is_root_zeros_location() {
        let reg = SessionRegistry::new();
        let s = reg.create("summary-root".into()).unwrap();
        let loc = if cfg!(windows) { r"C:\vfs-test\Summary" } else { r"C:\Games\Summary" };
        let backing = reg
            .with_session_mut(&s.id, |l| Ok(l.session.virtual_root().to_path_buf()))
            .unwrap();
        if cfg!(windows) {
            assert_eq!(s.root, backing, "on Windows the default location is the host dir");
        } else {
            assert_eq!(s.root, Path::new(r"C:\vfs-session\root"));
            assert_ne!(s.root, backing);
        }
        assert_eq!(reg.list().unwrap()[0].root, s.root);
        reg.declare_root(&s.id, 1, Path::new(&format!(r"{loc}\Saves")), "Saves").unwrap();
        assert_eq!(reg.list().unwrap()[0].root, s.root, "another root leaves it alone");
        reg.declare_root(&s.id, 0, Path::new(loc), "Games").unwrap();
        assert_eq!(reg.list().unwrap()[0].root, Path::new(loc));
    }

    fn toml_quote(s: &str) -> String {
        format!("{:?}", s)
    }
}


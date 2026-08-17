//! **The `aethervfs` Node addon.** A host over [`vfs_embed`], on the same
//! footing as `vfs.exe` and the daemon — spec §8b.
//!
//! ```js
//! const { Session, disk } = require('aethervfs');
//!
//! const s = new Session('skyrim');
//! s.addRoot(0, 'game', gameRoot);          // an empty directory: the game is in the graph
//! s.mount(0, disk(modsDir));
//! s.serve();
//! const code = s.launch('SkyrimSE.exe', { wait: true });
//! console.log(s.rejectedWrites());
//! s.close();
//! ```
//!
//! ## What crosses the boundary, and what deliberately does not
//!
//! Task 5 established that **a handle a JS caller may need in another isolate
//! has to be a process-global integer, not a JS object** — Rust `static`s are
//! shared by every isolate that loads the addon, whereas nothing JS-visible
//! crosses an isolate boundary. That is why [`Provider`] is a two-field wrapper
//! around a `u32` index into [`providers`] rather than a class holding an
//! `Arc<dyn Provider>`: task 7 registers JS-authored providers *inside a
//! worker*, and the session that mounts them lives on another loop. Building
//! the object-holding version first would mean tearing it up.
//!
//! [`Session`] is the other way round on purpose: it holds its
//! [`vfs_embed::Session`] directly and is bound to the isolate that created it.
//! A session is driven from one place — the graph is composed, served and
//! launched by one caller — so there is nothing for a second isolate to do with
//! it, and a global session table would be a table of one with a lock around
//! it.
//!
//! ## Why `shimDll` / `payloadDll` are resolved here and not by `vfs-embed`
//!
//! [`vfs_embed::LaunchOpts::shim_dll`] falls back to searching next to
//! `std::env::current_exe()`, which inside an addon is **`node.exe`, wherever
//! the user installed Node** — nowhere near the shipped DLLs. So the binding
//! resolves them itself, from the directory the addon was loaded out of, which
//! `index.cjs` hands over as `__dirname` at load time (see [`set_package_dir`]).
//! `scripts/build.cjs` is what puts `vfs_shim_dll.dll` and `vfs_payload.dll`
//! in that directory, and `package.json`'s `files` list is what ships them.
//!
//! Left to guess, the symptom is "`vfs_shim_dll.dll` not found" from a package
//! that contains it, so [`Session::resolve_dlls`] names every location it tried
//! instead.
//!
//! ## JS-authored providers
//!
//! [`jsprovider`] is the rest of the story: a JavaScript object mounted as an
//! `Arc<dyn Provider>`, with spec §8b's threading contract enforced rather than
//! documented. Read its module docs before touching the bridge — in particular
//! the deadlock guard, which compares the calling thread against the *loop that
//! services the provider*, not against "is this the main thread".

// No `unsafe` anywhere in the binding. The one place task 5's spike needed it —
// memcpying a JS `Buffer` into a parked director thread's destination pointer —
// is done through an owned `Vec` here instead, for one extra memcpy of at most
// the read size. See `jsprovider`'s module docs for why that trade is not close.
#![deny(unsafe_code)]
// `--all-targets` checks the lib once more under `cfg(test)`, and napi-derive
// gates its `#[ctor]` module registrations on `not(test)`
// (`napi-derive-backend/src/codegen/fn.rs:669`). So in that configuration every
// `#[napi]` export loses its only caller, and everything reachable only from a
// private module — `jsprovider` — becomes unreachable with it. Crate-root `pub`
// items are exempt because they are the public API; items inside a private
// module are not, which is why this appears the moment the bridge moves into its
// own module and did not before.
//
// Nothing is lost by allowing it: dead-code analysis still runs in the ordinary
// configuration, where the ctors exist and genuinely unreachable code is still
// an error, and `[lib] test = false` means no test is ever *run* under `cfg(test)`
// anyway (see `Cargo.toml` for why).
#![cfg_attr(test, allow(dead_code))]

mod jsprovider;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use napi::bindgen_prelude::Buffer;
use napi::{Error, Result};
use napi_derive::napi;

use vfs_embed::{DiskProvider, LaunchOpts, Provider as VfsProvider, RootId};

// ---------------------------------------------------------------------------
// Status codes → JS errors.
// ---------------------------------------------------------------------------

/// What [`status_name`] answers for a status the workspace does not define.
/// `jsprovider` compares against it to decide whether a host-supplied status is
/// real, so the two must be the same string.
pub(crate) const UNKNOWN_STATUS: &str = "unknown status";

/// The `ST_*` name for a status, so a thrown error says `ST_READ_ONLY` rather
/// than `status -13`. A bare number sends a JS developer into Rust source to
/// find out what went wrong.
pub(crate) fn status_name(status: i32) -> &'static str {
    match status {
        vfs_embed::ST_OK => "ST_OK",
        vfs_embed::ST_NOT_FOUND => "ST_NOT_FOUND",
        vfs_embed::ST_IO_ERROR => "ST_IO_ERROR",
        vfs_embed::ST_NOT_SUPPORTED => "ST_NOT_SUPPORTED",
        vfs_embed::ST_BAD_FH => "ST_BAD_FH",
        vfs_embed::ST_IS_DIR => "ST_IS_DIR",
        vfs_embed::ST_NOT_A_DIRECTORY => "ST_NOT_A_DIRECTORY",
        vfs_embed::ST_READ_ONLY => "ST_READ_ONLY",
        vfs_embed::ST_EXISTS => "ST_EXISTS",
        vfs_embed::ST_NO_SPACE => "ST_NO_SPACE",
        vfs_embed::ST_BAD_REQUEST => "ST_BAD_REQUEST",
        _ => UNKNOWN_STATUS,
    }
}

/// Turn a status into something a JS developer can act on.
///
/// **A JS provider's failures reach here as a bare `ST_IO_ERROR`**, because that
/// is all `Provider` can carry. When the failing call happened *on this very
/// thread* — which is exactly the deadlock guard's case, since the guard fires
/// on the thread that would have hung — the bridge left a full explanation in a
/// thread-local, and it is worth far more than the status name. Every status →
/// error conversion in this file goes through here, so every entry point gets
/// that for free.
fn status_err(what: &str, status: i32) -> Error {
    match jsprovider::take_diagnosis() {
        Some(d) => Error::from_reason(format!("{what}: {d}")),
        None => Error::from_reason(format!(
            "{what}: {} (status {status})",
            status_name(status)
        )),
    }
}

/// The same, for the `Result<_, String>` surfaces `vfs-embed` exposes.
fn reason_err(what: &str, reason: String) -> Error {
    match jsprovider::take_diagnosis() {
        Some(d) => Error::from_reason(format!("{what}: {reason} — {d}")),
        None => Error::from_reason(reason),
    }
}

// ---------------------------------------------------------------------------
// The provider registry. Process-global, because a handle must mean the same
// thing in every isolate — see the module docs.
// ---------------------------------------------------------------------------

static PROVIDERS: OnceLock<RwLock<Vec<Arc<dyn VfsProvider>>>> = OnceLock::new();

fn providers() -> &'static RwLock<Vec<Arc<dyn VfsProvider>>> {
    PROVIDERS.get_or_init(|| RwLock::new(Vec::new()))
}

/// Park a provider in the registry and return its handle.
///
/// **Entries are never removed, and that is a deliberate bounded leak rather
/// than an oversight.** A handle's whole purpose is to be resolvable from an
/// isolate that never saw the object it came from, so it cannot be released
/// when one isolate's wrapper is garbage-collected — the worker that created it
/// may hand the integer to the main thread and drop its own reference. A host
/// creates providers when it composes a graph, not per read, so the table is
/// tens of entries; if a host ever creates them in a loop this needs an
/// explicit `release(handle)`, which is a real API decision and not a fix to
/// smuggle in here.
///
/// `releaseProvider` (see [`jsprovider`]) releases a JS provider's *event loop*,
/// which is a different thing and does not free this entry.
pub(crate) fn intern_provider(p: Arc<dyn VfsProvider>) -> Result<u32> {
    let mut g = providers()
        .write()
        .map_err(|_| Error::from_reason("provider registry poisoned"))?;
    g.push(p);
    Ok((g.len() - 1) as u32)
}

fn lookup_provider(handle: u32) -> Result<Arc<dyn VfsProvider>> {
    let g = providers()
        .read()
        .map_err(|_| Error::from_reason("provider registry poisoned"))?;
    g.get(handle as usize).map(Arc::clone).ok_or_else(|| {
        Error::from_reason(format!(
            "no provider with handle {handle}; {} have been created in this process",
            g.len()
        ))
    })
}

/// An opaque handle to a provider living in Rust.
///
/// The object is a wrapper; `handle` is the value. Pass the number to another
/// isolate and rebuild the wrapper there with `Provider.fromHandle(n)` — the
/// object itself cannot cross, the integer can.
#[napi]
pub struct Provider {
    handle: u32,
}

impl Provider {
    /// Wrap an already-interned handle. For [`jsprovider::register_provider`],
    /// which interns its own provider.
    pub(crate) fn wrap(handle: u32) -> Self {
        Provider { handle }
    }
}

#[napi]
impl Provider {
    /// Rebuild a wrapper from a handle, validating that the handle exists.
    #[napi(factory)]
    pub fn from_handle(handle: u32) -> Result<Self> {
        lookup_provider(handle)?;
        Ok(Provider { handle })
    }

    /// The process-global integer this wrapper stands for.
    #[napi(getter)]
    pub fn handle(&self) -> u32 {
        self.handle
    }

    /// Counters and configuration for a JS-authored provider; `null` for a Rust
    /// one, which has no bridge and nothing to report.
    ///
    /// This is where the failures spec §8b asks to be *counted* rather than
    /// merely survived actually land: `stalledCalls` for a call that has not
    /// settled, `abandonedCalls` for one given up on, `hostErrors` for a throw
    /// that became `ST_IO_ERROR`, and `selfCallRefusals` for a call the deadlock
    /// guard refused.
    #[napi]
    pub fn stats(&self) -> Option<jsprovider::ProviderStats> {
        jsprovider::stats_for(self.handle)
    }
}

/// A read-write provider over a real directory (spec §6's `disk` primitive).
///
/// The directory must already exist. Refusing a missing one here is not
/// pedantry: `DiskProvider` over a path that is not there answers `not found`
/// for everything, so a typo in a mod directory produces a session that serves
/// nothing and reports no error at all — the exact failure shape this project
/// keeps getting bitten by. A host wanting a fresh directory (a write layer,
/// say) creates it first; that is one line of JS and it is explicit.
#[napi]
pub fn disk(path: String) -> Result<Provider> {
    let p = PathBuf::from(&path);
    if !p.is_dir() {
        return Err(Error::from_reason(format!(
            "disk({path:?}): not an existing directory. A DiskProvider over a \
             missing path answers ST_NOT_FOUND for every read without reporting \
             an error, so it is refused here instead."
        )));
    }
    Ok(Provider {
        handle: intern_provider(Arc::new(DiskProvider::new(&p)))?,
    })
}

// ---------------------------------------------------------------------------
// Where the addon was loaded from — `index.cjs` supplies `__dirname`.
// ---------------------------------------------------------------------------

static PACKAGE_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Record the directory the addon was loaded out of. Called by `index.cjs` with
/// `__dirname`; it is where `vfs_shim_dll.dll` and `vfs_payload.dll` are looked
/// for. Rust cannot work this out for itself without asking Windows for the
/// module handle of its own code, and `current_exe()` gives `node.exe`.
#[napi]
pub fn set_package_dir(dir: String) {
    if let Ok(mut g) = PACKAGE_DIR.lock() {
        *g = Some(PathBuf::from(dir));
    }
}

/// The directory [`set_package_dir`] recorded, if any.
#[napi]
pub fn package_dir() -> Option<String> {
    PACKAGE_DIR
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|p| p.to_string_lossy().into_owned()))
}

// ---------------------------------------------------------------------------
// Plain-data types crossing to JS.
// ---------------------------------------------------------------------------

/// One declared root, as `session.roots()` reports it.
#[napi(object)]
pub struct RootInfo {
    pub id: u32,
    /// The host's label for this root. Carried for diagnostics only — the
    /// director addresses roots by id.
    pub name: String,
    pub path: String,
}

/// One refused write, as `session.rejectedWrites()` reports it.
#[napi(object)]
pub struct RejectedWrite {
    pub path: String,
    /// `f64` rather than `BigInt`: a rejection count cannot approach 2^53, and
    /// a `BigInt` would make the ordinary `count > 0` comparison throw against
    /// a plain number.
    pub count: f64,
}

/// Which DLLs a launch would use, and enough about them to notice a stale one.
///
/// Spec §8's packaging section asks a binding to *verify* DLL identity at
/// session start against a build hash embedded in each DLL. **No such hash
/// exists in the workspace today** (nothing defines or reads one), so this
/// reports size and mtime instead. That is strictly weaker — it lets a host
/// print which DLL it is about to inject and when it was built, which is
/// enough to catch the "I rebuilt and the old DLL was still there" trap, and
/// is not enough to catch two different DLLs of the same size. The build-hash
/// check remains open work.
#[napi(object)]
pub struct ShimInfo {
    pub shim_dll: String,
    pub payload_dll: String,
    pub shim_size: f64,
    /// Unix epoch milliseconds, or 0 if unavailable.
    pub shim_modified_ms: f64,
    pub payload_size: f64,
    pub payload_modified_ms: f64,
}

/// Everything [`Session::launch`] takes beyond the image name. Every field is
/// optional; the defaults are `vfs_embed::LaunchOpts`'s.
#[napi(object)]
pub struct LaunchOptions {
    pub args: Option<Vec<String>>,
    /// Wait for the child to exit (default `true`). With `false` the session
    /// must outlive the child — it owns the ring and the staged image.
    pub wait: Option<bool>,
    /// Override the shim DLL for this launch. Unset, it is resolved from
    /// `setShimDlls` and then from the package directory.
    pub shim_dll: Option<String>,
    /// Override the payload DLL. Unset but with `shimDll` given, it is looked
    /// for beside that shim.
    pub payload_dll: Option<String>,
    /// Extra images to stage beside a graph-resolved image, by vpath.
    pub stage_also: Option<Vec<String>>,
    /// Real-disk directories searched for imports the graph does not carry.
    pub stage_fallback_dirs: Option<Vec<String>>,
    /// Extra environment variables for the child only.
    pub env: Option<HashMap<String, String>>,
}

// ---------------------------------------------------------------------------
// Session.
// ---------------------------------------------------------------------------

/// Turn a host-supplied session name into something safe to put in a path.
///
/// The name reaches the filesystem (see [`Session::new`]), so `..\..\Windows`
/// or a colon would escape or break the temp directory. Everything outside
/// `[A-Za-z0-9._-]` becomes `_`, the result is capped, and an empty result
/// becomes `session` rather than producing a path ending in a separator.
fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    let s = s.trim_matches('.').to_string();
    if s.is_empty() {
        "session".to_string()
    } else {
        s
    }
}

/// One VFS session: roots, the provider graph each root serves, the ring the
/// injected shim talks over, and the launch.
#[napi]
pub struct Session {
    /// `None` after [`Session::close`]. Every accessor goes through
    /// [`Session::get`] so a call after close throws instead of silently doing
    /// nothing — a session that accepts mounts after teardown and serves none
    /// of them is the kind of quiet wrong answer this project audits for.
    inner: Option<vfs_embed::Session>,
    name: String,
    /// The directory tree this session's `root`/`overlay`/`state` live under.
    base: PathBuf,
    /// Declared roots with their host-supplied labels, in declaration order.
    /// `vfs_embed::Session` keeps ids and paths but has nowhere for a name.
    roots: Vec<RootInfo>,
    shim_dll: Option<String>,
    payload_dll: Option<String>,
}

#[napi]
impl Session {
    /// A session whose `root`, `overlay` and `state` directories live under
    /// `%TEMP%/aethervfs-<name>-<pid>-<seq>`.
    ///
    /// `name` is a label; it appears in those paths so a developer poking at
    /// `%TEMP%` can tell which session left what, and in `session.name`.
    ///
    /// **The base directory is cleared on the way in**, which is an obligation
    /// this constructor takes on by choosing its own directories rather than
    /// keeping [`vfs_embed::Session::new`]'s. That crate's
    /// `set_overlay` documents why: nothing deletes a session's overlay when
    /// its owner dies, and "the overlay is empty afterwards" is how this
    /// project detects a write that bypassed the director — so an inherited
    /// overlay either fails that check with nothing wrong or, worse, gets a
    /// real bypass dismissed as leftovers. `pid` plus a per-process counter
    /// makes the path this session's alone, so clearing it cannot destroy
    /// anything a caller put there.
    #[napi(constructor)]
    pub fn new(name: String) -> Result<Self> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "aethervfs-{}-{}-{seq}",
            slug(&name),
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);

        let mut inner = vfs_embed::Session::new();
        inner.set_root(base.join("root"));
        inner.set_overlay(base.join("overlay"));
        inner.set_state_dir(base.join("state"));

        Ok(Session {
            inner: Some(inner),
            name,
            base,
            roots: Vec::new(),
            shim_dll: None,
            payload_dll: None,
        })
    }

    fn get(&self) -> Result<&vfs_embed::Session> {
        self.inner
            .as_ref()
            .ok_or_else(|| Error::from_reason("session is closed"))
    }

    fn get_mut(&mut self) -> Result<&mut vfs_embed::Session> {
        self.inner
            .as_mut()
            .ok_or_else(|| Error::from_reason("session is closed"))
    }

    #[napi(getter)]
    pub fn name(&self) -> String {
        self.name.clone()
    }

    /// The directory tree holding this session's root, overlay and state.
    #[napi(getter)]
    pub fn base_dir(&self) -> String {
        self.base.to_string_lossy().into_owned()
    }

    /// Root 0's managed directory — what the injected child recognises *as*
    /// the virtual root.
    #[napi(getter)]
    pub fn virtual_root(&self) -> Result<String> {
        Ok(self.get()?.virtual_root().to_string_lossy().into_owned())
    }

    #[napi(getter)]
    pub fn state_dir(&self) -> Result<String> {
        Ok(self.get()?.state_dir().to_string_lossy().into_owned())
    }

    /// Where root `root`'s shim-local overlay writes actually land on disk.
    /// Root-scoped, so this is *not* `baseDir/overlay` — a host mounting its
    /// own read layer over the overlay must use exactly this path.
    #[napi]
    pub fn overlay_layer_dir(&self, root: u32) -> Result<String> {
        Ok(self
            .get()?
            .overlay_layer_dir(RootId(root))
            .to_string_lossy()
            .into_owned())
    }

    /// Declare that `RootId(id)` virtualizes the host directory `path`.
    ///
    /// `name` is a label for diagnostics; the director addresses roots by id.
    /// Re-declaring an id replaces its path and label.
    ///
    /// **Id 0 is the managed root itself** — declaring it repoints
    /// `virtualRoot`, which is what a host walking all of its roots means.
    ///
    /// Declaring is not mounting: this says which real location the child
    /// should recognise as root `id`, while `mount` says what that root
    /// serves. Declare without mounting and the root serves nothing; mount
    /// without declaring and the child never classifies any path into that
    /// root, so every path under it falls through to real disk — silently.
    #[napi]
    pub fn add_root(&mut self, id: u32, name: String, path: String) -> Result<()> {
        self.get_mut()?.declare_root(id, &path);
        let record = RootInfo {
            id,
            name,
            path: PathBuf::from(&path).to_string_lossy().into_owned(),
        };
        match self.roots.iter_mut().find(|r| r.id == id) {
            Some(slot) => *slot = record,
            None => self.roots.push(record),
        }
        Ok(())
    }

    /// The roots this session has declared, in declaration order.
    #[napi]
    pub fn roots(&self) -> Vec<RootInfo> {
        self.roots
            .iter()
            .map(|r| RootInfo {
                id: r.id,
                name: r.name.clone(),
                path: r.path.clone(),
            })
            .collect()
    }

    /// Mount `provider` on root `root`, optionally under `prefix` within it.
    ///
    /// Accumulates: later mounts win on a path both serve. Each call
    /// recomposes that root whole, because the director holds exactly one
    /// provider per root.
    #[napi]
    pub fn mount(&self, root: u32, provider: &Provider, prefix: Option<String>) -> Result<()> {
        let backend = lookup_provider(provider.handle)?;
        self.get()?
            .mount_at(RootId(root), prefix.as_deref().unwrap_or(""), backend)
            .map_err(|st| status_err("mount", st))
    }

    /// Start the ring and its workers so an injected child can remap I/O.
    /// Idempotent. [`Session::launch`] calls this if it has not happened.
    #[napi]
    pub fn serve(&mut self) -> Result<()> {
        self.get_mut()?.serve().map_err(Error::from_reason)
    }

    #[napi]
    pub fn is_serving(&self) -> Result<bool> {
        Ok(self.get()?.is_serving())
    }

    /// Read a whole file out of root 0's graph, host-side. Not the primary
    /// path — the child's reads go over the ring — but it is how a host proves
    /// its graph serves what it thinks it does without launching anything.
    ///
    /// **It drives the graph on the calling thread**, so it is also the call
    /// that trips the deadlock guard when a host mounts a provider serviced by
    /// the very loop it is calling from. That is deliberate: the failure is
    /// reported here, immediately and with an explanation, instead of hanging.
    #[napi]
    pub fn read_file(&self, vpath: String) -> Result<Buffer> {
        jsprovider::clear_diagnosis();
        self.get()?
            .read_file(&vpath)
            .map(Buffer::from)
            .map_err(|st| status_err(&format!("readFile({vpath:?})"), st))
    }

    /// Point this session at a specific shim (and optionally payload) DLL, for
    /// a host running against a dev build rather than the packaged DLLs. Takes
    /// precedence over the package directory; a per-launch `shimDll` still
    /// wins over this.
    #[napi]
    pub fn set_shim_dlls(&mut self, shim_dll: String, payload_dll: Option<String>) {
        self.shim_dll = Some(shim_dll);
        self.payload_dll = payload_dll;
    }

    /// Which DLLs a launch would use right now, with size and mtime. Throws
    /// with every location it searched if they cannot be found — which is the
    /// whole reason this exists as a separate call: a host can check before it
    /// launches a game rather than after.
    #[napi]
    pub fn shim_info(&self) -> Result<ShimInfo> {
        let (shim, payload) = self.resolve_dlls(None, None)?;
        let stat = |p: &str| -> (f64, f64) {
            match std::fs::metadata(p) {
                Ok(m) => {
                    let ms = m
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as f64)
                        .unwrap_or(0.0);
                    (m.len() as f64, ms)
                }
                Err(_) => (0.0, 0.0),
            }
        };
        let (shim_size, shim_modified_ms) = stat(&shim);
        let (payload_size, payload_modified_ms) = stat(&payload);
        Ok(ShimInfo {
            shim_dll: shim,
            payload_dll: payload,
            shim_size,
            shim_modified_ms,
            payload_size,
            payload_modified_ms,
        })
    }

    /// Resolve the shim and payload DLLs, in precedence order: the per-launch
    /// override, then [`Session::set_shim_dlls`], then the package directory
    /// [`set_package_dir`] recorded.
    ///
    /// The error path lists every candidate, because the failure this replaces
    /// — `vfs-embed` searching next to `node.exe` — reports a missing DLL
    /// without saying where it looked.
    fn resolve_dlls(
        &self,
        opt_shim: Option<&str>,
        opt_payload: Option<&str>,
    ) -> Result<(String, String)> {
        let pkg = PACKAGE_DIR
            .lock()
            .map_err(|_| Error::from_reason("package dir lock poisoned"))?
            .clone();

        /// First candidate that is a file; every candidate rejected on the way
        /// is recorded, so the error can say where it looked.
        fn pick(candidates: Vec<PathBuf>, tried: &mut Vec<String>) -> Option<PathBuf> {
            for c in candidates {
                if c.is_file() {
                    return Some(c);
                }
                tried.push(c.to_string_lossy().into_owned());
            }
            None
        }

        let mut tried: Vec<String> = Vec::new();
        let mut shim_candidates: Vec<PathBuf> = Vec::new();
        if let Some(s) = opt_shim {
            shim_candidates.push(PathBuf::from(s));
        }
        if let Some(s) = &self.shim_dll {
            shim_candidates.push(PathBuf::from(s));
        }
        if let Some(d) = &pkg {
            shim_candidates.push(d.join("vfs_shim_dll.dll"));
        }
        let shim = pick(shim_candidates, &mut tried).ok_or_else(|| {
            Error::from_reason(format!(
                "vfs_shim_dll.dll not found. Tried: {}. An addon cannot discover \
                 it — vfs-embed's own fallback searches next to \
                 std::env::current_exe(), which here is node.exe. Run \
                 `npm run build` in the package directory to put the DLLs \
                 beside the addon, pass `shimDll` to launch(), or call \
                 session.setShimDlls(...).{}",
                if tried.is_empty() {
                    "nothing (no package directory recorded)".to_string()
                } else {
                    tried.join(", ")
                },
                if pkg.is_none() {
                    " Note: setPackageDir() was never called, so the package \
                     directory was not searched — load the addon through \
                     index.cjs rather than requiring the .node file directly."
                } else {
                    ""
                }
            ))
        })?;

        // A caller that names the shim and not the payload means "the pair
        // that ships together", so look beside the shim before the package.
        let mut payload_candidates: Vec<PathBuf> = Vec::new();
        if let Some(p) = opt_payload {
            payload_candidates.push(PathBuf::from(p));
        }
        if let Some(p) = &self.payload_dll {
            payload_candidates.push(PathBuf::from(p));
        }
        if let Some(d) = shim.parent() {
            payload_candidates.push(d.join("vfs_payload.dll"));
        }
        if let Some(d) = &pkg {
            payload_candidates.push(d.join("vfs_payload.dll"));
        }
        let mut payload_tried: Vec<String> = Vec::new();
        let payload = pick(payload_candidates, &mut payload_tried).ok_or_else(|| {
            Error::from_reason(format!(
                "vfs_payload.dll not found. Tried: {}. It is built from a \
                 separate cargo workspace (crates/vfs-payload, panic = \"abort\") \
                 and `npm run build` builds it; a plain \
                 `cargo build --workspace` does not.",
                payload_tried.join(", ")
            ))
        })?;

        Ok((
            shim.to_string_lossy().into_owned(),
            payload.to_string_lossy().into_owned(),
        ))
    }

    /// Launch `exe` under the virtual root with the shim injected. Returns the
    /// child's exit code (0 immediately when `wait: false`).
    ///
    /// Serves first if the session is not already serving.
    ///
    /// ## How `exe` is resolved
    ///
    /// An **absolute** path is launched as given. A **relative** name is
    /// joined onto the managed root and launched from there if a real file
    /// exists; failing that it is looked up as a vpath in root 0's provider
    /// graph, and if the graph holds it, the image and its PE import closure
    /// are written to `stateDir/stage` and *that* is launched, with the staging
    /// directory mounted back into the graph below everything the host
    /// mounted. A name neither holds is refused by name.
    ///
    /// **This is a blocking call** with `wait: true`: it occupies the calling
    /// JS thread for the child's lifetime. That is fine for a script and wrong
    /// for an Electron main process, which should call it from a worker. An
    /// `AsyncTask` form belongs with the threading work, not here.
    #[napi]
    pub fn launch(&mut self, exe: String, options: Option<LaunchOptions>) -> Result<i32> {
        // Resolving a relative image looks it up as a vpath in root 0's graph on
        // *this* thread, so launch can trip the deadlock guard exactly as
        // `readFile` can. Same treatment.
        jsprovider::clear_diagnosis();
        if !self.get()?.is_serving() {
            self.serve()?;
        }
        let o = options.unwrap_or(LaunchOptions {
            args: None,
            wait: None,
            shim_dll: None,
            payload_dll: None,
            stage_also: None,
            stage_fallback_dirs: None,
            env: None,
        });
        let (shim, payload) =
            self.resolve_dlls(o.shim_dll.as_deref(), o.payload_dll.as_deref())?;

        let opts = LaunchOpts {
            image: exe,
            args: o.args.unwrap_or_default(),
            wait: o.wait.unwrap_or(true),
            stage_also: o.stage_also.unwrap_or_default(),
            stage_fallback_dirs: o
                .stage_fallback_dirs
                .unwrap_or_default()
                .into_iter()
                .map(PathBuf::from)
                .collect(),
            shim_dll: Some(shim),
            payload_dll: Some(payload),
            env: o
                .env
                .unwrap_or_default()
                .into_iter()
                .collect::<BTreeMap<String, String>>(),
        };
        self.get()?
            .launch(&opts)
            .map_err(|e| reason_err("launch", e))
    }

    /// Every write refused because no read-write provider served that path.
    ///
    /// Spec §7's discovery workflow: launch, ask what was rejected, add an
    /// overlay for those subtrees.
    ///
    /// **Process-wide, not per session.** The director keeps one global table
    /// with no session or root dimension, so two live sessions in one host
    /// report each other's rejections.
    #[napi]
    pub fn rejected_writes(&self) -> Result<Vec<RejectedWrite>> {
        self.get()?;
        Ok(vfs_embed::rejected_writes()
            .into_iter()
            .map(|(path, count)| RejectedWrite {
                path,
                count: count as f64,
            })
            .collect())
    }

    /// Clear rejected-write tracking. Process-wide, same caveat as
    /// [`Session::rejected_writes`] — useful before a probe.
    #[napi]
    pub fn reset_rejected_writes(&self) -> Result<()> {
        self.get()?;
        vfs_embed::reset_rejected_writes();
        Ok(())
    }

    /// Opens that reached the director, as `[succeeded, failed]`. Compared
    /// against the shim's own classification, this answers "did anything under
    /// the managed root get served by real disk behind my back". Process-wide.
    #[napi]
    pub fn open_totals(&self) -> Result<Vec<f64>> {
        self.get()?;
        let (ok, failed) = vfs_embed::open_totals();
        Ok(vec![ok as f64, failed as f64])
    }

    /// Stop serving and drop the session.
    ///
    /// Dropping the Rust session is what removes the staged launch directory,
    /// so this is the deterministic teardown; without it that happens whenever
    /// the JS object is collected. Idempotent.
    ///
    /// **The session's directories are left in place** — `baseDir` and the
    /// overlay under it stay readable afterwards, because "the overlay is
    /// empty afterwards" is a check this project runs on a finished session. A
    /// host that wants them gone removes `baseDir` itself; deleting a caller's
    /// directories from a teardown call is not this binding's decision.
    #[napi]
    pub fn close(&mut self) -> Result<()> {
        if let Some(mut inner) = self.inner.take() {
            inner.stop_serve();
        }
        Ok(())
    }
}

/// Stop the ring if a host forgot to `close()`, so a collected session does not
/// leave worker threads running. `close()` has already taken `inner` in the
/// ordinary path, which makes this a no-op then.
impl Drop for Session {
    fn drop(&mut self) {
        if let Some(mut inner) = self.inner.take() {
            inner.stop_serve();
        }
    }
}

/// The `ST_*` status codes, as an object, so a JS caller can compare against a
/// name instead of a magic number.
#[napi]
pub fn status_codes() -> HashMap<String, i32> {
    [
        ("ST_OK", vfs_embed::ST_OK),
        ("ST_NOT_FOUND", vfs_embed::ST_NOT_FOUND),
        ("ST_IO_ERROR", vfs_embed::ST_IO_ERROR),
        ("ST_NOT_SUPPORTED", vfs_embed::ST_NOT_SUPPORTED),
        ("ST_BAD_FH", vfs_embed::ST_BAD_FH),
        ("ST_IS_DIR", vfs_embed::ST_IS_DIR),
        ("ST_NOT_A_DIRECTORY", vfs_embed::ST_NOT_A_DIRECTORY),
        ("ST_READ_ONLY", vfs_embed::ST_READ_ONLY),
        ("ST_EXISTS", vfs_embed::ST_EXISTS),
        ("ST_NO_SPACE", vfs_embed::ST_NO_SPACE),
        ("ST_BAD_REQUEST", vfs_embed::ST_BAD_REQUEST),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

/// Sanity check for `require('aethervfs')`: the addon is loaded and the Rust
/// side is answering. Deliberately does nothing else.
#[napi]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

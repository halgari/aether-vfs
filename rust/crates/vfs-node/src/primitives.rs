//! **Spec §6's primitive catalog, callable from JavaScript.**
//!
//! The claim §6 rests on is that a host writes *one* thing — its own data
//! source — and composes everything else out of Rust:
//!
//! > Python and TypeScript write none of these. They write novel data
//! > sources — a Steam CDN client, a mod-manager database — and compose the
//! > rest.
//!
//! So this module is the test of that claim, and the shape of the test is spec
//! §8's own composition, written in JavaScript instead of Python:
//!
//! ```js
//! const base = cached(seekable(cdn.provider), { ramBytes: 512 << 20 });
//! const inis = memory({ 'Skyrim.ini': iniBytes });
//! s.mount(0, layered(readonly(base), disk(modsDir)));
//! s.mount(1, router({ '*.ini': inis }, overlay(disk(docs), disk(scratch))));
//! ```
//!
//! Every name there but `cdn` is a Rust type. `readonly` and `seekable` were
//! **not** Rust types when this task started — see the report — and the fix was
//! to write them in `vfs-compose` where every host gets them, not here where
//! only Node would.
//!
//! ## Handles in, handle out
//!
//! Every combinator takes and returns the same process-global `u32` a
//! `Provider` wrapper carries (see [`crate`]'s module docs on why a handle is
//! an integer). `index.cjs` accepts a `Provider` object *or* a bare number and
//! unwraps to the integer, which is what lets a graph be composed on one thread
//! out of providers registered on another — the arrangement task 7 made
//! mandatory for a JS-authored leaf.
//!
//! ## What composition must not lose
//!
//! Task 7 established that `releaseProvider(handle)` is not hygiene: a live
//! threadsafe function keeps its loop alive, so a worker never exits until its
//! provider is released. Wrapping that provider in four combinators produces
//! four *new* handles, none of which is the one that has to be released — and
//! `Provider.stats()` on any of them is `null`, because a combinator has no
//! bridge. That is a genuine trap, so every constructor here records its
//! children ([`children`]), and [`js_leaves`] walks that graph to answer "which
//! handles in this composition are JS-backed and therefore have a loop to
//! release". `releaseProvider` on a combinator still fails, and now the host has
//! a way to find the handle it should have called it with instead.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use napi::bindgen_prelude::Buffer;
use napi::{Error, Result};
use napi_derive::napi;

use vfs_embed::{
    Access, BlockCache, CacheConfig, CachingProvider, DiskProvider, LayeredProvider, MemoryProvider,
    OverlayProvider, Provider as VfsProvider, ReadOnlyProvider, Route, RouterProvider,
    SeekableProvider,
};

use crate::{intern_provider, lookup_provider, Provider};

// ---------------------------------------------------------------------------
// What each handle is, and what it was built out of. Diagnostics, plus the two
// questions a host cannot otherwise answer: "which of these has a loop to
// release" and "am I double-caching".
// ---------------------------------------------------------------------------

type Registry<T> = OnceLock<RwLock<HashMap<u32, T>>>;

static KINDS: Registry<&'static str> = OnceLock::new();
static CHILDREN: Registry<Vec<u32>> = OnceLock::new();
/// A `cached(...)` handle's cache and the block size it actually settled on
/// (which is the wrapped provider's `preferredBlock` when it declares one, not
/// the cache's own).
static CACHES: Registry<(Arc<BlockCache>, u64)> = OnceLock::new();

fn table<T>(t: &'static Registry<T>) -> &'static RwLock<HashMap<u32, T>> {
    t.get_or_init(|| RwLock::new(HashMap::new()))
}

fn note_kind(handle: u32, kind: &'static str, children: Vec<u32>) {
    if let Ok(mut g) = table(&KINDS).write() {
        g.insert(handle, kind);
    }
    if !children.is_empty() {
        if let Ok(mut g) = table(&CHILDREN).write() {
            g.insert(handle, children);
        }
    }
}

/// What kind of provider `handle` is, as the constructor that made it named
/// itself. `None` for a handle interned before this table existed — only
/// [`crate::disk`] and [`crate::jsprovider::register_provider`] can produce one,
/// and both record themselves.
pub(crate) fn kind_of(handle: u32) -> Option<&'static str> {
    table(&KINDS).read().ok()?.get(&handle).copied()
}

/// The handles a combinator was built from, in argument order. Empty for a leaf.
pub(crate) fn children(handle: u32) -> Vec<u32> {
    table(&CHILDREN)
        .read()
        .ok()
        .and_then(|g| g.get(&handle).cloned())
        .unwrap_or_default()
}

/// Every JS-backed provider reachable from `handle`, including `handle` itself.
///
/// This is the list a host must call `releaseProvider` on — see the module docs.
/// Depth-first in argument order, deduplicated, and terminating by construction:
/// a child's handle is always smaller than its parent's, because the child had
/// to be interned first.
pub(crate) fn js_leaves(handle: u32) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::new();
    let mut stack = vec![handle];
    while let Some(h) = stack.pop() {
        let kids = children(h);
        if kids.is_empty() {
            if crate::jsprovider::stats_for(h).is_some() && !out.contains(&h) {
                out.push(h);
            }
            continue;
        }
        // Reversed, so the pop order is argument order.
        for k in kids.into_iter().rev() {
            stack.push(k);
        }
    }
    out
}

/// The `BlockCache` behind a `cached(...)` handle and its effective block size,
/// for [`cache_stats_for`].
fn cache_for(handle: u32) -> Option<(Arc<BlockCache>, u64)> {
    table(&CACHES).read().ok()?.get(&handle).cloned()
}

/// Intern a composed provider and record what it is and what it wraps.
fn compose(
    p: Arc<dyn VfsProvider>,
    kind: &'static str,
    children: Vec<u32>,
) -> Result<Provider> {
    let handle = intern_provider(p)?;
    note_kind(handle, kind, children);
    Ok(Provider::wrap(handle))
}

/// Record a leaf's kind. Called by the leaf constructors in [`crate`] and
/// [`crate::jsprovider`], which intern through [`intern_provider`] directly
/// because they build their provider before they know its handle.
pub(crate) fn note_leaf(handle: u32, kind: &'static str) {
    note_kind(handle, kind, Vec::new());
}

// ---------------------------------------------------------------------------
// Leaves.
// ---------------------------------------------------------------------------

/// One file in a `memory(...)` provider.
#[napi(object)]
pub struct MemoryFile {
    /// Forward or backslash separated; normalized like every other vpath.
    pub path: String,
    pub bytes: Buffer,
}

/// A read-write in-memory file tree (spec §6's `memory` primitive).
///
/// The round trip is the reason it exists, and it is spec §8's own example: a
/// host hands in `{'skyrim.ini': bytes}`, the game writes the file, and the host
/// reads back what the game wrote — with nothing touching disk. Reading it back
/// goes through the graph (`session.readFile`), because a `Provider` handle is
/// an integer and not an object with its own file API.
///
/// **Fold the keys.** The shim folds every vpath component before it crosses the
/// ring and this provider is case-sensitive by design, so a host that seeds
/// `Skyrim.ini` and a child that writes `Skyrim.ini` end up with **two** files —
/// with no error from the child, from `rejectedWrites()`, or from disk. Spec
/// §6b's `casefold` primitive is the fix and does not exist yet;
/// `examples/spec-8-example.cts` demonstrates the working round trip and the
/// silent wrong answer side by side.
///
/// Declares `Access::ReadWrite`, so it can serve as an `overlay` upper or a
/// `router` target for exactly the paths a host wants writable.
#[napi(catch_unwind)]
pub fn memory(files: Option<Vec<MemoryFile>>) -> Result<Provider> {
    let p = MemoryProvider::from_files(
        files
            .unwrap_or_default()
            .into_iter()
            .map(|f| (f.path, f.bytes.to_vec())),
    );
    compose(Arc::new(p), "memory", Vec::new())
}

// ---------------------------------------------------------------------------
// Combinators.
// ---------------------------------------------------------------------------

/// Demote a provider to read-only (spec §6's `readonly`).
///
/// Two things happen and both matter. The *declaration* becomes `Read`, which is
/// what `Director::open` and `MountGraph::open` consult — and therefore what
/// makes a refused write land in `session.rejectedWrites()` for spec §7's
/// discovery workflow. The *behaviour* refuses every mutating call, so a caller
/// holding a handle cannot write through it either.
///
/// **This is the only way to get a non-empty `rejectedWrites()` from Node.**
/// `disk()` is `ReadWrite`, so a graph built from `disk` alone can never refuse
/// a write; §7's whole workflow was undemonstrable from a host until this
/// existed.
#[napi(catch_unwind)]
pub fn readonly(provider: u32) -> Result<Provider> {
    let inner = lookup_provider(provider)?;
    compose(
        Arc::new(ReadOnlyProvider::new(inner)),
        "readonly",
        vec![provider],
    )
}

/// Give a forward-only provider positional reads (spec §6's `seekable`).
///
/// `SeqRead` becomes `Read`. A sequential provider that is *not* wrapped in this
/// cannot be mounted at all — see [`crate::Session::mount`], which refuses it
/// with the reason — because the director's read path is
/// `read_at(handle, offset, buf)` and a forward-only source has no answer for
/// it.
///
/// Wrapping an already-positional provider is a no-op passthrough rather than an
/// error, so a host applying the recommended wrapping uniformly pays nothing
/// where it is unnecessary.
#[napi(catch_unwind)]
pub fn seekable(provider: u32) -> Result<Provider> {
    let inner = lookup_provider(provider)?;
    compose(
        Arc::new(SeekableProvider::new(inner)),
        "seekable",
        vec![provider],
    )
}

/// Options for [`cached`]. Every field is optional; the defaults are
/// `vfs_embed::CacheConfig`'s (1 MiB blocks, a 64 MiB RAM budget, no disk tier).
#[napi(object)]
pub struct CacheOptions {
    /// RAM budget for block payloads, in bytes. Default 64 MiB.
    pub ram_bytes: Option<f64>,
    /// Block size in bytes. Default 1 MiB — and **overridden** by the wrapped
    /// provider's own `preferredBlock` when it declares one, clamped to
    /// [4 KiB, 4 MiB]. A source that states its natural unit knows better than
    /// its caller does.
    pub block_size: Option<f64>,
    /// Directory for the on-disk block tier. Unset means RAM only.
    ///
    /// Only worth setting for a provider that declares **both** `immutable` and
    /// `slow`: `immutable` is what makes persisting blocks across sessions
    /// sound, and `slow` is what makes it worth doing.
    pub disk_dir: Option<String>,
}

/// Put a block cache in front of a provider (spec §6's `cached`).
///
/// Access passes through and `slow` is cleared, which is not merely
/// bookkeeping: `slow` surviving a `cached` wrapper is exactly how
/// [`crate::Session::mount`] tells a host it forgot the cache, so the flag has
/// to mean "nothing is caching this" rather than "the source is expensive".
///
/// Each call gets its own `BlockCache`, keyed by the wrapped provider's handle,
/// and `provider.cacheStats()` reports its hits and misses — otherwise "I added
/// a cache" is an act of faith rather than a measurement.
#[napi(catch_unwind)]
pub fn cached(provider: u32, options: Option<CacheOptions>) -> Result<Provider> {
    let inner = lookup_provider(provider)?;
    let o = options.unwrap_or(CacheOptions {
        ram_bytes: None,
        block_size: None,
        disk_dir: None,
    });
    let default = CacheConfig::default();
    let cfg = CacheConfig {
        block_size: o
            .block_size
            .filter(|b| *b >= 1.0)
            .map(|b| b as u64)
            .unwrap_or(default.block_size),
        ram_budget: o
            .ram_bytes
            .filter(|b| *b >= 0.0)
            .map(|b| b as u64)
            .unwrap_or(default.ram_budget),
        disk_dir: o.disk_dir.map(std::path::PathBuf::from),
    };

    // Spec §6: "Nested `cached` inside `cached` — collapsed, not doubled."
    // Nothing in the workspace collapses them, so say so rather than let a host
    // pay twice for one read and never find out.
    if kind_of(provider) == Some("cached") {
        eprintln!(
            "aethervfs: cached() over a provider that is already cached (handle \
             {provider}). Spec §6 says nested caches are collapsed rather than \
             doubled; `CachingProvider` does not collapse them, so this graph \
             holds two independent caches of the same bytes and every miss fills \
             both. Cache once, at the layer nearest the slow source."
        );
    }

    let cache = Arc::new(BlockCache::new(cfg));
    let p = CachingProvider::new(inner, Arc::clone(&cache), u64::from(provider));
    let effective_block = p.block_size();
    let out = compose(Arc::new(p), "cached", vec![provider])?;
    if let Ok(mut g) = table(&CACHES).write() {
        g.insert(out.handle(), (cache, effective_block));
    }
    Ok(out)
}

/// Stack providers so a **later** argument wins on a path several of them serve
/// (spec §6's `layered`).
///
/// `layered(a, b, c)` is `a` at the bottom and `c` on top, which is the order
/// spec §8's own example needs — `layered(readonly(base), disk(mods))` has to
/// let the mod win over the vanilla file, or it is not a mod manager. `readdir`
/// unions across the stack with the same top-wins rule per name.
///
/// Access is the **strongest** child's, not the weakest: every write op routes
/// to whichever child declares `ReadWrite`, so a stack containing one writable
/// child can serve a write. `immutable`, `slow` and `preferredBlock` combine
/// conservatively.
#[napi(catch_unwind)]
pub fn layered(providers: Vec<u32>) -> Result<Provider> {
    if providers.len() < 2 {
        return Err(Error::from_reason(format!(
            "layered() needs at least two providers; got {}. A one-provider stack \
             is the provider itself, and an empty one serves nothing — both are \
             more likely a bug in how the list was built than an intent.",
            providers.len()
        )));
    }
    let mut iter = providers.iter().copied();
    let bottom = iter.next().expect("length checked above");
    let mut acc = lookup_provider(bottom)?;
    for h in iter {
        let upper = lookup_provider(h)?;
        acc = Arc::new(LayeredProvider::new(upper, acc));
    }
    compose(acc, "layered", providers)
}

/// Copy-up writes and whiteouts over an immutable base (spec §6's `overlay`).
///
/// Reports `ReadWrite` regardless of what `base` declares, which is the point:
/// a write to a path only `base` holds copies the whole file into `upper` first,
/// so an in-place edit of read-only content succeeds instead of being refused.
/// Removing a base-visible path writes a `.wh.<name>` marker into `upper`
/// rather than touching `base`.
///
/// `upper` must declare `ReadWrite` — checked here, not at the first write.
#[napi(catch_unwind)]
pub fn overlay(base: u32, upper: u32) -> Result<Provider> {
    let base_p = lookup_provider(base)?;
    let upper_p = lookup_provider(upper)?;
    let access = upper_p.capabilities().access;
    let p = OverlayProvider::from_arcs(base_p, upper_p).map_err(|e| {
        Error::from_reason(format!(
            "overlay(base, upper): {e} — the upper declares {access:?}. An overlay's \
             upper is where every write lands, so a read-only upper makes the \
             whole overlay a read-only provider that claims to be writable. \
             Refused here rather than at the first write, which happens inside an \
             injected process. `disk()` and `memory()` are both ReadWrite; \
             `readonly()` is not."
        ))
    })?;
    compose(Arc::new(p), "overlay", vec![base, upper])
}

/// One `router` route: a glob and the provider that owns matching paths.
#[napi(object)]
pub struct RouteSpec {
    /// A glob over the vpath, `*`/`**`/`?`, matched case-insensitively with the
    /// same fold the shim applies. **`*` does not cross a `/`**, so `'*.ini'`
    /// matches `Skyrim.ini` at the mount's own root and not `sub/Skyrim.ini`;
    /// use `'**/*.ini'` for the whole subtree.
    pub pattern: String,
    pub provider: u32,
}

/// Dispatch by glob to a provider, falling back to `default` (spec §6's
/// `router`).
///
/// First matching route wins, in the order given. `getattr` and `open` are
/// single-dispatch.
///
/// **`readdir` is not.** Spec §6 specifies a union across the default plus every
/// route that could contribute to a directory; `RouterProvider` still returns
/// only the answering child's listing, which is Stage 1's documented gap. The
/// consequence for a host is concrete: a file served by a route is *readable* by
/// name and *invisible* to a directory listing. A game that enumerates a
/// directory to find its INIs will not see one supplied by a `'*.ini'` route.
/// Until that is fixed, put such a file in the default provider (or in an
/// `overlay` upper) if anything enumerates it.
#[napi(catch_unwind)]
pub fn router(routes: Vec<RouteSpec>, default_provider: u32) -> Result<Provider> {
    let def = lookup_provider(default_provider)?;
    let mut kids = vec![default_provider];
    let mut built: Vec<Route> = Vec::with_capacity(routes.len());
    for r in routes {
        if r.pattern.trim().is_empty() {
            return Err(Error::from_reason(
                "router(): a route pattern must not be empty. An empty glob \
                 matches nothing, so the route would be dead weight that looks \
                 like configuration.",
            ));
        }
        kids.push(r.provider);
        built.push(Route {
            pattern: r.pattern,
            provider: lookup_provider(r.provider)?,
        });
    }
    compose(
        Arc::new(RouterProvider::new(def, built)),
        "router",
        kids,
    )
}

// ---------------------------------------------------------------------------
// Reading a composed graph back, from JS.
// ---------------------------------------------------------------------------

/// A provider's declared capabilities, as `provider.capabilities()` reports
/// them.
///
/// Spec §6 lists the recomputation rules a combinator must follow — *"`seekable`
/// over `SeqRead` reports `Read`", "`cached` passes access through and clears
/// `slow`", "`overlay` reports `ReadWrite` regardless of base", "`readonly`
/// clamps access to `Read`"* — and until this existed a host had no way to check
/// any of them. Every one is now assertable from JavaScript against the same
/// `Capabilities` the director reads.
#[napi(object)]
pub struct ProviderCapabilities {
    /// `'seqread'`, `'read'` or `'readwrite'`.
    pub access: String,
    pub immutable: bool,
    /// Reads are expensive and this provider should sit behind `cached`. Cleared
    /// by `cached`, which is what makes `mount`'s warning exact rather than
    /// heuristic.
    pub slow: bool,
    pub preferred_block: Option<u32>,
}

pub(crate) fn access_name(a: Access) -> &'static str {
    match a {
        Access::SeqRead => "seqread",
        Access::Read => "read",
        Access::ReadWrite => "readwrite",
    }
}

pub(crate) fn capabilities_of(handle: u32) -> Result<ProviderCapabilities> {
    let c = lookup_provider(handle)?.capabilities();
    Ok(ProviderCapabilities {
        access: access_name(c.access).to_string(),
        immutable: c.immutable,
        slow: c.slow,
        preferred_block: c.preferred_block,
    })
}

/// Block-cache counters for a `cached(...)` provider.
#[napi(object)]
pub struct ProviderCacheStats {
    pub hits: f64,
    pub misses: f64,
    pub ram_evicts: f64,
    pub disk_hits: f64,
    pub disk_writes: f64,
    pub bytes_from_cache: f64,
    pub bytes_from_source: f64,
    pub ram_bytes: f64,
    pub ram_blocks: f64,
    /// The block size actually in use, after the wrapped provider's
    /// `preferredBlock` and the [4 KiB, 4 MiB] clamp.
    pub block_size: f64,
}

pub(crate) fn cache_stats_for(handle: u32) -> Option<ProviderCacheStats> {
    let (cache, block_size) = cache_for(handle)?;
    let s = cache.stats();
    Some(ProviderCacheStats {
        hits: s.hits as f64,
        misses: s.misses as f64,
        ram_evicts: s.ram_evicts as f64,
        disk_hits: s.disk_hits as f64,
        disk_writes: s.disk_writes as f64,
        bytes_from_cache: s.bytes_from_cache as f64,
        bytes_from_source: s.bytes_from_source as f64,
        ram_bytes: s.ram_bytes as f64,
        ram_blocks: s.ram_blocks as f64,
        block_size: block_size as f64,
    })
}

/// A read-write provider over a real directory (spec §6's `disk` primitive).
///
/// Lives here with the rest of the catalog; the directory-must-exist check and
/// the reason for it are in the implementation.
pub(crate) fn disk_provider(path: &std::path::Path) -> Result<Provider> {
    let handle = intern_provider(Arc::new(DiskProvider::new(path)))?;
    note_leaf(handle, "disk");
    Ok(Provider::wrap(handle))
}

//! **The public embeddable API.** Session lifecycle, roots, composition,
//! launch — design spec §4.
//!
//! Everything above this crate is a *host*: `vfs.exe` and its daemon, the Node
//! binding, the Python binding after it. Everything below it is the engine:
//! the [`Director`] kernel, the provider contract, and the composition
//! primitives. A host is expected to name **only this crate**; if a host has
//! to `use vfs_director::…` or `use vfs_directord::…` to get something done,
//! the seam is in the wrong place and the fix belongs here rather than in the
//! host.
//!
//! ```no_run
//! use std::sync::Arc;
//! use vfs_embed::{DiskProvider, LaunchOpts, Session};
//!
//! # fn main() -> Result<(), String> {
//! let mut session = Session::new();
//! session.set_root(r"C:\vfs\root");
//! session.declare_root(1, r"C:\Users\me\Documents\My Games\Skyrim");
//!
//! // A composed graph: read-only content, with a writable upper over it, so
//! // an in-place edit of that content copies up instead of failing.
//! session
//!     .mount("", Arc::new(DiskProvider::new(r"C:\game")))
//!     .map_err(|st| format!("mount: status {st}"))?;
//! session
//!     .set_write_layer(Arc::new(DiskProvider::new(r"C:\scratch")))
//!     .map_err(|st| format!("write layer: status {st}"))?;
//!
//! session.serve()?;
//! // A relative image is looked for on real disk under the managed root and
//! // then as a vpath in the graph. The second case — an exe that is content,
//! // in a managed root that is deliberately empty — is staged out with its PE
//! // import closure and launched from there; see [`Session::launch`].
//! session.launch(&LaunchOpts {
//!     image: "SkyrimSE.exe".into(),
//!     ..Default::default()
//! })?;
//! # Ok(())
//! # }
//! ```
//!
//! ## What this crate owns, and what it deliberately does not
//!
//! It owns **one session**: its roots, the provider graph each root serves,
//! the ring the injected shim talks over, and the launch. It does not own a
//! *table* of sessions, a control plane, or a config file format. The daemon
//! in `vfs-directord` keeps those, because they are properties of that
//! particular host rather than of embedding — a Node host addresses its
//! sessions with JavaScript object references, not with `"s1"` strings over
//! gRPC, and composes its graph from code rather than from TOML (spec §6:
//! "Config is a serialization of the graph, not the other way round").
//!
//! ## Composition
//!
//! [`compose_root`] is the single function in the workspace that turns "these
//! sources, that write layer" into the one provider a root serves, and every
//! surface funnels through it. Read its doc comment before composing a graph
//! by hand: a sibling mount and an overlay upper are not interchangeable, and
//! getting that wrong produces a session that reads correctly and silently
//! cannot be written to.
//!
//! [`RootSources`] is the incremental form of the same rule, for a host that
//! learns about its sources one at a time (a config being applied, an RPC
//! stream, a UI) and must rebuild a root's mount list from scratch each time.
//!
//! ## Launching content
//!
//! A managed root is normally **empty on disk** — the game is in the graph.
//! `CreateProcess` cannot create a process from bytes, and the Windows loader
//! resolves the image's static imports before any hook of ours exists in the
//! child, so [`Session::launch`] writes a graph-resolved image and its PE
//! import closure out to `state_dir/stage` and launches that. The staging
//! directory is mounted back into the graph so the same file still answers at
//! its vpath, and **below** everything the host mounted, so a point-in-time
//! staged copy can never shadow curated content. The session holds the staged
//! directory alive for the child's lifetime.
//!
//! A host does not have to know any of that: `launch("SkyrimSE.exe")` is
//! enough. [`Session::stage_launch`] is the same sequence exposed, for seeding
//! staging from something other than this session's graph.
//!
//! ## What a host still has to build for itself
//!
//! Written down because the alternative is each new binding rediscovering it.
//!
//! * **Locate its own `vfs_shim_dll.dll` / `vfs_payload.dll`.** Left unset,
//!   [`LaunchOpts::shim_dll`] searches next to `std::env::current_exe()`,
//!   which for a Node addon is `node.exe` and for a Python extension is
//!   `python.exe` — neither anywhere near the shipped DLLs. A binding must
//!   resolve both from its own module path and set them; they are effectively
//!   mandatory outside this workspace's own binaries.
//! * **Keep its threads away from `std::env`.** The child inherits its ring
//!   coordinates, so [`Session::serve`] and [`Session::launch`] write
//!   process-global `VFS_*` variables under a lock. The lock orders *our*
//!   writers and cannot order a host's: `std::env::set_var` is unsound in a
//!   multi-threaded process, and a Node or Python host is multi-threaded by
//!   construction. See [`Session::launch`] for what removing the hazard takes.
//! * **A `CachingProvider` per source, and never over the write layer** — see
//!   [`Session::set_write_layer_at`] for why the exemption is not optional.
//! * **Session directories that do not inherit a previous run's litter** — see
//!   [`Session::set_overlay`]. [`Session::new`]'s own defaults handle this for
//!   themselves; a host that calls `set_root`/`set_overlay`/`set_state_dir`
//!   takes it on.
//! * **Building a provider from a name** (`"disk"`, `"zip"`, `"remote"`)
//!   rather than calling its constructor. `vfs-source` does that and reaches
//!   the gRPC `remote` provider, which is not in the catalog below; it also
//!   pulls tonic, prost and a vendored `protoc`, which is why it is not a
//!   dependency here. Spec §6's `register_provider` is the intended answer and
//!   does not exist yet.

#![deny(unsafe_code)]

mod session;
mod sources;

pub use session::{compose_root, LaunchOpts, Session, StageOpts};
// `RootMounts` stays for the same reason `CacheStats` does below: it is
// `RootSources::mounts()`'s return type and `Session::set_root_mounts`'s
// argument type, so a host writing either signature needs the name. It has no
// *direct* consumer, which is a different thing from being unreachable.
pub use sources::{RootMounts, RootSources};

// ---------------------------------------------------------------------------
// The provider contract. Re-exported whole: a host writing a provider needs
// every one of these names, and should not have to learn which internal crate
// each one lives in.
// ---------------------------------------------------------------------------
pub use vfs_provider::{
    bad_fh, bad_request, exists, is_dir, map_io_err, not_a_dir, not_found, not_supported, ok,
    read_only, Access, Capabilities, CaseMatch, DirEntry, Handle, Provider, RootId, SetAttr, Stat,
    VPath, KIND_DIR, KIND_FILE, KIND_TOMBSTONE, OPEN_APPEND, OPEN_CREATE, OPEN_EXCL, OPEN_READ,
    OPEN_TRUNC, OPEN_WRITE, ST_BAD_FH, ST_BAD_REQUEST, ST_EXISTS, ST_IO_ERROR, ST_IS_DIR,
    ST_NOT_A_DIRECTORY, ST_NOT_FOUND, ST_NOT_SUPPORTED, ST_NO_SPACE, ST_OK, ST_READ_ONLY,
};

// ---------------------------------------------------------------------------
// The conformance suite. Re-exported because spec §10's requirement is that a
// host-authored provider is held to *this* suite — "one conformance suite, run
// against every provider in every language" — and a binding that had to name
// `vfs_provider` to reach it would be reaching past the seam this crate exists
// to be. `write_fixture_tree` comes along because a disk-backed provider needs
// the reference tree on real disk to be conformance-testable at all, and
// `FIXTURE_FILES` because a host-language provider has to serve exactly that
// tree and must not hold a second, drifting copy of it.
// ---------------------------------------------------------------------------
pub use vfs_provider::{assert_conformance, write_fixture_tree, FIXTURE_FILES};

// ---------------------------------------------------------------------------
// Leaves and combinators (spec §6's primitive catalog). A host composes a
// graph out of these and its own providers; it writes none of them.
// ---------------------------------------------------------------------------
// `CacheStats` is on this list because `BlockCache::stats()` returns it and that
// method is re-exported — a host able to call a method but not to name what it
// returns is the seam leaking by omission. `DEFAULT_BLOCK_SIZE` and `MountGraph`
// were on it and are gone: neither appears in any re-exported signature, no
// consumer in or out of the workspace named either, and `Session`'s own
// composition already owns the `MountGraph` decision (a host hands mounts to
// `mount_at`/`set_root_mounts` and never builds one). Re-exporting a type nothing
// can reach is not neutral: it advertises a supported surface.
pub use vfs_cache::{BlockCache, CacheConfig, CacheStats, CachingProvider};
pub use vfs_compose::{
    stack_layers, InlineProvider, LayeredProvider, MemoryProvider, OverlayProvider, ReadOnlyProvider,
    Route, RouterProvider, SeekableProvider, SubdirProvider,
};
pub use vfs_director::DiskProvider;
#[cfg(feature = "zip")]
pub use vfs_zip::ZipProvider;

// ---------------------------------------------------------------------------
// The kernel, for the cases a host genuinely needs it: reading back through
// the same graph the injected process sees, and staging a launch image.
// ---------------------------------------------------------------------------
pub use vfs_director::stage;
pub use vfs_director::Director;
/// Where a root's shim-local overlay writes actually land on disk — see
/// [`Session::overlay_layer_dir`], which is the same thing bound to a session.
/// The free function exists for a host that must write into that directory
/// *before* a `Session` exists.
pub use vfs_director::overlay_layer_dir;

/// Every write refused because no `ReadWrite` provider served that path, as
/// `(path, count)`.
///
/// Spec §7 ("read-only rejection, made discoverable") makes this the workflow:
/// launch, ask what was rejected, add an overlay for those subtrees. Also
/// available as [`Session::rejected_writes`].
///
/// **Process-wide, not per session.** `vfs_director::io_stats` keeps one
/// global table with no session or root dimension, so two concurrent sessions
/// in one host report each other's rejections. That is the behaviour today and
/// is left unchanged here; a host with one live session — every host so far —
/// reads it correctly.
pub fn rejected_writes() -> Vec<(String, u64)> {
    vfs_director::io_stats::rejected_writes()
}

/// Clear rejected-write tracking. Process-wide, with the same caveat as
/// [`rejected_writes`]: useful before a probe, and used by tests.
pub fn reset_rejected_writes() {
    vfs_director::io_stats::reset_rejected_writes()
}

/// Opens that reached the director, as `(succeeded, failed)`.
///
/// The director-side half of the open reconciliation this project measures
/// with: the injected shim classifies every under-root open by which path it
/// took, and these are the ones that actually arrived here. A host comparing
/// the two numbers is asking "did anything under the managed root get served
/// by real disk behind my back", which is the question a VFS has to be able to
/// answer about itself. `vfs-directord` reports it on its `Stats` RPC; it is
/// here because a host without a control plane needs the same number and had
/// to name the kernel crate to get it.
///
/// **Process-wide**, with the same caveat as [`rejected_writes`]: no session or
/// root dimension.
pub fn open_totals() -> (u64, u64) {
    vfs_director::io_stats::open_totals()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use vfs_provider::{OPEN_WRITE, ST_BAD_REQUEST, ST_READ_ONLY};

    #[test]
    fn session_read_file_helper() {
        let dir = std::env::temp_dir().join(format!("vfs-sess-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("a.bin"), b"xyz").unwrap();
        let s = Session::new();
        s.mount("", Arc::new(DiskProvider::new(&dir))).unwrap();
        let got = s.read_file("a.bin").unwrap();
        assert_eq!(got, b"xyz");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Spec §6's hard error, and the positive control beside it: the *same*
    /// provider mounts once `seekable` is around it. Without the second half
    /// this test would also pass if `mount_at` refused everything.
    #[test]
    fn a_sequential_provider_is_refused_until_it_is_wrapped_in_seekable() {
        let seq: Arc<dyn Provider> = Arc::new(vfs_provider::conformance::SeqFixture::new());
        assert_eq!(seq.capabilities().access, Access::SeqRead);

        let s = Session::new();
        assert_eq!(
            s.mount("", Arc::clone(&seq)),
            Err(ST_BAD_REQUEST),
            "a SeqRead provider must not mount — every read through it would fail"
        );

        let wrapped: Arc<dyn Provider> = Arc::new(SeekableProvider::new(seq));
        assert_eq!(wrapped.capabilities().access, Access::Read);
        s.mount("", wrapped).expect("seekable(seq) mounts");
        assert_eq!(s.read_file("a.txt").unwrap(), b"hello");
    }

    /// The same hard error through the **other** mount entry point.
    ///
    /// `mount_at` checked this and `set_root_mounts` did not, so the gate did
    /// not exist for the surface that actually matters: `vfs-directord`'s
    /// `SessionRegistry::add_source` rebuilds a root's list and installs it with
    /// `set_root_mounts` on *every* source, so in the daemon a `SeqRead`
    /// provider mounted cleanly and failed every read instead. Two entry points,
    /// one contract.
    #[test]
    fn set_root_mounts_refuses_a_sequential_provider_too() {
        let seq: Arc<dyn Provider> = Arc::new(vfs_provider::conformance::SeqFixture::new());
        let s = Session::new();
        assert_eq!(
            s.set_root_mounts(RootId(0), vec![(String::new(), Arc::clone(&seq))]),
            Err(ST_BAD_REQUEST),
            "set_root_mounts must refuse what mount_at refuses"
        );
        // Refused *before* it was recorded, so the root is still usable — the
        // rejected list must not be parked where every later mount inherits it.
        let wrapped: Arc<dyn Provider> = Arc::new(SeekableProvider::new(Arc::clone(&seq)));
        s.set_root_mounts(RootId(0), vec![(String::new(), wrapped)])
            .expect("seekable(seq) installs through set_root_mounts");
        assert_eq!(s.read_file("a.txt").unwrap(), b"hello");
    }

    /// The third route into a root's provider, and the reason the check lives in
    /// `compose_root` as well as in the two `Session` methods: `compose_root` is
    /// public, and `skyrim-live` and `SessionRegistry::compose` both call it
    /// directly and hand the result to `Director::mount`, never touching
    /// `mount_at` or `set_root_mounts`.
    #[test]
    fn compose_root_refuses_a_sequential_mount() {
        let seq: Arc<dyn Provider> = Arc::new(vfs_provider::conformance::SeqFixture::new());
        assert_eq!(
            compose_root(vec![(String::new(), Arc::clone(&seq))], None).err(),
            Some(ST_BAD_REQUEST),
            "composing a SeqRead mount must be refused at the funnel too"
        );
        let wrapped: Arc<dyn Provider> = Arc::new(SeekableProvider::new(seq));
        assert!(
            compose_root(vec![(String::new(), wrapped)], None).is_ok(),
            "seekable(seq) must still compose"
        );
    }

    /// `readonly` is what makes spec §7's discovery workflow reachable at all:
    /// `DiskProvider` is `ReadWrite`, so a graph built from `disk` alone can
    /// never refuse a write and `rejected_writes()` can never be non-empty.
    /// `rejected_writes()` is process-global with no session dimension, so a
    /// test asserting on it takes a lock rather than assuming test order — the
    /// convention stated at `VA_LOCK` in `vfs-shim::lazy_section`. Nothing else
    /// in this binary touches the table today; the lock is here so that stays
    /// true when something does.
    static REJECTED_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn a_write_refused_by_a_readonly_layer_is_recorded_for_discovery() {
        let _rej = REJECTED_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("vfs-ro-rej-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("vanilla.ini"), b"[General]").unwrap();

        let s = Session::new();
        s.mount("", Arc::new(ReadOnlyProvider::new(Arc::new(DiskProvider::new(&dir)))))
            .unwrap();

        reset_rejected_writes();
        // The director's own pre-check: the root's provider declares Read, so an
        // OPEN_WRITE is refused before it reaches the provider — and recorded.
        assert_eq!(
            s.kernel().open(RootId(0), "vanilla.ini", OPEN_WRITE),
            Err(ST_READ_ONLY)
        );
        let rejected = rejected_writes();
        assert!(
            rejected.iter().any(|(p, n)| p == "vanilla.ini" && *n >= 1),
            "the refused write must be discoverable by path; got {rejected:?}"
        );
        // The file on disk is untouched: the refusal is not a silent success.
        assert_eq!(std::fs::read(dir.join("vanilla.ini")).unwrap(), b"[General]");
        reset_rejected_writes();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Drives the live shared-memory ring end to end, so it is Windows-only:
    // `Session::serve` has no non-Windows body yet and `Session::ipc` (used
    // below) does not exist there at all — increment 2 of
    // docs/superpowers/specs/2026-09-01-wine-hosted-shim-design.md.
    #[cfg(windows)]
    #[test]
    fn session_serve_and_ring_read() {
        use vfs_protocol::{
            decode_open_resp, decode_read_resp, encode_open_req, encode_read_req, OpenResp, ReadReq,
            OP_OPEN, OP_READ, OPEN_READ, ST_OK,
        };

        let dir = std::env::temp_dir().join(format!("vfs-sess-ring-{}", std::process::id()));
        let state = std::env::temp_dir().join(format!("vfs-sess-st-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("payload.bin"), b"ring-bytes").unwrap();

        let mut s = Session::new();
        s.set_root(&dir);
        s.set_state_dir(&state);
        s.mount("", Arc::new(DiskProvider::new(&dir))).unwrap();
        s.serve().expect("serve");
        assert!(s.is_serving());

        {
            let ipc = s.ipc().expect("ipc");
            let client = ipc.client().expect("client");
            let open = client
                .submit(OP_OPEN, 0, &encode_open_req(0, OPEN_READ, "payload.bin"))
                .unwrap();
            assert_eq!(open.status, ST_OK);
            let OpenResp { fh, size, .. } = decode_open_resp(&open.payload).unwrap();
            assert_eq!(size, 10);
            let r = client
                .submit(
                    OP_READ,
                    0,
                    &encode_read_req(&ReadReq {
                        fh,
                        offset: 0,
                        len: 10,
                    }),
                )
                .unwrap();
            assert_eq!(r.status, ST_OK);
            assert_eq!(decode_read_resp(&r.payload).unwrap(), b"ring-bytes");
        }

        s.stop_serve();
        assert!(!s.is_serving());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state);
    }
}

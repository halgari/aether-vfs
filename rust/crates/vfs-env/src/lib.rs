//! Every `VFS_*` environment switch, defined once.
//!
//! # Why this exists
//!
//! Configuration reaches this system almost entirely through the environment:
//! the host sets variables, `CreateProcessW` inherits them, and the shim reads
//! them inside the game. That is the right mechanism — it survives a process
//! boundary we do not otherwise control — but spelling the names as string
//! literals at both ends made two failure modes routine, and neither produces
//! an error:
//!
//! 1. **Silent drift.** A name renamed at the writer is not renamed at the
//!    reader. Measured 2026-08-13: the hollow refactor renamed
//!    `VFS_HOLLOW_HOST` to `VFS_LAUNCH_IMAGE` at the writer, and the shim's
//!    `stage_root_from_env` kept reading the old name — so the staging-directory
//!    alias silently stopped resolving. Nothing failed; it just stopped working,
//!    and only did not bite because a separate change had made that alias
//!    redundant.
//! 2. **An unreviewable surface.** Switches accumulate one inline
//!    `env::var("…")` at a time. Before this module there were 47, of which 25
//!    appeared in no document, several of them able to disable a serving path or
//!    un-seal the managed root.
//!
//! So: one constant per name, one table describing all of them, and a test in
//! this crate that fails if any crate reads a `VFS_*` name that is not in the
//! table. Drift becomes a test failure instead of a behaviour change.
//!
//! # What this module is not
//!
//! It does not own defaults for values that belong to a caller (paths, sizes a
//! host computes). It owns the *name*, the *meaning*, and the parsing of the
//! handful of switches that are booleans, because "is `0` false?" was answered
//! three different ways before this.

use std::ffi::OsString;
use std::path::PathBuf;

// ─── ring / IPC handshake ────────────────────────────────────────────────────
// Written by the director when it stands up the shared segment; read by the
// shim's FUSE client. Both ends must agree exactly, which is the whole point of
// naming them here.

/// Name of the shared-memory section backing the control ring.
pub const RING_SECTION: &str = "VFS_RING_SECTION";
/// The ring's backing **file**, used instead of [`RING_SECTION`] when the client
/// and the server sit on opposite sides of a Wine boundary.
///
/// A page-file-backed named section exists only inside one Windows (or Wine)
/// session and has no identity a native Linux process can open, whereas a
/// file-backed mapping is coherent with an `mmap` of the same path. When this is
/// set it **wins** over `RING_SECTION`: a session configured for the Wine path
/// must not silently fall back to a named section that happens to exist.
pub const RING_PATH: &str = "VFS_RING_PATH";
/// Total size of that mapping, in bytes.
pub const RING_BYTES: &str = "VFS_RING_BYTES";
/// Maximum inline payload carried in a ring slot.
pub const RING_PAYLOAD_CAP: &str = "VFS_RING_PAYLOAD_CAP";
/// Byte offset of the bulk arena within the segment.
pub const ARENA_OFFSET: &str = "VFS_ARENA_OFFSET";
/// Length of the bulk arena, in bytes.
pub const ARENA_LEN: &str = "VFS_ARENA_LEN";
/// Event the client signals to wake a sleeping director.
pub const SERVER_EV: &str = "VFS_SERVER_EV";
/// Event the director signals to wake a waiting client.
pub const CLIENT_EV: &str = "VFS_CLIENT_EV";
/// Path to the thin FUSE config the shim reads at startup.
pub const FUSE_CFG: &str = "VFS_FUSE_CFG";
/// How long the ring spins before sleeping, in microseconds.
///
/// The single most consequential performance switch in the tree: sleeping
/// between bursts cost ~7.6 s of game load before spin-then-wait landed.
pub const RING_SPIN_US: &str = "VFS_RING_SPIN_US";

// ─── managed root and session paths ──────────────────────────────────────────

/// The managed virtual root. Everything under it resolves through the director.
///
/// **Required.** There is no good default — it names *which tree is being
/// virtualised* — and the one that used to exist pointed at a layout that no
/// longer exists, so an unset root connected the client to a path nothing
/// matched and the failure surfaced later as missing content.
pub const VIRTUAL_DIR: &str = "VFS_VIRTUAL_DIR";
/// The session's *additional* managed roots, beyond root `0`
/// ([`VIRTUAL_DIR`]): `id=path` entries separated by `;`, e.g.
/// `1=C:\Users\me\Documents\My Games\Skyrim`.
///
/// A session virtualizes several real filesystem locations, one provider each
/// (stage 2b). The shim must know every one of them, because the root id is
/// what its ring requests now carry — a root the shim has never heard of is a
/// root whose paths it classifies as "not ours" and lets fall to real disk.
///
/// Optional and additive: unset means the single-root session every caller
/// before stage 2b had, and [`VIRTUAL_DIR`] alone still defines root `0`. A
/// malformed entry is skipped rather than failing the launch — the same shape
/// as an unparseable numeric switch elsewhere here — but an entry naming id
/// `0` overrides [`VIRTUAL_DIR`]'s path for that root rather than being
/// silently ignored.
pub const VIRTUAL_ROOTS: &str = "VFS_VIRTUAL_ROOTS";
/// Directory for session state (ready flag, configs, logs).
pub const STATE_DIR: &str = "VFS_STATE_DIR";
/// Base directory aether-vfs owns: downloaded Proton runtimes, per-session
/// prefixes, overlays. Defaults to `$XDG_DATA_HOME/aether-vfs`, then
/// `$HOME/.local/share/aether-vfs`, and `%LOCALAPPDATA%\aether-vfs` on Windows.
///
/// Deliberately ours and never a system or Steam path: `umu` downloads Proton
/// into `~/.local/share/Steam/compatibilitytools.d`, and not touching that is
/// the point of acquiring runtimes ourselves.
pub const HOME: &str = "VFS_HOME";
/// Directory of the daemon's persistent storage (write layers and the block
/// cache). Defaults to `$VFS_HOME/storage`.
pub const STORAGE_DIR: &str = "VFS_STORAGE_DIR";
/// Absolute path of the image to launch, normally the staged EXE. The shim also
/// derives the staging directory from this, and serves that directory as an
/// alias for the virtual root.
pub const LAUNCH_IMAGE: &str = "VFS_LAUNCH_IMAGE";
/// Where the daemon publishes its endpoint for clients to discover.
pub const DISCOVERY_PATH: &str = "VFS_DISCOVERY_PATH";
/// Directory holding the Windows artefacts a Proton launch needs
/// (`vfs-injector.exe`, `vfs_shim_dll.dll`, `vfs_payload.dll`), used instead of
/// the directory beside the running executable. An explicit
/// `LaunchOpts::shim_dll` still wins. The Proton tests read it too.
pub const WINDOWS_ARTIFACTS: &str = "VFS_WINDOWS_ARTIFACTS";
/// Seconds to wait for the shim's hooks to report ready before giving up.
///
/// One value bounds both waits: the launcher's wait for the top-level target
/// (`vfs-injector` reads it; `run_target_with_shim` also *sets* it in the
/// target's environment from `RunConfig::ready_timeout`), and the shim's wait
/// for each child process it injects (`CreateProcessInternalW`), which reads it
/// back from that inherited environment. A child that is a slow but healthy
/// cold start therefore gets the same allowance the launch did. Unset or
/// unparsable: [`DEFAULT_READY_TIMEOUT_SECS`].
pub const READY_TIMEOUT_SECS: &str = "VFS_READY_TIMEOUT_SECS";
/// The ready wait when [`READY_TIMEOUT_SECS`] is unset: what a Windows
/// `Session::launch` has always defaulted to. A cold first launch in a fresh
/// Wine prefix can take well over the 20 s this used to be. The one place the
/// number is written; the injector, `vfs-embed` and the shim all use it.
pub const DEFAULT_READY_TIMEOUT_SECS: u64 = 180;

/// The ready wait in seconds, as the environment says it: [`READY_TIMEOUT_SECS`]
/// if set and numeric, else [`DEFAULT_READY_TIMEOUT_SECS`]; never below 1.
pub fn ready_timeout_secs() -> u64 {
    parsed_or(READY_TIMEOUT_SECS, DEFAULT_READY_TIMEOUT_SECS).max(1)
}
/// Working directory `vfs-injector` starts its target in, as the target sees
/// it (`C:\…`). Set by the Proton launch; unset, the target inherits the
/// injector's own directory.
pub const INJECT_CWD: &str = "VFS_INJECT_CWD";
/// The command line of Proton's Steam helper, as `vfs-injector` runs it
/// (`C:\windows\system32\steam.exe <a program>`), or
/// [`INJECT_STEAM_HELPER_OFF`]. Set by the Proton launch: with a command
/// line, the injector starts the helper and waits for it to publish itself as
/// the running Steam client before it creates the target; with `off`, it only
/// clears the pid an earlier helper left. Either way it reports what it did
/// in `<ready file>` + [`STEAM_HELPER_REPORT_SUFFIX`]. Unset, it does
/// neither. Honoured only under Wine.
pub const INJECT_STEAM_HELPER: &str = "VFS_INJECT_STEAM_HELPER";
/// The [`INJECT_STEAM_HELPER`] value that asks for no helper, only for the
/// stale pid to be cleared.
pub const INJECT_STEAM_HELPER_OFF: &str = "off";

// ─── injection handshake ─────────────────────────────────────────────────────

/// Path to the shim's config file, read during bootstrap.
pub const SHIM_CONFIG: &str = "VFS_SHIM_CONFIG";
/// Path to the flag the shim touches once its hooks are live.
pub const SHIM_READY: &str = "VFS_SHIM_READY";
/// Path to `vfs_payload.dll`, for children that resolve it by environment.
pub const PAYLOAD_PATH: &str = "VFS_PAYLOAD_PATH";
/// File carrying the remote address of the payload config, for `install_late`.
pub const PAYLOAD_CFG_FILE: &str = "VFS_PAYLOAD_CFG_FILE";
/// Set when the launch uses the dual-layer (pre-init payload + full shim) path.
pub const DUAL_LAYER: &str = "VFS_DUAL_LAYER";
/// Test-only: force `vfs_shim::fuse_client::try_init_from_env` to report a
/// connect failure, regardless of ring configuration. Exists to exercise a
/// director-launched process's abort path without a director that is
/// actually broken.
pub const TEST_FUSE_INIT_FAIL: &str = "VFS_TEST_FUSE_INIT_FAIL";

// ─── shim-ready handshake payload ────────────────────────────────────────────
// Not `VFS_*` switch names — these are the two contents [`SHIM_READY`] can
// hold once written. The file's mere existence used to be the whole protocol:
// the launcher polled for the path and released the process the moment it
// appeared. That could not distinguish "hooks are live and virtualising" from
// "hooks are live but the FUSE client never attached" — exactly the silent,
// total bypass this pair of constants exists to close. The launcher now reads
// the content and refuses to release the process on the failure spelling.

/// Written when hooks are live and, if a director was configured, its FUSE
/// client attached. The launcher may release the process.
pub const READY_OK: &str = "ready";
/// Prefix of the content written when a director *was* configured (a ring
/// section was named) but the FUSE client failed to attach — followed by a
/// short reason. Releasing the process past this point means every path it
/// opens falls straight through to the real filesystem, unnoticed.
pub const READY_FUSE_FAILED_PREFIX: &str = "fuse-failed:";
/// Prefix of the content written when the shim could not bootstrap for a
/// reason that is not the director: a config from another build or a damaged
/// one, an unreadable config file, a hook that would not install. Followed by
/// the reason. The launcher kills the parked process on it, as for
/// [`READY_FUSE_FAILED_PREFIX`]. (An older shim spelled a config error with the
/// fuse prefix; an injector still reads that spelling, as a fuse failure.)
pub const READY_BOOTSTRAP_FAILED_PREFIX: &str = "bootstrap-failed:";

// ─── injector failure report ─────────────────────────────────────────────────
// Also not switch names. `vfs-injector` exits 3 for every injection failure,
// and an exit code carries no reason; so before exiting it writes one line to
// `<ready file>` + [`INJECTOR_ERROR_SUFFIX`], which the Proton launch reads
// back into its error.

/// Appended to the ready file's path to name the injector's failure report.
pub const INJECTOR_ERROR_SUFFIX: &str = ".injector-error";
/// Report prefix: the target exited before the shim reported ready, followed
/// by its exit code in hex (`0xc0000135`).
pub const INJECTOR_TARGET_EXITED_PREFIX: &str = "target-exited:";
/// Report prefix: the shim did not report ready in time, followed by the
/// timeout in seconds.
pub const INJECTOR_READY_TIMEOUT_PREFIX: &str = "ready-timeout:";
/// Report prefix: the shim reported that the director's client failed to
/// attach, followed by the reason.
pub const INJECTOR_FUSE_FAILED_PREFIX: &str = "fuse-failed:";
/// Report prefix: the shim reported that it could not bootstrap (bad config,
/// hook install), followed by the reason.
pub const INJECTOR_BOOTSTRAP_FAILED_PREFIX: &str = "bootstrap-failed:";
/// Report prefix: any other injection failure, followed by its description.
pub const INJECTOR_FAILED_PREFIX: &str = "inject:";

// ─── Steam helper report ─────────────────────────────────────────────────────
// Also not switch names: what `vfs-injector` did with [`INJECT_STEAM_HELPER`],
// one line in `<ready file>` + [`STEAM_HELPER_REPORT_SUFFIX`], written before
// the target is created. Its absence once the target is running means an
// injector that predates the helper.

/// Appended to the ready file's path to name the injector's helper report.
pub const STEAM_HELPER_REPORT_SUFFIX: &str = ".steam-helper";
/// The helper published itself: `started:<pid>:<milliseconds>`.
pub const STEAM_HELPER_STARTED_PREFIX: &str = "started:";
/// No helper was asked for, and the stale pid was cleared.
pub const STEAM_HELPER_CLEARED: &str = "cleared";
/// The helper was not started: `disabled:<why>`.
pub const STEAM_HELPER_DISABLED_PREFIX: &str = "disabled:";
/// The helper was started and failed (did not start, exited early, timed out
/// and was stopped): `failed:<why>`.
pub const STEAM_HELPER_FAILED_PREFIX: &str = "failed:";

// ─── behaviour switches (booleans) ───────────────────────────────────────────

/// Allow an under-root miss to fall through to whatever is really on disk.
///
/// **This is the isolation invariant.** With it set, the game can read content
/// the VFS did not give it. Default off, and `skyrim-live` clears it defensively
/// at startup.
pub const ALLOW_DISK_FALLTHROUGH: &str = "VFS_ALLOW_DISK_FALLTHROUGH";
/// Serve the managed root from real disk only, bypassing the director.
pub const DISK_ONLY_ROOT: &str = "VFS_DISK_ONLY_ROOT";
// `VFS_KEEP_HOST_STEAM_API`, `VFS_FUSE_SKYRIM_EXE` and the temporary
// `VFS_CLOSE_DRM_EXCEPTIONS` probe switch were deleted by gate 5, Task 4 along
// with the four DRM/identity exceptions they configured. Nothing under a
// managed root reaches the host tree any more, so there is nothing left for
// them to select between.
/// Start managed children in the virtual root rather than the launcher's cwd.
pub const CHILD_CWD_ROOT: &str = "VFS_CHILD_CWD_ROOT";
/// Refuse `SEC_IMAGE` sections on VFS-backed handles.
pub const REJECT_FUSE_SECTION: &str = "VFS_REJECT_FUSE_SECTION";
/// Refuse data sections on VFS-backed handles (narrower than the above).
pub const REJECT_FUSE_DATA_SECTION: &str = "VFS_REJECT_FUSE_DATA_SECTION";
/// Disable the vectored handler that demand-pages lazy sections.
pub const LAZY_NO_VEH: &str = "VFS_LAZY_NO_VEH";
/// The shim's block cache for small reads of immutable files
/// (`vfs_ipc::readcache`). On unless set to `0`/`false`/`no`/`off`, which
/// sends every read over the ring as before — what to do when capturing a
/// provider-side trace of the program's own read pattern, which the cache
/// otherwise turns into 1 MiB block fetches.
pub const SHIM_READ_CACHE: &str = "VFS_SHIM_READ_CACHE";
/// The read cache's process-wide budget in MiB (default 256): what the
/// cached units of every file, and the fetches in flight, may hold at once.
pub const SHIM_READ_CACHE_MIB: &str = "VFS_SHIM_READ_CACHE_MIB";
/// Install the shim's registry hooks (the registry overlay). Set to `1` by the
/// host only while the session has a registry layer attached; without it the
/// registry is not virtualised.
pub const REGISTRY: &str = "VFS_REGISTRY";
/// Wait for the launched process to exit instead of detaching.
pub const WAIT: &str = "VFS_WAIT";
/// Stop at the first rendered frame and print a benchmark row.
pub const BENCH: &str = "VFS_BENCH";

// ─── diagnostics (a path enables the log) ────────────────────────────────────

/// Per-hook call counts, timings and path frequencies.
pub const SHIM_STATS_LOG: &str = "VFS_SHIM_STATS_LOG";
/// Overrides the report's periodic-write interval (milliseconds), default
/// 250. Nothing flushes the report on process exit, so a process shorter
/// than the interval — a millisecond-scale test fixture, never a real game
/// session — produces no report file at all; this lets such a caller shorten
/// the interval for just its own child instead of guessing at a longer sleep.
pub const SHIM_STATS_INTERVAL_MS: &str = "VFS_SHIM_STATS_INTERVAL_MS";
/// Every file the director serves, with its size.
pub const DIRECTOR_OPEN_LOG: &str = "VFS_DIRECTOR_OPEN_LOG";
/// Opens of the game EXE, for tracing DRM behaviour.
pub const DRM_EXE_LOG: &str = "VFS_DRM_EXE_LOG";
/// Demand-paged section fills.
pub const SECTION_FILL_LOG: &str = "VFS_SECTION_FILL_LOG";
/// Where the shim records a panic before it takes the game down.
pub const SHIM_PANIC_LOG: &str = "VFS_SHIM_PANIC_LOG";
/// Path for the hook breadcrumb: a file-backed mapping naming the hook this
/// process is currently inside.
///
/// For diagnosing an injected process that hangs in a hook with zero CPU and
/// immune to `TerminateProcess`. It must be a shared *file* rather than heap
/// state, because such a process cannot be attached to — an outside reader
/// samples the file while the target is wedged. Two relaxed stores per hook, no
/// clock and no thread, unlike [`SHIM_STATS_LOG`], whose reporter thread has
/// been measured to suppress the very race it would be used to find.
pub const SHIM_BREADCRUMB: &str = "VFS_SHIM_BREADCRUMB";
/// Label for the benchmark row emitted under [`BENCH`].
pub const BENCH_LABEL: &str = "VFS_BENCH_LABEL";

// ─── skyrim-live harness ─────────────────────────────────────────────────────

/// Source archive.
pub const SKYRIM_ZIP: &str = "VFS_SKYRIM_ZIP";
/// Session data root (saves, profiles, overrides, staging).
pub const SKYRIM_DATA: &str = "VFS_SKYRIM_DATA";
/// The managed virtual root for the harness.
pub const SKYRIM_ROOT: &str = "VFS_SKYRIM_ROOT";
/// Mod overlay directory, composed above the archive.
pub const SKYRIM_MODS: &str = "VFS_SKYRIM_MODS";
/// Executable to launch: the game, or a loader such as `skse64_loader.exe`.
pub const SKYRIM_LAUNCH: &str = "VFS_SKYRIM_LAUNCH";
/// Serve from an extracted tree instead of the archive, for differential runs.
pub const SKYRIM_DISK: &str = "VFS_SKYRIM_DISK";
/// Skip the harness's `SkyrimPrefs.ini` seeding (gate 4, Task 9).
///
/// The seeding turns off the Bethesda.net platform and the missing-content
/// startup check so a main-menu dialog cannot hold an unattended session. Set
/// this to run with the profile exactly as it is on disk — the control arm for
/// deciding whether a menu dialog was caused by the seeding or merely
/// unaffected by it.
pub const SKYRIM_NO_PROFILE_SEED: &str = "VFS_SKYRIM_NO_PROFILE_SEED";

// ─── test fixtures ───────────────────────────────────────────────────────────

/// Path a fixture reads or writes.
pub const FIXTURE_PATH: &str = "VFS_FIXTURE_PATH";
/// Expected byte length, for the read fixture.
pub const FIXTURE_EXPECT: &str = "VFS_FIXTURE_EXPECT";
/// Expected fill byte, for the read fixture.
pub const FIXTURE_FILL: &str = "VFS_FIXTURE_FILL";
/// `vfs-fixture-read`: after the read, write `FIXTURE_WRITE_DATA` (default
/// `written`) here — how a Linux e2e proves a write into a second root lands
/// in that root's write layer.
pub const FIXTURE_WRITE_PATH: &str = "VFS_FIXTURE_WRITE_PATH";
/// `vfs-fixture-read`: the bytes written to `FIXTURE_WRITE_PATH` (default `written`).
pub const FIXTURE_WRITE_DATA: &str = "VFS_FIXTURE_WRITE_DATA";
/// `vfs-fixture-read`: a file the director serves immutable, read in many
/// small pieces by every route and compared with one big read (the shim's
/// read cache).
pub const FIXTURE_CACHE_PATH: &str = "VFS_FIXTURE_CACHE_PATH";
/// `vfs-fixture-read`: a file read in small pieces, rewritten with
/// `FIXTURE_CACHE_RW_DATA` through another handle, and read again.
pub const FIXTURE_CACHE_RW_PATH: &str = "VFS_FIXTURE_CACHE_RW_PATH";
/// `vfs-fixture-read`: what `FIXTURE_CACHE_RW_PATH` is rewritten with.
pub const FIXTURE_CACHE_RW_DATA: &str = "VFS_FIXTURE_CACHE_RW_DATA";
/// `vfs-fixture-read`: milliseconds to stay alive after the read-cache phase,
/// so a `SHIM_STATS_LOG` report covers it.
pub const FIXTURE_LINGER_MS: &str = "VFS_FIXTURE_LINGER_MS";
/// `vfs-fixture-read`: after the first read, run a second copy of the fixture
/// and fail unless it succeeds (it can only if the shim injected it).
pub const FIXTURE_SPAWN_CHILD: &str = "VFS_FIXTURE_SPAWN_CHILD";
/// `vfs-fixture-read`: a file read on slow threads while others read
/// `FIXTURE_PATH` (the concurrent-read e2e).
pub const FIXTURE_SLOW_PATH: &str = "VFS_FIXTURE_SLOW_PATH";
/// `vfs-fixture-read`: how many threads read `FIXTURE_SLOW_PATH`.
pub const FIXTURE_SLOW_THREADS: &str = "VFS_FIXTURE_SLOW_THREADS";
/// `vfs-fixture-read`: a file waited for before the fast reads start.
pub const FIXTURE_SLOW_STARTED: &str = "VFS_FIXTURE_SLOW_STARTED";
/// `vfs-fixture-read`: a path looked up once the fast reads have finished.
pub const FIXTURE_SLOW_RELEASE: &str = "VFS_FIXTURE_SLOW_RELEASE";
/// `vfs-fixture-read`: how many threads read `FIXTURE_PATH` alongside the slow ones.
pub const FIXTURE_THREADS: &str = "VFS_FIXTURE_THREADS";
/// `vfs-fixture-read`: how many times each fast thread reads `FIXTURE_PATH`.
pub const FIXTURE_ROUNDS: &str = "VFS_FIXTURE_ROUNDS";
/// `vfs-fixture-read` names phase: `kind|opened|final` entries.
pub const FIXTURE_NAMES: &str = "VFS_FIXTURE_NAMES";
/// `vfs-fixture-read` names phase: `dir|file` entries.
pub const FIXTURE_NAME_PREFIXES: &str = "VFS_FIXTURE_NAME_PREFIXES";
/// `vfs-fixture-read` names phase: `dir|child,child…` entries.
pub const FIXTURE_NAME_LISTS: &str = "VFS_FIXTURE_NAME_LISTS";
/// `vfs-fixture-read` names phase: paths to create.
pub const FIXTURE_NAME_CREATES: &str = "VFS_FIXTURE_NAME_CREATES";
/// `vfs-fixture-read` names phase: `from|to` renames.
pub const FIXTURE_NAME_RENAMES: &str = "VFS_FIXTURE_NAME_RENAMES";
// `VFS_FIXTURE_DATA` and `VFS_FIXTURE_DIR` lived here for `vfs-fixture-write`
// and `vfs-fixture-writeset`. Both fixture crates were deleted in gate 4 task
// 8 — no test harness had ever invoked either — so the switches went with
// them rather than staying as a surface nothing reads.
/// A path `vfs-fixture-writepath` edits **in place** before its other steps:
/// read-write open with no create and no truncate, so only the director's
/// copy-up can answer it (gate 4, Task 6b). Unset, that step does not run and
/// the fixture behaves as it did before the step existed — which is what
/// keeps the two pre-existing write-path scenarios unchanged.
pub const FIXTURE_COW_PATH: &str = "VFS_FIXTURE_COW_PATH";
/// Restrict `vfs-fixture-escape` to constructing and attempting exactly one
/// of its fourteen vectors, skipping every other one entirely rather than
/// merely omitting it from the output — see that crate's module doc for why
/// a caller correlating against the shim's own (not vector-keyed) hook-stats
/// report needs this to isolate one vector's own classification effect.
pub const ESCAPE_ONLY_VECTOR: &str = "VFS_ESCAPE_ONLY_VECTOR";
/// Which access `vfs-fixture-escape` exercises against every one of its
/// spellings: `read` (the default) or `write`. The spellings themselves are
/// identical either way — only the call made against each one changes — so the
/// two matrices are comparable line for line, which is what lets a reader see
/// that a vector sealed for reads is also sealed for writes rather than
/// inferring it. Anything else, including an unrecognised value, runs the read
/// matrix: a containment fixture must never be switched off by a typo.
pub const ESCAPE_ACCESS: &str = "VFS_ESCAPE_ACCESS";
/// A pre-existing junction directory for vector 7 (junction/reparse point)
/// to open through, created by the *caller* before launching the fixture at
/// all — not by the fixture itself. Set, `vector7_junction` opens
/// `<this>\<target's own filename>` directly and skips its own `mklink /J`
/// construction step entirely; unset, it falls back to constructing (and
/// cleaning up) its own junction exactly as before, for a standalone
/// (uninjected) reproduction where no such pre-existing junction is set up.
///
/// Needed once `vfs-redirect`'s `RootMap` volume/junction table is resolved
/// lazily on a session's first real decision rather than eagerly at
/// bootstrap (see `vfs-shim::Engine::map`): the fixture's own `mklink /J`
/// spawn is itself real, hooked file activity in the injected process, so
/// if the fixture created the junction *after* that first decision had
/// already fired — which it reliably had, since spawning `cmd.exe` to run
/// `mklink` is exactly such activity — the junction would not exist yet at
/// the moment resolution ran, and would never be picked up afterward (the
/// table is resolved once, not on a schedule). Creating the junction from
/// the test harness process (never injected) before the fixture is even
/// launched sidesteps the ordering question entirely — indistinguishable,
/// from the shim's perspective, from a junction a real mod manager already
/// had in place before the game process started.
pub const ESCAPE_VECTOR7_LINK_DIR: &str = "VFS_ESCAPE_VECTOR7_LINK_DIR";

/// The INI file `vfs-fixture-prefs` drives the Windows profile APIs against —
/// `GetPrivateProfileStringW` and friends, the way Skyrim loads
/// `SkyrimPrefs.ini`. Separate from [`FIXTURE_PATH`] because this fixture uses
/// both a subject path and an output path, and one name cannot be both.
pub const FIXTURE_INI_PATH: &str = "VFS_FIXTURE_INI_PATH";
/// Where `vfs-fixture-prefs` writes its tab-separated results. Must be
/// **outside** every managed root: writing them is not part of what is under
/// test. Unset, results go to stdout.
pub const FIXTURE_INI_OUT: &str = "VFS_FIXTURE_INI_OUT";
/// Set, `vfs-fixture-prefs` calls `WritePrivateProfileStringW` with this value
/// before reading, exercising the write half of the profile API. Unset, the
/// fixture only reads.
pub const FIXTURE_INI_WRITE: &str = "VFS_FIXTURE_INI_WRITE";
/// The INI section `vfs-fixture-prefs` reads/writes.
pub const FIXTURE_INI_SECTION: &str = "VFS_FIXTURE_INI_SECTION";
/// The INI key `vfs-fixture-prefs` reads/writes.
pub const FIXTURE_INI_KEY: &str = "VFS_FIXTURE_INI_KEY";
/// `vfs-fixture-steam`: the `steam_api64.dll` to load (default: by name, in
/// the loader's search order).
pub const FIXTURE_STEAM_API_DLL: &str = "VFS_FIXTURE_STEAM_API_DLL";
/// `vfs-fixture-steam`: a file that receives the report it prints.
pub const FIXTURE_STEAM_OUT: &str = "VFS_FIXTURE_STEAM_OUT";
/// `vfs-fixture-steam`: `0` skips the controller and Steam Input calls.
pub const FIXTURE_STEAM_INPUT: &str = "VFS_FIXTURE_STEAM_INPUT";
/// `vfs-fixture-steam`: an action manifest path to hand Steam Input, the call
/// that waits for the client's controller mapping.
pub const FIXTURE_STEAM_MANIFEST: &str = "VFS_FIXTURE_STEAM_MANIFEST";

/// What a switch is for, so the surface can be listed and reviewed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Written by the host, read by the child. Both ends must agree.
    Handshake,
    /// Changes what the system does. The ones worth auditing.
    Behaviour,
    /// Names a file; setting it enables a diagnostic.
    Diagnostic,
    /// Input to the `skyrim-live` harness.
    Harness,
    /// Input to a test fixture binary.
    Fixture,
}

/// One row of the switch surface.
#[derive(Clone, Copy, Debug)]
pub struct Var {
    pub name: &'static str,
    pub kind: Kind,
    /// What happens when it is unset.
    pub default: &'static str,
}

/// Every switch. The drift test asserts the source reads nothing outside this.
pub const ALL: &[Var] = &[
    Var { name: RING_SECTION, kind: Kind::Handshake, default: "required by the shim" },
    Var { name: RING_PATH, kind: Kind::Handshake, default: "none (named section via VFS_RING_SECTION)" },
    Var { name: RING_BYTES, kind: Kind::Handshake, default: "ring default" },
    Var { name: RING_PAYLOAD_CAP, kind: Kind::Handshake, default: "ring default" },
    Var { name: ARENA_OFFSET, kind: Kind::Handshake, default: "ring default" },
    Var { name: ARENA_LEN, kind: Kind::Handshake, default: "ring default" },
    Var { name: SERVER_EV, kind: Kind::Handshake, default: "no server wake" },
    Var { name: CLIENT_EV, kind: Kind::Handshake, default: "no client wake" },
    Var { name: FUSE_CFG, kind: Kind::Handshake, default: "none" },
    Var { name: RING_SPIN_US, kind: Kind::Behaviour, default: "400" },
    Var { name: VIRTUAL_DIR, kind: Kind::Handshake, default: r"C:\GameLayers\runtime (see audit §2.6)" },
    Var { name: VIRTUAL_ROOTS, kind: Kind::Handshake, default: "none (root 0 only)" },
    Var { name: STATE_DIR, kind: Kind::Handshake, default: "session state dir" },
    Var { name: HOME, kind: Kind::Handshake, default: "$XDG_DATA_HOME/aether-vfs" },
    Var { name: STORAGE_DIR, kind: Kind::Behaviour, default: "$VFS_HOME/storage" },
    Var { name: LAUNCH_IMAGE, kind: Kind::Handshake, default: "none; staging derives it" },
    Var { name: DISCOVERY_PATH, kind: Kind::Handshake, default: "platform default" },
    Var { name: WINDOWS_ARTIFACTS, kind: Kind::Behaviour, default: "beside the running executable" },
    Var { name: READY_TIMEOUT_SECS, kind: Kind::Behaviour, default: "180 (DEFAULT_READY_TIMEOUT_SECS)" },
    Var { name: INJECT_CWD, kind: Kind::Handshake, default: "the injector's own directory" },
    Var { name: INJECT_STEAM_HELPER, kind: Kind::Handshake, default: "no Steam helper" },
    Var { name: SHIM_CONFIG, kind: Kind::Handshake, default: "required by the shim" },
    Var { name: SHIM_READY, kind: Kind::Handshake, default: "no ready signal" },
    Var { name: PAYLOAD_PATH, kind: Kind::Handshake, default: "resolved beside the shim" },
    Var { name: PAYLOAD_CFG_FILE, kind: Kind::Handshake, default: "none" },
    Var { name: DUAL_LAYER, kind: Kind::Handshake, default: "unset" },
    Var { name: TEST_FUSE_INIT_FAIL, kind: Kind::Fixture, default: "false (FUSE inits normally)" },
    Var { name: ALLOW_DISK_FALLTHROUGH, kind: Kind::Behaviour, default: "false (root stays sealed)" },
    Var { name: DISK_ONLY_ROOT, kind: Kind::Behaviour, default: "false" },
    Var { name: CHILD_CWD_ROOT, kind: Kind::Behaviour, default: "true" },
    Var { name: REJECT_FUSE_SECTION, kind: Kind::Behaviour, default: "false" },
    Var { name: REJECT_FUSE_DATA_SECTION, kind: Kind::Behaviour, default: "false" },
    Var { name: LAZY_NO_VEH, kind: Kind::Behaviour, default: "false (VEH installed)" },
    Var { name: SHIM_READ_CACHE, kind: Kind::Behaviour, default: "on" },
    Var { name: SHIM_READ_CACHE_MIB, kind: Kind::Behaviour, default: "256" },
    Var { name: REGISTRY, kind: Kind::Handshake, default: "false (registry not virtualised)" },
    Var { name: WAIT, kind: Kind::Behaviour, default: "false (detach)" },
    Var { name: BENCH, kind: Kind::Behaviour, default: "false" },
    Var { name: SHIM_STATS_LOG, kind: Kind::Diagnostic, default: "off" },
    Var { name: SHIM_STATS_INTERVAL_MS, kind: Kind::Diagnostic, default: "250" },
    Var { name: DIRECTOR_OPEN_LOG, kind: Kind::Diagnostic, default: "off" },
    Var { name: DRM_EXE_LOG, kind: Kind::Diagnostic, default: "off" },
    Var { name: SECTION_FILL_LOG, kind: Kind::Diagnostic, default: "off" },
    Var { name: SHIM_PANIC_LOG, kind: Kind::Diagnostic, default: "state dir" },
    Var { name: SHIM_BREADCRUMB, kind: Kind::Diagnostic, default: "off (no breadcrumb)" },
    Var { name: BENCH_LABEL, kind: Kind::Diagnostic, default: "\"run\"" },
    Var { name: SKYRIM_ZIP, kind: Kind::Harness, default: r"C:\tmp\skyrimse.zip" },
    Var { name: SKYRIM_DATA, kind: Kind::Harness, default: r"C:\tmp\skyrim-data" },
    Var { name: SKYRIM_ROOT, kind: Kind::Harness, default: r"C:\tmp\skyrim-runtime" },
    Var { name: SKYRIM_MODS, kind: Kind::Harness, default: "no overlay" },
    Var { name: SKYRIM_LAUNCH, kind: Kind::Harness, default: "SkyrimSE.exe" },
    Var { name: SKYRIM_DISK, kind: Kind::Harness, default: "use the archive" },
    Var {
        name: SKYRIM_NO_PROFILE_SEED,
        kind: Kind::Harness,
        default: "false (the harness seeds SkyrimPrefs.ini)",
    },
    Var { name: FIXTURE_PATH, kind: Kind::Fixture, default: "fixture-specific" },
    Var { name: FIXTURE_EXPECT, kind: Kind::Fixture, default: "none" },
    Var { name: FIXTURE_FILL, kind: Kind::Fixture, default: "none" },
    Var { name: FIXTURE_WRITE_PATH, kind: Kind::Fixture, default: "unset: no write" },
    Var { name: FIXTURE_WRITE_DATA, kind: Kind::Fixture, default: "written" },
    Var { name: FIXTURE_CACHE_PATH, kind: Kind::Fixture, default: "unset: no cache phase" },
    Var { name: FIXTURE_CACHE_RW_PATH, kind: Kind::Fixture, default: "unset: no rewrite" },
    Var { name: FIXTURE_CACHE_RW_DATA, kind: Kind::Fixture, default: "fresh" },
    Var { name: FIXTURE_LINGER_MS, kind: Kind::Fixture, default: "0" },
    Var { name: FIXTURE_SPAWN_CHILD, kind: Kind::Fixture, default: "unset: no child" },
    Var { name: FIXTURE_SLOW_PATH, kind: Kind::Fixture, default: "unset: no slow reads" },
    Var { name: FIXTURE_SLOW_THREADS, kind: Kind::Fixture, default: "1" },
    Var { name: FIXTURE_SLOW_STARTED, kind: Kind::Fixture, default: "unset: no wait" },
    Var { name: FIXTURE_SLOW_RELEASE, kind: Kind::Fixture, default: "unset: no cue" },
    Var { name: FIXTURE_THREADS, kind: Kind::Fixture, default: "4" },
    Var { name: FIXTURE_ROUNDS, kind: Kind::Fixture, default: "50" },
    Var { name: FIXTURE_NAMES, kind: Kind::Fixture, default: "unset: no names phase" },
    Var { name: FIXTURE_NAME_PREFIXES, kind: Kind::Fixture, default: "none" },
    Var { name: FIXTURE_NAME_LISTS, kind: Kind::Fixture, default: "none" },
    Var { name: FIXTURE_NAME_CREATES, kind: Kind::Fixture, default: "none" },
    Var { name: FIXTURE_NAME_RENAMES, kind: Kind::Fixture, default: "none" },
    Var {
        name: FIXTURE_COW_PATH,
        kind: Kind::Fixture,
        default: "unset (the in-place-edit step does not run)",
    },
    Var { name: ESCAPE_ONLY_VECTOR, kind: Kind::Fixture, default: "unset (every vector runs)" },
    Var { name: ESCAPE_ACCESS, kind: Kind::Fixture, default: "read" },
    Var {
        name: ESCAPE_VECTOR7_LINK_DIR,
        kind: Kind::Fixture,
        default: "unset (vector 7 constructs its own junction)",
    },
    Var { name: FIXTURE_INI_PATH, kind: Kind::Fixture, default: "none (required)" },
    Var { name: FIXTURE_INI_OUT, kind: Kind::Fixture, default: "unset (results go to stdout)" },
    Var { name: FIXTURE_INI_WRITE, kind: Kind::Fixture, default: "unset (read-only run)" },
    Var { name: FIXTURE_INI_SECTION, kind: Kind::Fixture, default: "Display" },
    Var { name: FIXTURE_INI_KEY, kind: Kind::Fixture, default: "sTest" },
    Var { name: FIXTURE_STEAM_API_DLL, kind: Kind::Fixture, default: "steam_api64.dll" },
    Var { name: FIXTURE_STEAM_OUT, kind: Kind::Fixture, default: "unset (stdout only)" },
    Var { name: FIXTURE_STEAM_INPUT, kind: Kind::Fixture, default: "on" },
    Var { name: FIXTURE_STEAM_MANIFEST, kind: Kind::Fixture, default: "unset (not called)" },
];

/// Is `name` a known switch?
pub fn is_known(name: &str) -> bool {
    ALL.iter().any(|v| v.name == name)
}

/// A switch that is **off unless explicitly turned on**.
///
/// True only for `1`, `true`, `yes` or `on`. Anything else — including an
/// unrecognised value — is false.
///
/// Use this for anything that relaxes a guarantee. `ALLOW_DISK_FALLTHROUGH`
/// un-seals the managed root, and it must not be possible to enable it by
/// accident: under the opposite rule, `=off` would read as *true*.
pub fn opt_in(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        Err(_) => false,
    }
}

/// A switch that is **on unless explicitly turned off**.
///
/// False only for `0`, `false`, `no` or `off`; anything else, including unset,
/// is true.
///
/// The two forms exist because the tree already had both, spelled inline and
/// inconsistently: some readers accepted only an affirmative, others rejected
/// only a negative, so the same string meant different things in different
/// crates. Naming the intent makes the call site say which one it wants.
pub fn opt_out(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => !matches!(v.to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
        Err(_) => true,
    }
}

/// A switch whose presence alone enables something, whatever its value.
pub fn present(name: &str) -> bool {
    std::env::var_os(name).is_some()
}

/// The value as a `String`, if set and valid UTF-8.
pub fn text(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// The value as an [`OsString`], if set.
pub fn raw(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

/// The value as a path, if set.
pub fn path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// A numeric switch, falling back to `default` when unset or unparsable.
pub fn parsed_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// The whole surface as a human-readable table, for `--help`-style output.
pub fn describe() -> String {
    let mut out = String::from("VFS environment switches:\n");
    for kind in [Kind::Handshake, Kind::Behaviour, Kind::Diagnostic, Kind::Harness, Kind::Fixture] {
        out.push_str(&format!("\n  {kind:?}\n"));
        for v in ALL.iter().filter(|v| v.kind == kind) {
            out.push_str(&format!("    {:<32} {}\n", v.name, v.default));
        }
    }
    out
}

/// The handshake: every name the host writes and the child reads, in one place.
///
/// Three crates had to agree on this set by hand: `vfs-proton`'s stale list,
/// the Windows `IpcServe::apply_env_roots`, and the reserved-name check for
/// `LaunchOpts::env`. They disagreed (`VFS_REGISTRY` and `VFS_FUSE_CFG` were in
/// some and not others). They now all read these tables, and a test ties the
/// tables to [`ALL`]: a new [`Kind::Handshake`] name that is in no table fails
/// the build of this crate's tests.
pub mod handshake {
    use super::*;

    /// The names a launch **sets or clears every time**, so a value inherited
    /// from an earlier session in the host process can never reach the child:
    /// the ring transport, its geometry, the roots, and the registry switch.
    pub const TRANSPORT: &[&str] = &[
        RING_PATH,
        RING_SECTION,
        RING_BYTES,
        RING_PAYLOAD_CAP,
        ARENA_OFFSET,
        ARENA_LEN,
        SERVER_EV,
        CLIENT_EV,
        FUSE_CFG,
        VIRTUAL_DIR,
        VIRTUAL_ROOTS,
        REGISTRY,
    ];

    /// The names only the injector reads. A Wine launch sets or clears them
    /// like [`TRANSPORT`]; the Windows in-process serve leaves them alone,
    /// because there the host may have set them for its own injector.
    pub const INJECT: &[&str] = &[INJECT_CWD, INJECT_STEAM_HELPER];

    /// The remaining handshake names: configuration the host hands the shim,
    /// the payload or the staging step. A launch does not set them, but
    /// `LaunchOpts::env` may not either.
    pub const CONFIG: &[&str] = &[
        STATE_DIR,
        HOME,
        LAUNCH_IMAGE,
        DISCOVERY_PATH,
        SHIM_CONFIG,
        SHIM_READY,
        PAYLOAD_PATH,
        PAYLOAD_CFG_FILE,
        DUAL_LAYER,
    ];

    /// Every handshake name.
    pub fn all() -> impl Iterator<Item = &'static str> {
        TRANSPORT.iter().chain(INJECT).chain(CONFIG).copied()
    }

    /// Whether `name` is a handshake name. ASCII case-insensitive: Wine hands
    /// the Windows side an environment whose names compare without case.
    pub fn is_handshake(name: &str) -> bool {
        all().any(|n| n.eq_ignore_ascii_case(name))
    }

    /// The names a Wine launch must remove from the child's inherited
    /// environment: every [`TRANSPORT`] and [`INJECT`] name `is_set` says the
    /// launch did not set itself.
    pub fn stale<'a>(is_set: impl Fn(&str) -> bool + 'a) -> impl Iterator<Item = &'static str> + 'a {
        TRANSPORT.iter().chain(INJECT).copied().filter(move |n| !is_set(n))
    }

    /// The `id=location;id=location` encoding of `VFS_VIRTUAL_ROOTS`, or `None`
    /// for no extra roots (the variable is then unset, not empty).
    pub fn encode_roots(roots: &[(u32, String)]) -> Option<String> {
        if roots.is_empty() {
            return None;
        }
        Some(
            roots
                .iter()
                .map(|(id, loc)| format!("{id}={loc}"))
                .collect::<Vec<_>>()
                .join(";"),
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_tables_cover_exactly_the_handshake_names() {
            let mut table: Vec<&str> = all().collect();
            let mut listed: Vec<&str> = ALL
                .iter()
                .filter(|v| v.kind == Kind::Handshake)
                .map(|v| v.name)
                .collect();
            let n = table.len();
            table.sort_unstable();
            table.dedup();
            assert_eq!(n, table.len(), "a name is in two handshake tables");
            listed.sort_unstable();
            assert_eq!(table, listed, "handshake tables and Kind::Handshake disagree");
        }

        #[test]
        fn registry_and_fuse_cfg_are_launch_cleared_handshake_names() {
            for n in [REGISTRY, FUSE_CFG, RING_PATH, INJECT_CWD] {
                assert!(is_handshake(n), "{n}");
                assert!(stale(|_| false).any(|s| s == n), "{n} not cleared");
            }
            assert!(is_handshake("vfs_registry"), "case-insensitive");
            assert!(!is_handshake("VFS_BENCH"));
        }

        #[test]
        fn stale_skips_what_the_launch_set() {
            let s: Vec<_> = stale(|n| n == REGISTRY || n == RING_PATH).collect();
            assert!(!s.contains(&REGISTRY) && !s.contains(&RING_PATH));
            assert!(s.contains(&RING_SECTION));
        }

        #[test]
        fn roots_encode_as_id_equals_location_joined_by_semicolons() {
            assert_eq!(encode_roots(&[]), None);
            assert_eq!(
                encode_roots(&[(1, r"C:\a".into()), (2, r"C:\b".into())]).as_deref(),
                Some(r"1=C:\a;2=C:\b")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_is_unique_and_prefixed() {
        let mut seen = std::collections::BTreeSet::new();
        for v in ALL {
            assert!(v.name.starts_with("VFS_"), "{} is not VFS_-prefixed", v.name);
            assert!(seen.insert(v.name), "{} listed twice", v.name);
        }
    }

    #[test]
    fn opt_in_requires_an_affirmative() {
        let n = "VFS_TEST_OPT_IN";
        for yes in ["1", "true", "TRUE", "yes", "on", "On"] {
            std::env::set_var(n, yes);
            assert!(opt_in(n), "{yes:?} should enable");
        }
        // The important half: nothing else enables it, however it is spelled.
        for no in ["0", "false", "no", "off", "", "2", "maybe"] {
            std::env::set_var(n, no);
            assert!(!opt_in(n), "{no:?} must not enable an opt-in switch");
        }
        std::env::remove_var(n);
        assert!(!opt_in(n));
    }

    #[test]
    fn opt_out_requires_a_negative() {
        let n = "VFS_TEST_OPT_OUT";
        for no in ["0", "false", "FALSE", "no", "off", "Off"] {
            std::env::set_var(n, no);
            assert!(!opt_out(n), "{no:?} should disable");
        }
        for yes in ["1", "true", "yes", "on", "anything"] {
            std::env::set_var(n, yes);
            assert!(opt_out(n), "{yes:?} should leave it enabled");
        }
        std::env::remove_var(n);
        assert!(opt_out(n), "unset means on");
    }

    /// `off` used to read as *true* under the denylist form and `on` as *false*
    /// under the allowlist form. Both are now recognised by the side that means
    /// them, which is the one behaviour change this module makes deliberately.
    #[test]
    fn on_and_off_are_understood_by_both_forms() {
        let n = "VFS_TEST_ON_OFF";
        std::env::set_var(n, "off");
        assert!(!opt_out(n));
        assert!(!opt_in(n));
        std::env::set_var(n, "on");
        assert!(opt_out(n));
        assert!(opt_in(n));
        std::env::remove_var(n);
    }

    #[test]
    fn describe_lists_every_switch() {
        let text = describe();
        for v in ALL {
            assert!(text.contains(v.name), "{} missing from describe()", v.name);
        }
    }

    /// The guard that makes this module worth having: every `VFS_*` name the
    /// workspace mentions must be in [`ALL`].
    ///
    /// This is what a rename like `VFS_HOLLOW_HOST` → `VFS_LAUNCH_IMAGE` needs.
    /// That one changed the writer and not the reader, and nothing failed —
    /// the staging alias just stopped resolving.
    #[test]
    fn no_crate_reads_a_switch_that_is_not_registered() {
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates dir");
        let mut unknown: Vec<String> = Vec::new();
        let mut files = 0usize;
        visit(crates, &mut |path, text| {
            files += 1;
            for name in scan_names(text) {
                // This crate defines them; fixtures under tests/ may invent
                // throwaway names for their own harness.
                if !is_known(&name) && !name.starts_with("VFS_TEST_") {
                    unknown.push(format!("{} in {}", name, path.display()));
                }
            }
        });
        assert!(files > 20, "scanned only {files} files — did the walk break?");
        assert!(
            unknown.is_empty(),
            "these VFS_* names are read but not registered in vfs-env::ALL:\n  {}",
            unknown.join("\n  ")
        );
    }

    /// Every `VFS_[A-Z0-9_]+` token in `text`.
    fn scan_names(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let bytes = text.as_bytes();
        let mut i = 0;
        while let Some(rel) = text[i..].find("VFS_") {
            let start = i + rel;
            let mut end = start + 4;
            // Part of a longer identifier (`AETHER_VFS_…`), not a switch of ours.
            let embedded = start > 0
                && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_');
            while end < bytes.len()
                && (bytes[end].is_ascii_uppercase() || bytes[end].is_ascii_digit() || bytes[end] == b'_')
            {
                end += 1;
            }
            let tok = &text[start..end];
            // Prose writes families as `VFS_*` or `VFS_RING_*`. A bare prefix
            // is not a name, so require a suffix that does not end in `_`.
            let is_prefix = tok.len() == 4 || tok.ends_with('_');
            if !is_prefix && !embedded {
                out.push(tok.to_string());
            }
            i = end;
        }
        out
    }

    fn visit(dir: &std::path::Path, f: &mut impl FnMut(&std::path::Path, &str)) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
                if name == "target" {
                    continue;
                }
                visit(&p, f);
            } else if p.extension().and_then(|s| s.to_str()) == Some("rs") {
                // Skip this file: it necessarily contains every name.
                if p.ends_with("vfs-env/src/lib.rs") {
                    continue;
                }
                if let Ok(text) = std::fs::read_to_string(&p) {
                    f(&p, &text);
                }
            }
        }
    }
}

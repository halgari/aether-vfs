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
/// Absolute path of the image to launch, normally the staged EXE. The shim also
/// derives the staging directory from this, and serves that directory as an
/// alias for the virtual root.
pub const LAUNCH_IMAGE: &str = "VFS_LAUNCH_IMAGE";
/// Where the daemon publishes its endpoint for clients to discover.
pub const DISCOVERY_PATH: &str = "VFS_DISCOVERY_PATH";
/// Seconds to wait for the child's hooks to report ready before giving up.
pub const READY_TIMEOUT_SECS: &str = "VFS_READY_TIMEOUT_SECS";

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
    Var { name: LAUNCH_IMAGE, kind: Kind::Handshake, default: "none; staging derives it" },
    Var { name: DISCOVERY_PATH, kind: Kind::Handshake, default: "platform default" },
    Var { name: READY_TIMEOUT_SECS, kind: Kind::Behaviour, default: "built-in timeout" },
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
    Var { name: WAIT, kind: Kind::Behaviour, default: "false (detach)" },
    Var { name: BENCH, kind: Kind::Behaviour, default: "false" },
    Var { name: SHIM_STATS_LOG, kind: Kind::Diagnostic, default: "off" },
    Var { name: SHIM_STATS_INTERVAL_MS, kind: Kind::Diagnostic, default: "250" },
    Var { name: DIRECTOR_OPEN_LOG, kind: Kind::Diagnostic, default: "off" },
    Var { name: DRM_EXE_LOG, kind: Kind::Diagnostic, default: "off" },
    Var { name: SECTION_FILL_LOG, kind: Kind::Diagnostic, default: "off" },
    Var { name: SHIM_PANIC_LOG, kind: Kind::Diagnostic, default: "state dir" },
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
            while end < bytes.len()
                && (bytes[end].is_ascii_uppercase() || bytes[end].is_ascii_digit() || bytes[end] == b'_')
            {
                end += 1;
            }
            let tok = &text[start..end];
            // Prose writes families as `VFS_*` or `VFS_RING_*`. A bare prefix
            // is not a name, so require a suffix that does not end in `_`.
            let is_prefix = tok.len() == 4 || tok.ends_with('_');
            if !is_prefix {
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
                if p.ends_with("vfs-env/src/lib.rs") || p.file_name().and_then(|s| s.to_str()) == Some("lib.rs")
                    && p.parent().and_then(|d| d.parent()).and_then(|d| d.file_name())
                        .and_then(|s| s.to_str()) == Some("vfs-env")
                {
                    continue;
                }
                if let Ok(text) = std::fs::read_to_string(&p) {
                    f(&p, &text);
                }
            }
        }
    }
}

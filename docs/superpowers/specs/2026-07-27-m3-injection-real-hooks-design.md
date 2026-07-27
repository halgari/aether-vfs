# M3 — JVM-driven injection, real hooks, end-to-end read

**Date:** 2026-07-27
**Status:** Approved (design); plan pending
**Predecessors:** M1 (merge + anti-drift), M2 (JVM FFM ring server) — both complete, on `master`.
**Parent design:** `docs/superpowers/specs/2026-07-26-unified-cross-platform-vfs-design.md`

## What

Replace M2's Rust test harness with a **real injected shim in a real target
process**. The JVM (M2 ring server) becomes the daemon behind the shim: it
creates the shared section, launches a target with the shim injected, and serves
the target's hooked file operations (`NtCreateFile`/`NtReadFile`/
`NtQueryDirectory`) from a Clojure `Provider` over the ring. The milestone is
proven when a normal Windows process opens a virtual file it did not create,
under a configured virtual root, and reads bytes that only exist in the JVM
`Provider`.

M3 targets the **read path** through **classic injection** (spawn suspended →
`LoadLibrary` the shim → resume), in **pure-ring** mode (every getattr/readdir/
open/read/close goes to the JVM — no local file tree in the shim).

## Decisions (as approved)

1. **Reuse the existing engine, extract a generic core.** The shim, injector,
   and ring machinery already exist (`vfs-shim`, `vfs-inject`, `vfs-ipc`,
   `vfs-win`). `vfs-launch` is the *game-specific* driver (zip layers, SKSE,
   Skyrim paths). M3 extracts a **generic `launch + inject + serve-ring` core**
   — "spawn a target with the shim injected, pointed at a named ring section" —
   from the game specifics, which stay in a separate layer off M3's path.
2. **Pure-ring.** Every getattr/readdir/open/read/close goes to the JVM. The
   shim's existing `FuseClient` (`vfs-shim/src/fuse_client.rs`) already does
   this: it classifies opens by **root prefix** (`vpath_under_root_norm`, no
   snapshot) and serves all ops over the ring. M3 makes the shim run
   FuseClient-authoritatively with a minimal/empty local snapshot.
3. **JVM owns the lifecycle** (per the parent design): the JVM creates the
   section (M2 `section.clj`), sets the shim's env, and invokes a Rust injector;
   the injector only does spawn-suspend / inject / resume.
4. **Classic injection, read path.** No pre-init/dual-layer/static-import paths
   (those exist for statically-imported DLLs / game renderers — not needed to
   read a data file). Write path is M4.
5. **Version handshake enforced.** The shim already validates the ring
   magic/version on `ring::open`; M3 enforces the descriptor protocol version so
   a stale shim vs a newer JVM server fails loudly rather than mis-decoding.

## How the existing shim already fits (what M3 leverages)

- `fuse_client.rs::FuseClient` — reads `VFS_RING_SECTION`, `VFS_RING_BYTES`,
  `VFS_RING_PAYLOAD_CAP`, `VFS_ARENA_LEN`, `VFS_VIRTUAL_DIR` from env;
  `SharedMapping::open` + `RingClient`; getattr/readdir/open/`read_fragmented`
  (inline + bulk arena) / close over the ring; `vpath_under_root_norm` for
  root-prefix classification. `hook.rs` already routes the NT detours to
  `fuse_client::global()`.
- `bootstrap.rs` — the injected DLL's `DllMain` bootstraps from a
  `VFS_SHIM_CONFIG` file `(root, overlay, snapshot)`, **always builds an
  `Engine(root, snapshot)`**, and calls `try_init_from_env()` to attach the
  FuseClient (best-effort). `try_init_from_env` calls `heartbeat()` (`OP_HEARTBEAT`).

## The two facts that shape M3 (from reading the shim)

1. **`OP_HEARTBEAT` is mandatory on the JVM side.** `try_init_from_env` does
   `client.heartbeat()?` *before* publishing the FuseClient; if the JVM server
   doesn't answer `OP_HEARTBEAT` with `ST_OK`, the FuseClient is never
   installed and the shim falls back to its (empty) engine — virtual files are
   not served. **M2's server does not handle `OP_HEARTBEAT`** (it hits the
   `BAD_REQUEST` default). Adding it is a required, tiny change.
2. **`VFS_SHIM_CONFIG` (a file) is always read** and an `Engine(root, snapshot)`
   is always built. For pure-ring the JVM supplies a config with the virtual
   `root` and an **empty snapshot**. Whether the hook then serves ring-backed
   files correctly with an empty snapshot (FuseClient-authoritative) — or
   whether it short-circuits on the empty engine before consulting the
   FuseClient — is **the one real unknown**, resolved by the M3 de-risk spike
   (milestone 1). If the hook needs it, a small "fuse-authoritative" tweak makes
   paths under root consult the FuseClient before the snapshot.

## Architecture

```
   JVM process (Clojure)                                  Target process (real exe)
   ┌───────────────────────────────────────┐             ┌──────────────────────────────┐
   │ os/windows/launch.clj                  │  env vars   │ injected vfs-shim DLL          │
   │  1. section/create + ring/init         │────────────▶│  DllMain → bootstrap:          │
   │  2. write VFS_SHIM_CONFIG (root, [])    │  section    │   Engine(root, empty snapshot) │
   │  3. set VFS_RING_* / VFS_VIRTUAL_DIR    │◀──ring+─────│   + FuseClient (ring client)   │
   │  4. invoke Rust injector (spawn susp.,  │   arena     │  hooks NtCreateFile/NtReadFile │
   │     LoadLibrary shim, resume)           │  (shared)   │   → vpath_under_root → ring    │
   │  5. server/serve(provider)  ◀───────────┼─────────────┤ target opens C:\<root>\hello   │
   │  6. wait for target; teardown           │             │  → reads JVM Provider bytes    │
   └───────────────────────────────────────┘             └──────────────────────────────┘
```

## Components

### Rust — generic injector (extract + thin bin)
Extract from `vfs-inject`/`vfs-launch` a **generic** injection entry that is not
game-coupled, exposed as a small bin the JVM invokes:
`vfs-injector <target-exe> <shim-dll> [-- target-args…]`, reading the section/
config/root from the env the JVM sets. Responsibilities: `CreateProcess`
suspended → inject the shim DLL (classic `LoadLibrary` via the validated
recipe) → resume → wait for `VFS_SHIM_READY` → propagate the target's exit code.
The game-specific `vfs-launch` (zip/SKSE) is refactored to sit *on top of* this
core, not intermixed with it.

### Rust — `OP_HEARTBEAT` awareness
No change needed in Rust (the shim already sends it). The gap is JVM-side (below).

### Rust — read fixture (`vfs-fixture-read`, new)
A minimal exe: opens `%VFS_FIXTURE_PATH%` (a path under the virtual root),
reads it fully, compares against an expected value passed via env/arg, exits 0
on match / non-zero otherwise. Analogous to the existing `vfs-fixture-staticimp`
but for a plain data-file read. This is the *target* the injector launches.

### Clojure — `os/windows/launch.clj`
`launch [provider {:keys [target-exe root section-name expected …]}]`:
creates the section (M2 `section/create` + `ring/init` + arena), writes the
`VFS_SHIM_CONFIG` file (encode `root` + empty snapshot — a tiny Clojure encoder
mirroring `vfs-shim`'s `encode_config`), sets the env map
(`VFS_RING_SECTION`/`VFS_RING_BYTES`/`VFS_RING_PAYLOAD_CAP`/`VFS_ARENA_LEN`/
`VFS_VIRTUAL_DIR`/`VFS_SHIM_CONFIG`/`VFS_SHIM_READY`), spawns the Rust injector
(`ProcessBuilder`) with that env + the target exe, runs `server/serve` with the
`provider` on a thread, waits for the target's exit, and tears down (stop server,
`section/close!`, temp files). Windows-only.

### Clojure — `server.clj`: `OP_HEARTBEAT`
Add `OP_HEARTBEAT (13)` → `{:status ST_OK :payload (byte-array 0)}` in
`dispatch`. Small, but required for the FuseClient to attach.

### Clojure — config encoder + geometry
A `os/windows/shim_config.clj` (or a fn in `launch.clj`) encoding the
`VFS_SHIM_CONFIG` bytes: `[u32 root_len][root][u32 overlay_len=0][snapshot=…]`
mirroring `vfs-shim::encode_config` (empty overlay, empty/minimal snapshot).
The section geometry the JVM inits (payload_cap, slot_count, arena_len) is
written into the matching `VFS_RING_*` env vars so the shim's `SharedMapping::
open` + `ring::open` see a consistent layout.

### Version handshake
The descriptor already carries `:version` + `:content-hash` (M1). M3 enforces
the ring `version` (already checked by `ring::open`) and, if cheap, stamps the
descriptor `:content-hash` into a header/handshake slot the shim checks on
connect — so a shim built against a different protocol fails with a clear error
rather than mis-decoding. (Scope this minimally: version check is the floor;
hash check is the stretch.)

## De-risk spike (milestone 1 — resolves the one unknown before productionizing)

Before building the clean `launch.clj`, prove the path end-to-end with the
*existing* shim and injector, driven by the JVM, in the crudest working form:
JVM creates the section + an empty-snapshot config + env, adds `OP_HEARTBEAT`,
invokes the existing injector against the read fixture, and observes whether the
fixture reads the JVM-served bytes. Outcome determines whether the hook serves
pure-ring with an empty snapshot as-is, or needs the small fuse-authoritative
tweak. Everything downstream (the productionized `launch.clj`, the generic
injector extraction) builds on the spike's confirmed mechanism.

## Testing & proof

- **Windows-only end-to-end proof (`windows-clojure` CI job):** a Clojure
  `deftest` (self-skips off Windows) calls `launch.clj` with a `Provider`
  serving `hello.txt` (small) + a `>64 KiB` file, launching the read fixture
  pointed at `C:\<root>\hello.txt` (and the big file); asserts the fixture exits
  0 — i.e. a real process read JVM-`Provider` bytes through real
  `NtCreateFile`/`NtReadFile` hooks (inline + bulk). This is the M2 cross-process
  proof upgraded from "test harness client" to "real injected shim in a real
  target."
- **Rust unit tests** for the extracted generic injector (arg/env parsing,
  suspend/resume/ready sequencing where testable) and the config encoder
  round-trip (Clojure encoder bytes == `vfs-shim::decode_config` — a
  cross-language pin like M1's golden vectors).
- **Cross-platform** component tests (config encoder) run in the ubuntu job;
  the injection proof is Windows-only.
- **Load-safety:** any new `os/windows/*` Clojure namespace with native lookups
  must defer them (lazy) so the full suite loads on Linux — the M2 `section.clj`
  regression lesson; guard it.

## Risks

- **The empty-snapshot pure-ring behavior** (the one real unknown) — front-loaded
  as the milestone-1 spike; fallback is a small fuse-authoritative hook change.
- **Injection timing / flakiness** — classic `LoadLibrary`-into-suspended is
  de-risked in prior spikes, but injection + two spinning processes on a 2-core
  CI runner can be timing-sensitive; the proof must synchronize on
  `VFS_SHIM_READY` (which `signal_ready` writes) before the target runs, and the
  server thread must be serving before resume.
- **Geometry drift** between JVM `section/create` and the shim env vars — pin
  the geometry in one place (the launch opts) and derive both the ring init and
  the env from it.
- **Refactor blast radius** — extracting the generic core from `vfs-launch`
  must not break the existing game path; keep `vfs-launch` compiling/passing by
  layering it on the extracted core, not rewriting it.

## Milestones (tasks — detailed in the plan)

1. **De-risk spike** — JVM drives the *existing* injector + shim against the
   read fixture with an empty-snapshot config + `OP_HEARTBEAT`; confirm (or fix
   via a fuse-authoritative tweak) that a real process reads JVM-served bytes.
2. **`OP_HEARTBEAT`** on the JVM server (+ test).
3. **Read fixture** (`vfs-fixture-read`) + config encoder (Clojure) with a
   cross-language round-trip test vs `vfs-shim::decode_config`.
4. **Generic injector** — extract the game-agnostic `spawn+inject+resume+ready`
   core into a `vfs-injector` bin (refactor `vfs-launch` to layer on it; keep it
   green).
5. **`launch.clj`** — the productionized Clojure launch/serve/teardown over the
   generic injector, replacing the spike's crude form.
6. **End-to-end proof + CI** — the Windows-only `deftest` (inline + bulk read
   through real hooks) wired into the `windows-clojure` job.
7. **Version handshake** — enforce protocol version (floor) / descriptor hash
   (stretch) at shim connect.

## Out of scope (unchanged)
- Write path — create/unlink/rename/truncate/whiteout (M4).
- Pre-init / dual-layer / static-import injection (classic only for M3).
- Child-process propagation (`CreateProcessInternalW` hooking) — later.
- Unified `mount`/`launch` public entry (M5).
- OS event notifier (spin only).
- Full game launch (SKSE/zip layers) — stays in the game-specific layer.

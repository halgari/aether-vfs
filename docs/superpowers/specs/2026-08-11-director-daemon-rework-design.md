# Director Daemon Rework — Design Spec

**Date:** 2026-08-11
**Status:** Implemented through M4 (read-only path). Overlay CoW writes remain partial (read-side whiteouts only).
**Scope:** Windows-first. Linux (director outside Proton) is a later phase, explicitly deferred.

## 1. Goal

Turn the director into a standalone, long-lived **Rust daemon** that is the central
connection point for the whole VFS. Clients (any language) talk to it over a
cross-platform control API to: start the daemon, create a **session**, add
**source mappings** to that session, and **launch executables tied to the
session**. The filesystem a launched process sees is **composed** from pluggable
**sources** ("filesystem shards"). The director owns the **block cache** and
performs all caching. The Clojure codebase is deleted; everything moves to Rust.

## 2. Locked decisions

| Fork | Decision |
|------|----------|
| Control-plane transport | **gRPC (tonic)** — schema-first `.proto`, cross-language codegen, streaming events. Control ops are low-frequency; overhead is irrelevant. |
| Source-plugin model | **Hybrid** — fast built-ins (zip/disk/http) are in-process Rust `Backend`s; arbitrary-language sources implement the **same op schema** out-of-process. Backends are interchangeable. |
| Daemon/session lifetime | **Multi-session per-user daemon** — one daemon holds many sessions; a client auto-spawns it if absent; a session is torn down when its launched process tree exits. Block cache is shared across sessions. |

### Also decided
- **Read-only composition first.** Overlay copy-on-write / the write path is
  **deferred** to a dedicated later milestone; M1 ships read-only `router` +
  `layered` only. The block cache and the whole read pipeline assume immutable
  sources for now.
- **gRPC is the only entry point.** The in-process C ABI is removed:
  `vfs-director/src/ffi.rs` and `include/vfs.h` are deleted and the `pub mod ffi`
  export dropped. (Verified: nothing outside that crate references `vfs_director_*`
  or `vfs_launch`.) No in-process embedding mode going forward.

### Secondary calls (my defaults — change if you disagree)
- **Out-of-process source transport = gRPC too.** One RPC stack, one codegen story. In-proc backends implement the Rust trait directly; a `RemoteBackend` adapts a gRPC `SourceService` client to that trait. (Alternative considered: the existing binary ring for lower latency — rejected because external sources are network-bound, and local zip/disk stay in-proc anyway.)
- **Data plane unchanged.** The shared-memory control ring + bulk arena + inject/payload/shim/PE-hollow stack is kept as-is and re-homed under the daemon. It is the hard part and it works.
- **Block cache = fixed-size blocks**, default 1 MiB (configurable), keyed by `(source_id, file_id, block_index)` where `file_id` is a stable identity the source returns on `open` (etag, or `mtime+size`). Two tiers: bounded RAM LRU + optional on-disk cache dir. Cache assumes immutable sources (read-only phase); invalidation returns with the write path.

## 3. Architecture

```text
           gRPC control plane (tonic, named pipe / loopback)
                              │
      ┌───────────────────────▼───────────────────────────────┐
      │                 vfs-directord (daemon)                 │
      │  Session registry:  id → Session                       │
      │  Session = { Director kernel, IpcServe ring, proctree } │
      │  Process-wide:  BlockCache,  SourceRegistry             │
      └───┬───────────────────────────────┬────────────────────┘
          │ in-proc Backend               │ gRPC SourceService (out-of-proc)
   ┌──────▼──────┐  ┌──────────┐    ┌──────▼─────────────┐
   │ ZipBackend  │  │ Disk...  │    │ RemoteBackend ↔    │  (any language)
   └─────────────┘  └──────────┘    │  Steam/Nexus/REST  │
                                    └────────────────────┘
          │ data plane: shared-mem ring + bulk arena (unchanged)
   ┌──────▼───────────────────────────────────────────────────┐
   │  Launched game process tree (dual-layer inject + hollow)  │
   └───────────────────────────────────────────────────────────┘
```

- **Session** = one `Director` kernel (the FS composition) + one `IpcServe` (its ring to the injected process tree) + the launched process tree it owns. Modeled on today's `vfs-director::Session`, but many live at once inside the daemon.
- **SourceRegistry** = process-wide table of live sources (in-proc and remote), reference-counted so multiple sessions can share one source instance (and its cache footprint).
- **BlockCache** = process-wide, shared across sessions and sources.

## 4. Crate layout

**New**
- `vfs-directord` — the daemon binary: tonic server, session registry, lifecycle, auto-spawn/discovery, teardown-on-exit.
- `vfs-control` — control-plane `.proto` + generated types + a thin Rust client (reference client).
- `vfs-source` — the source op schema: the `Backend` trait (moved/re-exported from `vfs-protocol`), the `SourceService` `.proto`, the `RemoteBackend` gRPC↔trait adapter, and the **source conformance suite**.
- `vfs-cache` — the block cache (RAM + disk tiers, keying, invalidation, metrics).
- `vfs-compose` — composition ported from Clojure: `router` (glob→source), `layered` (top-wins), `overlay` (CoW + `.wh.` whiteouts). Built on `vfs-core`'s existing layered/tombstone tree where it fits.

**Keep (re-home, minimal change)**
- `vfs-director` (kernel: mount/getattr/readdir/open/read/close), `vfs-protocol`, `vfs-ipc`, `vfs-win`, `vfs-zip`, `vfs-inject`, `vfs-payload`, `vfs-shim`, `vfs-shim-dll`, `vfs-shared`.

**Evaluate for deletion/merge**
- `vfs-core` / `vfs-server` / `vfs-redirect` are marked legacy in the overview. Salvage the layered-tree logic into `vfs-compose`; delete the rest once parity tests pass.
- `vfs-launch` (Skyrim CLI host) → replaced by a reference CLI client over gRPC.

**Delete (Clojure)**
- All of `src/**.clj`, `test/**.clj`, `deps.edn`, `build.clj`, the Clojure CI job. The jar's native-bundling role is replaced by daemon packaging.

## 5. Control protocol (gRPC sketch)

```proto
service Director {
  rpc Health(HealthReq) returns (HealthResp);
  rpc CreateSession(CreateSessionReq) returns (Session);           // → session_id
  rpc AddSource(AddSourceReq) returns (SourceRef);                 // built-in or remote
  rpc SetComposition(SetCompositionReq) returns (Empty);           // router/layer/overlay wiring
  rpc Launch(LaunchReq) returns (stream LaunchEvent);              // started, stdout marker, exited(code)
  rpc TeardownSession(TeardownReq) returns (Empty);
  rpc ListSessions(Empty) returns (SessionList);
}

message AddSourceReq {
  string session_id = 1;
  string mount = 2;                 // prefix or glob
  oneof source {
    ZipSource zip = 3;              // built-in
    DiskSource disk = 4;            // built-in
    HttpSource http = 5;            // built-in REST-ish
    RemoteSource remote = 6;        // out-of-proc plugin endpoint (any language)
  }
  int32 layer = 7;                  // precedence for layered/overlay
}
```

Transport: **named pipe on Windows** (`\\.\pipe\vfs-director-<user>`), Unix domain socket on Linux later. Loopback TCP is a fallback for remote/dev.

## 5.1 CLI & config-driven setup

One binary, `vfs` (the reference client + daemon launcher), drives everything.
Two ways to stand the system up, so scenarios are reproducible for testing:

**A. Config-driven (declarative).** `vfs up --config scenario.toml` parses one file
that fully describes a session — sources, mounts, layers, launch target, cache —
and translates it into control RPCs (`CreateSession` → `AddSource`×N → `Launch`).
`vfs down` tears it back down. This is the primary harness for integration tests
and for hand-running real game layouts. TOML is the primary format; JSON is also
accepted (detected by extension) so other languages can generate scenarios.

```toml
# scenario.toml
[session]
name = "skyrim-test"           # optional; root/state dirs default under temp

[[source]]
type  = "zip"                  # built-in: zip | disk | http | remote(later)
path  = "C:/GameLayers/1. Skyrim Special Edition.zip"
mount = "/"
layer = 0                      # lower = base; higher wins (layered precedence)

[[source]]
type  = "disk"
path  = "C:/mods/SkyUI"
mount = "/"
layer = 20

[launch]
exec      = "SkyrimSE.exe"
args      = []
wait      = true
hollow_pe = true

[cache]                        # honored from M2; ignored earlier
block_size = "1MiB"
ram_budget = "512MiB"
dir        = "C:/vfs-cache"
```

**B. Flag-driven (imperative).** The same setup without a file, for quick tests:

```
vfs launch \
  --source zip:"C:/GameLayers/1. Skyrim Special Edition.zip"@/#0 \
  --source disk:"C:/mods/SkyUI"@/#20 \
  --exec SkyrimSE.exe --wait
```

`--source TYPE:PATH@MOUNT#LAYER` repeats; `#LAYER` and `@MOUNT` default to `0`/`/`.

**Subcommands (M0 subset in bold):**
`vfs` **`daemon`** (run the daemon in the foreground — tests/debugging; otherwise
auto-spawned) · **`up --config`** / `down` · **`launch`** (flag-driven) ·
`session ls` / `session rm` · **`health`** · `sources` (list registered source
types) · `stats` (cache metrics, M2). Global `--endpoint` overrides discovery.

The config schema is a serde type in `vfs-control` so the daemon, CLI, and tests
share one definition; the integration test (M0.8) drives the e2e path via a
`scenario.toml` so the config surface is exercised from day one.

## 6. Source protocol & conformance

The in-proc contract is the existing trait (add cache-relevant identity to `open`):

```rust
pub trait Backend: Send + Sync {
    fn getattr(&self, path: &str) -> Result<Option<Stat>, i32>;
    fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, i32>;
    fn open(&self, path: &str, flags: u32) -> Result<OpenInfo, i32>; // + file_id for cache keying
    fn read(&self, bh: BackendHandle, offset: u64, buf: &mut [u8]) -> Result<usize, i32>;
    fn release(&self, bh: BackendHandle) -> Result<(), i32>;
}
```

The out-of-proc `SourceService` mirrors this op-for-op over gRPC (server-streaming
`Read` for large files). `RemoteBackend` implements `Backend` by forwarding.

**Conformance suite** (validation lever): one battery of behavioral tests that
*every* source must pass — run against in-proc backends directly and against a
reference out-of-proc plugin through `RemoteBackend`. Covers: path
normalization/casefold, `getattr` on file/dir/missing, `readdir` union &
ordering, offset/short/oversized reads, EOF, concurrent reads, error mapping
(`not_found`/`not_a_dir`/`is_dir`/`bad_fh`), and `file_id` stability. A new
source in any language is "done" when it passes this suite.

## 7. Block cache

- Fixed block size (default 1 MiB, configurable). Reads are block-aligned; the
  director requests whole blocks from a source and slices for the caller.
- Key: `(source_id, file_id, block_index)`. `file_id` comes from `open`.
- Tiers: bounded RAM LRU (byte budget) → optional on-disk cache dir (content
  file per block, or a packed store). Miss path fills from the source.
- Invalidation: overlay writes to a file drop that file's blocks; `file_id`
  change (etag/mtime) invalidates transparently.
- Metrics: hit/miss/evict counters, bytes served from cache vs. source; exposed
  via a `Stats` RPC for tests and tuning.

## 8. Composition (Clojure → Rust)

Port these with parity tests seeded from the existing Clojure test cases:
- **router** — first glob match wins, else default (was `router.clj`).
- **layered** — two+ read-only sources, top-wins (was `layered.clj`).
- **overlay** — CoW: reads merge upper-over-base; deletes recorded as `.wh.*`
  whiteouts; first write copies a file up to a writable dir; base never mutated
  (was `overlay.clj`). Writes flow here; overlay signals the cache to invalidate.
- `compose` helpers (`build-data-root`, `build-inline-root`, …) → Rust builders.

## 9. Testing & validation strategy

- **Unit** per crate (kernel resolve/override, cache keying/eviction, each composer).
- **Source conformance suite** (§6) — in-proc and out-of-proc, the gate for any new source.
- **Integration** — daemon up → create session → add sources → launch a fixture
  that reads/writes through the ring → assert bytes and exit code. Reuse the
  existing `vfs-fixture-*` binaries and the injection stack.
- **Cross-language proof** — a second-language reference source plugin (e.g. a
  tiny Python/Node REST proxy) passing the conformance suite through `RemoteBackend`.
- **Concurrency/fault** — many in-flight reads, source that errors/stalls, plugin
  process that crashes mid-session (director must isolate to an errno, not die).
- **Perf** — cache hit vs. cold source, ring throughput regression vs. today's benchmarks.

## 10. Milestones (Windows-first)

- **M0 — Daemon skeleton + control plane.** tonic server over named pipe;
  `CreateSession`/`AddSource(disk)`/`Launch`/`Teardown`; launch reuses the
  existing inject/ring. End-to-end: reference client creates a session, adds a
  disk source, launches `vfs-fixture-read`, asserts bytes + exit. No Clojure in
  this path.
- **M1 — Read-only sources & composition in Rust.** Wire `ZipBackend`; port
  `router` + `layered` into `vfs-compose` with parity tests migrated from the
  Clojure suite. **Overlay/CoW deferred** (see M-Write).
- **M2 — Block cache.** `vfs-cache` with RAM+disk tiers, invalidation, `Stats`;
  correctness + concurrency + eviction tests; perf benchmark vs. direct.
- **M3 — Out-of-proc source protocol.** `SourceService` proto, `RemoteBackend`,
  the conformance suite, a Rust reference plugin + a second-language plugin
  proving cross-language.
- **M4 — Delete Clojure + package.** Remove `src/`, `test/`, `deps.edn`,
  `build.clj`, Clojure CI; update README/specs; daemon packaging that bundles the
  natives (replacing the jar's role).
- **M-Write — Overlay & write path.** Port overlay CoW + `.wh.` whiteouts into
  `vfs-compose`; add cache invalidation on write. Slots after M2/M3 (the block
  cache and source protocol assume immutable sources until this lands).
- **Later — Linux.** Director runs outside Proton; game process tree under Proton
  reaches the daemon over the Linux control/data transport.

## 11. Resolved / remaining
Resolved: read-only composition first (writes → M-Write); gRPC is the only entry
point (C ABI removed); tonic/tokio accepted (sync kernel called from blocking
tasks).

Remaining (proceeding on the stated default unless changed):
- Daemon discovery/auth: per-user endpoint with a default-deny ACL; no extra
  token/handshake beyond OS transport security for now.
- **M0 transport:** loopback TCP on `127.0.0.1:0` with a per-user discovery file
  (endpoint + pid), to keep M0 tonic-native and testable. A Windows **named-pipe**
  transport is tracked as an M0 hardening task before M1.
```

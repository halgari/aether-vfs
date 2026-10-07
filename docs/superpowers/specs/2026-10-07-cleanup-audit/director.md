# aether-vfs cleanup audit: director side and transport

Repo: `/home/tbaldrid/oss/haskill/external/aether-vfs` at `fcdf874`. Paths are relative to `rust/crates/` unless they start with `rust/`, `docs/`, `bin/` or `.github/`.
Scope: vfs-director, vfs-directord, vfs-source, vfs-control, vfs-ipc, vfs-protocol, vfs-server, vfs-launch, vfs-unix, vfs-win, vfs-shared, vfs-ring-harness, xtask-descriptor.

`cargo check --all-targets` on Linux for these crates gives **zero warnings**, with one exception: **`vfs-launch` does not compile on Linux.** `vfs-launch/src/bin/vfs-game-probe.rs:67,78` uses `std::os::windows` without a cfg gate. As a result, `cargo check --workspace --all-targets` fails on Linux. CI never notices, because the Linux job uses a `-p` list that leaves vfs-launch out. There are no TODO, FIXME or commented-out code blocks in scope.

## What each questioned crate is for today

| crate | role today | last real change | consumers |
|---|---|---|---|
| vfs-directord | `vfs` CLI and gRPC daemon. It is a host over vfs-embed, the reference CLI, and the subject of the `proton-linux` CI job (`proton_cli`). It also carries `skyrim-live`, a Windows-only Skyrim harness that drives `tools/gamectl.ps1`. | 2026-10-02 | nothing in the workspace; not used by Haskill |
| vfs-server | Legacy `vfs-core`-tree ring server. Its own docs call it "**not the product path**" and say it exists only for `vfs-fuse-bench` (`vfs-server/src/lib.rs:4-12`). | Only mechanical edits since 2026-08-16 | vfs-launch (`vfs-fuse-bench`) |
| vfs-launch | Windows-only Skyrim launcher with `C:\tmp` defaults (`vfs-launch/src/main.rs:31`). Also holds two bins: `vfs-fuse-bench` and `vfs-game-probe`. | Only mechanical edits since 2026-08-17 | nothing |
| vfs-ring-harness | `ring-bench` is live: architecture §3.5 cites it, and it runs the real Director. The other four bins are spikes from the July JVM work and the September Wine-transport work. | 2026-10-02 (ring-bench only) | nothing; CI builds it (`ci.yml:80`) |
| vfs-control | gRPC `director.proto` plus the TOML `SessionConfig` | 2026-09-29 | vfs-directord, vfs-source |
| xtask-descriptor | Emits `resources/protocol-descriptor.edn` and the golden vectors "for the Clojure mirror" (`xtask-descriptor/src/lib.rs:1`). That mirror no longer exists anywhere: there are no `.clj` files in this repo, in `~/oss/aether-vfs` or in Haskill. | 2026-10-05 | CI "Fail on protocol drift" (`ci.yml:169-172`) |

`vfs-directord/src/registry.rs` is **not** the registry overlay. It is `SessionRegistry`, the daemon's table mapping a live session id to an `vfs_embed::Session` (`vfs-directord/src/registry.rs:1`, `:243`). That name now collides with three other things:
- `vfs-director/src/registry.rs` (`RegistryHost`, the Windows-registry overlay host);
- the `vfs-registry` crate;
- `struct Registry` in `vfs-ipc/src/readcache.rs:420`, the file table.

---

## High value

### D1 — Retire vfs-server and vfs-launch (legacy, parallel path)
- **Evidence**
  - `vfs-server/src/lib.rs:4-12` says "Retire it when the benchmark can express the same thing against the director". That condition is now met: `vfs-ring-harness/src/bin/ring-bench.rs:1-8` benchmarks the real `Director` behind `IpcServe::start_file_backed_with_workers`.
  - vfs-server is reached only through `vfs-launch/src/bin/vfs-fuse-bench.rs:18`.
  - `RingClient::submit_many` is used only by vfs-fuse-bench.
  - `vfs-launch/src/main.rs` duplicates what `skyrim-live`, `vfs exec` and Haskill already do.
  - vfs-launch has received only mechanical edits since 2026-08-17, and `vfs-game-probe` fails to compile on Linux (see above).
  - `vfs-server/src/handler.rs:27` is a third hand-written ring dispatcher. It drops the root (`:18-26`).
  - `vfs-server/src/server.rs:68` `handle_meta` is unreferenced.
- **Change**
  - Delete both crates. Drop `submit_many` from `vfs-ipc/src/endpoint.rs:269`.
  - Mark `docs/benchmarks/fuse-rpc-*.md` as historical, and move the 12 timestamped July reports into `docs/benchmarks/archive/`.
- **What breaks**
  - `rust/Cargo.toml` members.
  - The `-p vfs-server` in the Linux CI list (`ci.yml:143`). Its comment at `ci.yml:113-120` calls `fuse_e2e.rs` "load-bearing" for proving the ring works on Linux; that job is now done by `vfs-director/tests/serve_file_backed.rs` and `vfs-ipc/tests/*`, so fix the comment.
  - `rust/README.md:28-33` (quick start).
  - `rust/docs/overview.md:32,39,65`.
  - `rust/docs/benchmarks/README.md:7,25-26`.
- **Value** High · **Effort** S · **Risk** Low. Nothing depends on either crate, and the benchmark numbers are already historical.

### D2 — Move `skyrim-live` out of vfs-directord, and with it the Windows game bits in the kernel
- **Evidence**
  - `vfs-directord/src/bin/skyrim-live.rs` is 2,160 lines.
  - It is the **only** user of five of directord's dependencies: vfs-director, vfs-compose, vfs-zip, vfs-protocol and vfs-win. `src/*.rs` (the daemon itself) names only `vfs_control`, `vfs_embed`, `vfs_env` and `vfs_source`.
  - The dependency comment at `vfs-directord/Cargo.toml:28` ("the daemon still reaches for `stage` and `io_stats`") is false for the daemon.
  - The guard test says "skyrim-live is a scenario harness that **stage 5 removes from this crate**" (`vfs-directord/src/registry.rs:970,1015`). That move never happened.
  - It also pins two things in vfs-director:
    - `vfs-director/src/bench.rs`: Win32 window enumeration with `#![allow(unsafe_code)]` inside a crate declared `#![deny(unsafe_code)]`, used only by skyrim-live (`skyrim-live.rs:81,509-525`);
    - the `overlay_layer_dir` re-export, which exists "for skyrim-live" (`vfs-director/src/lib.rs:51-55`).
  - Duplicates inside the file: `SKYRIM_SE_APP_ID` (`:873`) and `SKYRIM_SE_APPID` (`:961`) are the same constant. Wipe helpers (`is_safe_to_wipe :1307`, `wipe_files :1488`) are repeated in `vfs-launch/src/main.rs:138`.
- **Change**
  - Create `vfs-skyrim-harness`, with `publish=false` and a Windows-only body, holding skyrim-live plus `bench.rs`. Alternatively fold it into vfs-ring-harness renamed `vfs-bench` (see D10).
  - Drop the five dependencies from directord, delete `vfs_director::bench`, and remove the re-export.
  - Remove the "stage 5" exemption from the guard test.
- **What breaks**
  - `tools/gamectl.ps1:194-196`: the exe path and build command.
  - `README.md:73`.
  - `rust/docs/bypass-baseline.md` (47 mentions) and `escape-matrix.md`: the build instructions.
  - Haskill's `.superpowers/launch-spike-host/src/main.rs:140` (comment only).
- **Value** High · **Effort** M · **Risk** Low. It is a pure move with a cfg-gated body. The `profile_seed_tests`, `steam_gate_tests` and `staging_layer_tests` modules move with it.

### D3 — The snapshot path is vestigial in the product (vfs-shared, and the seqlock)
- **Evidence**
  - Every product launch sends a **hard-coded empty tree**. `vfs-embed/src/session.rs:3053` (`empty_tree_snapshot`, from a hex constant) is used at `:1466`, `:1848` and `:2169`.
  - The comment there says it exists only because "`Engine::build` rejects zero-length snapshot bytes".
  - `vfs_shared::seqlock::{publish, read_stable}` (`vfs-shared/src/lib.rs:21`) has no caller outside vfs-shared's own tests.
  - `SnapshotBuilder` and `bridge::flatten` appear only in tests, xtask-descriptor, vfs-server and ring-harness.
  - vfs-redirect's `SnapResolution` branch (`vfs-redirect/src/lib.rs:452-473`) therefore always resolves against an empty tree in production.
- **Change**
  - Step 1: remove the snapshot from `vfs_protocol::shimcfg` and stop shipping the hex constant. (As done in stream I: once the shim's Engine was gone the snapshot was only decoded and ignored, so it was dropped from the config rather than made optional, and the config was given a version.)
  - Step 2: delete `seqlock.rs`.
  - Step 3, together with whoever audits the shim and redirect: decide whether vfs-shared's snapshot reader and vfs-core's tree survive at all.
- **What breaks**
  - `vfs-shim/src/engine.rs:9` and many vfs-shim/vfs-inject tests that build snapshots.
  - The `empty-tree-snapshot` and `shim-config-*` golden vectors (`xtask-descriptor/src/lib.rs:117,153-157`).
- **Value** High (it removes a whole parallel content model) · **Effort** L · **Risk** Med. It crosses the shim boundary, and an older shim must still accept the new config. Do the seqlock deletion first; it is S and has no risk.

### D4 — Rename `vfs-directord/src/registry.rs` to `sessions.rs`
- **Evidence**
  - `pub mod registry` and `pub use registry::SessionRegistry` at `vfs-directord/src/lib.rs:5,21`.
  - The names collide with `vfs_director::registry::RegistryHost`, the `vfs-registry` crate, and architecture §3.9 ("Registry overlay — `vfs-director::registry`").
  - Haskill's research doc already cites `vfs-directord/src/registry.rs:446-461` as if it were storage code.
- **Change**
  - `git mv` the file to `sessions.rs` and add `pub use sessions::SessionRegistry`.
  - Optionally keep `pub mod registry { pub use super::sessions::*; }` for one release.
  - Fix the guard test's `ends_with("registry.rs")` at `:1023`.
  - Separately, in vfs-ipc, rename the `readcache` `struct Registry` to `FileTable`.
- **Value** High (confusion cost) · **Effort** S · **Risk** Low.

### D5 — Stale architecture and README docs
- **`rust/docs/architecture.md`**
  - §3.3 (`:157-158`) says `Session` is in vfs-director; it moved to vfs-embed (`vfs-director/src/lib.rs:6-9`).
  - §3.4 (`:160-169`) mentions only vfs-win (no vfs-unix and no file-backed ring) and says "VERSION, now 3" at `:217`. The value is 4, as §3.9 `:353` itself says.
  - §3.8 (`:324-328`) lists vfs-launch as part of the control plane.
  - The §8 crate map (`:705-730`) is missing vfs-embed, vfs-proton, vfs-unix, vfs-pe, vfs-storage's block store and vfs-ring-harness's real role, and lists vfs-launch as "end-user launcher".
  - §9 (`:739-740`) says "**Windows x64 only**", but the Proton/Linux director runs in CI.
- **`rust/README.md`**
  - `:28-33`: vfs-launch quick start.
  - `:38-53`: host sketch uses `vfs_director::{LaunchOpts, Session}`, `set_overlay` and `mount_zip`, none of which exist there.
  - `:77`: "`vfs-director` | Kernel + Session + inject launch".
- **`rust/docs/overview.md`**: the whole document describes the hollow/C-ABI era (`:20-26`, `:57-70`).
- **`rust/docs/benchmarks/README.md`**: vfs-launch commands at `:25-26`; `vfs-cache` and Node binding rows (`:15-18`) for harnesses that are not in this repo.
- **Change**: update architecture §3.3, §3.4, §3.8, §8 and §9 and rewrite the README sketch against `vfs_embed::Session`. Either move `overview.md` and `vfs-summary.md` to `docs/history/` or delete them.
- **Value** High (onboarding) · **Effort** S–M · **Risk** Low.

### D6 — One codec idiom in vfs-protocol
- **Evidence**: three styles live in one file, `vfs-protocol/src/lib.rs`.
  - Fixed-offset slicing with `try_into` in `decode_path_req :116`, `decode_getattr_resp :134`, `decode_open_req :196`, `decode_open_resp :226`, `decode_close_req :483`, and others.
  - `take_u32/u64/u8` free functions with an `&mut usize` cursor (`:490-506`). Only `decode_readdir_resp :162` uses them.
  - The `Rd` cursor (`:524-570`), used by all `reg_*` codecs.
- **Behaviour differs between the styles**
  - File-op decoders ignore trailing bytes and read a bool as `!= 0`. Registry decoders reject trailing bytes (`done()`) and accept only 0 or 1.
  - File-op decoders allocate a `String`; registry decoders borrow `&str`.
- **Same pattern elsewhere**
  - `vfs-registry/src/format.rs:53-60,121-175` has its own `put_u32`, `put_str` and a `Reader` that returns a `Result`.
  - `encode_reg_key_reply` (`vfs-protocol/src/lib.rs:729`) and `format::encode_node` serialise the same `Node`.
- **Change**
  - Make `Rd` (plus a matching `Wr`/`put_*`) the only cursor, in a private `wire` module, and port every file-op decoder to it.
  - Delete `take_*`.
  - Decide deliberately whether file-op decoders become strict. Strict is safer, but check that the shim never sends padding; `decode_read_req` expects a trailing `pad:u32`.
  - Optionally export the cursor from vfs-registry, generic over the error type, so `format.rs` shares it.
- **Value** Med-High · **Effort** M · **Risk** Med. This is wire-adjacent: run the golden vectors and the shim's round-trip tests before changing strictness.

### D7 — Opcodes and statuses: one source and one re-export path
- **Opcode evidence**
  - The full catalog is defined twice: `vfs-ipc/src/layout.rs:51-73` ("reference values; the ring never interprets these") and `vfs-protocol/src/lib.rs:21-45`.
  - vfs-ipc already depends on vfs-protocol.
  - The test that claims to guard the duplication, `opcode_constants_match_ipc_catalog` (`vfs-protocol/src/lib.rs:950-957`), compares 6 literals and never touches `vfs_ipc::layout`. The registry opcodes are unguarded.
  - `OP_MATERIALIZE` (4) and `OP_REGISTER_PROCESS` (12) are never sent or dispatched: `dispatch_director` returns `ST_BAD_REQUEST` for them.
- **Status evidence**
  - Statuses live in `vfs-provider/src/status.rs`. Its header (`:3`) says "Values `0` through `-9` are fixed", but -10 and -11 exist.
  - The helper functions (`ok()`, `not_found()`, …) cover only some codes: there is no `no_space()` and no `reply_too_large()`.
- **Re-export chain**
  - vfs-provider → `vfs_protocol::ops` (`vfs-protocol/src/ops.rs`; no external user) → `vfs_protocol` root → `vfs_director::ops` (`vfs-director/src/ops.rs`).
  - The last hop is incomplete: it lacks `ST_NO_SPACE`, `ST_REPLY_TOO_LARGE` and `OPEN_APPEND`.
- **The descriptor**: `xtask-descriptor/src/lib.rs:36-60` hand-lists both tables a third time.
- **Change**
  - Make `vfs-protocol` the only opcode definition, exposed as `pub const OPCODES: &[(&str, u32)]` plus the consts.
  - Have `vfs_ipc::layout` re-export them, or simply delete them there (nothing uses `layout::OP_*` except 3 tests).
  - Add `STATUSES: &[(&str, i32)]` in vfs-provider. The descriptor and the tests iterate those tables.
  - Delete `vfs_protocol::ops` and `vfs_director::ops`, and import from `vfs_provider` / `vfs_protocol` directly.
  - Reserve 4 and 12 in a comment rather than exporting them.
  - Rename the ring slot states `ST_FREE`… to `SLOT_FREE`… so they stop sharing the `ST_` prefix with statuses.
- **Value** Med-High · **Effort** S–M · **Risk** Low (no numeric change; the descriptor's golden check proves it).

---

## Medium value

### D8 — Reduce xtask-descriptor to what still matters
- **Evidence**
  - The EDN descriptor exists for a Clojure mirror that is gone (`xtask-descriptor/src/lib.rs:1`).
  - `content_hash` is documented as "the handshake hash (M3)" (`:8`), but nothing outside the crate calls it.
  - The golden vectors are still a useful wire freeze. But they cover none of the registry codecs, `OP_STORED_NAMES` or `OPEN_RESP` flags beyond a default, and they duplicate the shim-config encoder inline (`:96-113`) only to avoid a vfs-shim dependency that `vfs_protocol::shimcfg` now removes.
- **Change**
  - Move the golden-vector test into `vfs-protocol/tests/golden.rs`, with the golden file inside the crate. Add registry and stored-names vectors, and call `shimcfg::encode_config_full` directly.
  - Delete the EDN descriptor, `content_hash`, `bin/regen-protocol`, `resources/`, the crate itself, and CI steps `ci.yml:169-172`.
- **Value** Med · **Effort** S · **Risk** Low. Check that no out-of-repo consumer reads `resources/*.edn`; I found none.

### D9 — Move `DiskProvider` and `MountGraph` out of the kernel crate
- **Evidence**
  - `vfs-source/src/lib.rs:47` depends on **all of vfs-director**, and with it vfs-unix, vfs-win and vfs-registry, just for `DiskProvider`.
  - `MountGraph` (`vfs-director/src/mount_graph.rs:1-22`) describes itself as an ordinary provider combinator "one layer below Director". vfs-embed builds it (`vfs-embed/src/session.rs:282`), and vfs-compose's docs already talk about it (`vfs-compose/src/lib.rs:33`).
  - Path normalisation is also duplicated: `vfs-director/src/path.rs:8` repeats `vfs-core/src/path.rs:18` minus the NT prefix stripping.
- **Change**
  - Move `DiskProvider` and `MountGraph` into vfs-compose, or into a small `vfs-disk` leaf crate. Keep `pub use` re-exports in vfs-director for one cycle.
  - vfs-source then drops its vfs-director dependency.
  - Replace `director::path::normalize` with `vfs_core::normalize_vpath`, after checking that the extra `\??\` stripping is harmless here.
- **Value** Med (dependency graph; the kernel becomes "Director + ring + stage + registry host") · **Effort** M · **Risk** Low–Med. `DiskProvider` has portability cfgs; run the Windows CI.

### D10 — Trim vfs-ring-harness to the live benchmark
- **Evidence**
  - `vfs-ring-harness/src/main.rs:1` is a "client for a **JVM-created** section, asserting the JVM server". That server no longer exists, and nothing runs the binary.
  - `ring-file-server` and `ring-file-client` (a hand-written dispatcher at `ring-file-server.rs:172-230`), `shim-ring-client`, and the bin `vfs-director/src/bin/vfs-serve-fb.rs` were the September Wine-transport proofs. The `proton_launch` CI test now covers that path with the real Session.
  - `ring-bench` is the only bin that docs cite (architecture §3.5).
- **Change**
  - Delete `main.rs` now.
  - Retire the ring-file pair, `shim-ring-client` and `vfs-serve-fb`, or move them under `examples/`.
  - Rename the crate to `vfs-bench`, which is also a home for skyrim-live's `bench.rs` (D2).
- **What breaks**: `ci.yml:80` (`-p vfs-ring-harness`), a comment at `vfs-proton/src/launch.rs:278`, and `vfs-director/tests/serve_file_backed.rs:24` (comment).
- **Value** Med · **Effort** S · **Risk** Low.

### D11 — Split `vfs-directord/src/lib.rs` (1,309 lines, 445 of them tests)
- **Evidence**: one file mixes five concerns.
  - Client connect and auto-spawn: `:28-266`.
  - Storage directory and opening: `:269-376`.
  - Serving the daemon: `:378-508`.
  - CLI flag parsing: `:509-640`.
  - Applying a config, and launch: `:655-863`.
- **Change**: split into `client.rs` (connect, spawn, wait), `storage.rs`, `server.rs`, `flags.rs` and `apply.rs`. Keep the `pub use` surface in `lib.rs` identical. Put the tests beside each module.
- **Value** Med · **Effort** S · **Risk** Low.

### D12 — Split `vfs-directord/tests/e2e.rs` (4,170 lines, 16 tests)
- **Evidence**
  - Artifact location and building: `:20-384`.
  - Scenario launches: `:455-1415`, `:4055`.
  - Escape matrix: `:1416-2110`, `:2843-3577`.
  - Enumeration: `:2110-2285`.
  - Profile API: `:2286-2842`.
  - Metadata seal: `:3578-3703`.
  - Five portable config/session tests: `:3704-4054`. These are not Windows-only, so they currently live behind a file-wide `#![cfg_attr(not(windows), allow(dead_code, unused_imports))]` (`:5`).
  - The header is still "M0 acceptance" (`:1`).
- **Change**
  - Move the artifact code to `tests/support/artifacts.rs`.
  - Split into `launch_scenarios.rs`, `escape_matrix.rs`, `profile_api.rs`, `enumeration.rs` and `session_config.rs`. The last has no cfg, so its tests stop hiding among Windows ones.
  - `LAUNCH_LOCK` stays per binary. Cargo runs test binaries one after another, so serialisation holds.
- **What breaks**: the CI comment at `ci.yml:64-66` names `ensure_inject_artifacts`'s `needed` array "in crates/vfs-directord/tests/e2e.rs".
- **Value** Med · **Effort** M · **Risk** Low.

### D13 — A shared dev-only test kit
- **`crc32` plus `write_stored_zip`**: 6 copies of the pair, plus 2 more `crc32` in vfs-zip unit tests.
  - `vfs-directord/tests/{support/mod.rs:484, composition.rs:20,67, copy_on_write_daemon.rs:582, proton_cli.rs:67}`
  - `vfs-embed/tests/copy_on_write_composition.rs:382`
  - `vfs-director/tests/unicode_case_fold_across_the_ring.rs:201`
- **`profile_dir` / `locate_artifact`**: 8 copies.
  - `vfs-directord/tests/e2e.rs:20,30`
  - `vfs-embed/tests/{proton_steam:45, launch_vfs_content:246, proton_skyrim:101, proton_launch:196, fuse_init_gate:26, proton_registry:58}`
  - `vfs-inject/tests/common/mod.rs:63,95`
- **In-process gRPC daemon setup**: written out separately in 5 directord test files (`TcpListener::bind` + `DirectorServer` occurs 24 times).
- **Change**: create a `vfs-testkit` crate (`publish=false`, dev-dependency only) with `StoredZip`, `artifacts::{profile_dir, locate, ensure_built}` and `daemon::in_process()`.
- **Value** Med · **Effort** M · **Risk** Low.

### D14 — Split `vfs-ipc/src/readcache.rs` (2,192 lines; about 950 are tests)
- **Evidence**
  - The code runs `:1-1235`, and the `impl ReadCache` block alone spans `:465-1214`.
  - Its types fall into clear groups: config/stats/diagnostics (`:118-240`), per-file state, slots and flights (`:266-440`), and the cache itself.
  - It touches no ring code. It is in vfs-ipc only so the shim and ring-bench can share it (`:12-13`).
- **Change**: turn it into a directory `readcache/{mod.rs, config.rs, stats.rs, file.rs (State/Slot/Flight), tests.rs}`. A separate `vfs-readcache` crate is possible later, but not required.
- **Value** Med · **Effort** S–M · **Risk** Low (pure move).

### D15 — Tidy `vfs-director/src/ring_dispatch.rs` (1,041 lines; 335 code)
- **Evidence**
  - In `OP_READ`, the inline-read branch appears twice, verbatim (`:164-178` and `:179-193`).
  - In `OP_READDIR`, two error arms do exactly what the catch-all does (`:100-111`).
  - File ops spell out `match … None => (ST_BAD_REQUEST, Vec::new())` 13 times, while `dispatch_registry` (`:283-288`) uses `reply` and `bad` closures.
  - The `OP_STORED_NAMES` reply is encoded inline (`names.join("/")`, `:248`) with no codec in vfs-protocol.
- **Change**
  - Restructure as `if want_bulk && let Some(arena) … else inline`.
  - Collapse the readdir arms.
  - Use one `reply` helper for both halves, and move `dispatch_registry` to `ring_dispatch/registry.rs`.
  - Add `encode_names_resp` / `decode_names_resp`.
  - Move the 700 lines of tests to `ring_dispatch/tests.rs`.
- **Value** Med · **Effort** S · **Risk** Low (well covered by tests).

---

## Low value / nits

### D16 — vfs-control, vfs-source and vfs-directord form a daemon-only cluster
- **Evidence**
  - `SourceSpec` is defined in vfs-control and re-exported by vfs-source (`vfs-source/src/lib.rs:15`), so the source crate depends on the control-plane crate.
  - `SourceSpec::Http` (`vfs-control/src/config.rs:71`) is a stub that always errors ("later milestone", `vfs-source/src/lib.rs:56`).
  - Haskill uses none of these three crates.
- **Change**
  - Move `SourceSpec` into vfs-source and have vfs-control depend on vfs-source.
  - Delete the `Http` variant until it is real.
  - Optionally fold vfs-control into vfs-directord as `directord::proto` and `directord::config`. vfs-source would then need only the `source.proto` half.
- **Value** Low–Med · **Effort** M · **Risk** Med: `SessionConfig` TOML and the proto wire format are user-facing.

### D17 — Stale comments
- `vfs-directord/Cargo.toml:28-29` (daemon "still reaches for stage and io_stats").
- The guard-test needle `vfs_cache::` names a deleted crate (`vfs-directord/src/registry.rs:1001`).
- `vfs-ring-harness/src/main.rs:1` (JVM).
- `xtask-descriptor/src/lib.rs:1,8` (Clojure, M3 handshake).
- `vfs-shared/src/lib.rs:23` ("pub use lines are added by later tasks").
- `vfs-server/src/lib.rs:14` ("re-exported here for older call sites").
- `vfs-provider/src/status.rs:3` (0..-9).
- `ci.yml:113-120` (vfs-server is the load-bearing one).
- "Userspace FUSE" naming in `vfs-protocol/src/lib.rs:1` and `vfs-director/src/ring_dispatch.rs:1`. Since the Linux port it misleads readers into thinking of `/dev/fuse`; `ci.yml:119-120` already has to explain this.
- Out of scope, noted only: vfs-compose's Clojure references (`layered.rs:1`, `router.rs:1`), and `vfs-shim/src/fuse_client.rs:670,699` ("JVM overlay").
- **Value** Low · **Effort** S · **Risk** Low.

### D18 — Dead or over-public items
- **Dead in production**
  - `DataArena::write_bank` (`vfs-ipc/src/arena.rs:56`; test-only).
  - `Server::handle_meta` (`vfs-server/src/server.rs:68`).
  - `#[allow(dead_code)] fn reset` (`vfs-win/src/event_notifier.rs:195`).
  - `IpcServe::apply_env` (`vfs-director/src/ipc.rs:388`; only `apply_env_roots` is called).
  - `bench::{deltas, find_pid, phases}` (`vfs-director/src/bench.rs`).
- **`pub` but used only inside their own file**
  - `Director::registry_changed` (`director.rs:123`) and `registry::reg_status` (`registry.rs:82`).
  - `ring::Geom::payload_off`, `DataArena::bank_index` / `bank_mapping_offset`, `ReadCache::cacheable`, `Permits::retire`.
  - directord's `daemon_log_path`, `parse_root_flag`, `build_provider_graph` (tests only), `layer_users` and `LiveSession::next_source_id`.
  - Narrow these to `pub(crate)`.
- **Value** Low · **Effort** S · **Risk** Low.

### D19 — Ring backing types follow an implicit contract
- **Evidence**: `vfs-director/src/ipc.rs:28-39` selects `RingMapping = SharedMapping | FileMapping` and relies on both "exposing `seg()`, `len()`, `as_mut_ptr()` with identical meaning" by convention.
- **Responsibility split across the platform crates**
  - vfs-win mixes the ring mapping and events with path canonicalisation (`volumes.rs`). Of `volumes.rs`'s callers, vfs-redirect is the main one (≈26 refs); vfs-shim and directord's skyrim-live use only `final_path_for_*`.
  - vfs-unix holds only the mapping.
  - vfs-shared is not OS code at all: it is the snapshot format (see D3).
- **Change**
  - Add a small `RingBacking` trait in vfs-ipc, implemented by both mapping types.
  - If D3 removes the snapshot, delete vfs-shared. Otherwise rename it `vfs-snapshot`, because "shared" reads as shared memory, which is vfs-win's and vfs-unix's job.
  - Consider renaming vfs-win and vfs-unix to `vfs-os-win` and `vfs-os-unix`.
- **Value** Low · **Effort** S (trait) or M (renames) · **Risk** Low.

### D20 — The RingClient submit API has grown five entry points
- **Evidence**: `submit`, `submit_reporting`, `submit_many`, `submit_many_held` and `submit_many_held_reporting` (`vfs-ipc/src/endpoint.rs:222-296`).
  - `submit_many` is used only by vfs-fuse-bench (D1).
  - The two `_reporting` variants are each called once, from `concurrent.rs`.
- **Change**: after D1, delete `submit_many`. Fold each `_reporting` variant into its base function behind an options struct.
- **Value** Low · **Effort** S · **Risk** Low.

---

## Suggested order of work
1. **D17 and D5**: comments and docs. Zero risk, and they make later reviews accurate.
2. **D1**: delete vfs-launch and vfs-server, then **D20**.
3. **D10**: delete the ring-harness JVM `main.rs` and the transport spikes; rename the crate to `vfs-bench`.
4. **D2**: move skyrim-live and `vfs_director::bench` out of directord and the kernel.
5. **D4**: rename to `sessions.rs`. **D11**: split directord's `lib.rs`.
6. **D7**, then **D6**, then **D8**: one opcode and status table, one codec cursor, golden vectors moved into vfs-protocol.
7. **D9**: move `DiskProvider` and `MountGraph` into vfs-compose.
8. **D13**, then **D12**: test kit first, then split e2e.rs on top of it.
9. **D14** and **D15**: mechanical splits.
10. **D3**: coordinate with the shim/redirect audit. Delete the seqlock early; decide the snapshot's fate later.
11. **D16**, **D18**, **D19** as time allows.

## Delete candidates

| crate | used by | safe to delete? | what must change first |
|---|---|---|---|
| vfs-server | vfs-launch (`vfs-fuse-bench` only) | **Yes**, together with vfs-launch | Remove `-p vfs-server` and fix the comment at `ci.yml:113-120,143`; mark `docs/benchmarks/fuse-rpc-*` historical |
| vfs-launch | nothing | **Yes** | `rust/README.md:28-33`, `rust/docs/overview.md`, `rust/docs/benchmarks/README.md:25-26`, architecture §3.8/§8; workspace member list |
| vfs-ring-harness | nothing (CI build `ci.yml:80`) | **Partly**: delete `main.rs` (JVM), ring-file-*, shim-ring-client; keep `ring-bench` | Architecture §3.5 cites ring-bench, so keep that bin, ideally in a renamed `vfs-bench`; `vfs-director/src/bin/vfs-serve-fb.rs` goes with the spikes |
| xtask-descriptor | CI drift check only | **Yes**, once the golden test moves | Move the golden vectors into `vfs-protocol/tests`; delete `bin/regen-protocol`, `resources/` and `ci.yml:169-172` |
| vfs-directord | nothing in the workspace; it is the `vfs` CLI | **No**: reference CLI, `proton_cli` CI job, README | Only extract skyrim-live (D2) |
| vfs-control | vfs-directord, vfs-source | **No** (could fold into directord, D16) | Move `SourceSpec` into vfs-source first |
| vfs-shared | vfs-shim, vfs-redirect, vfs-inject, vfs-server, xtask-descriptor, ring-harness | **Not yet**: the seqlock is deletable now | D3: remove the snapshot from shimcfg (done in stream I; the shim's `Engine` was already gone) |
| vfs-unix / vfs-win | vfs-director, vfs-shim, vfs-redirect, … | No | — |
| vfs-source | vfs-directord | No | D9 removes its vfs-director dependency |

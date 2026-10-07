# aether-vfs cleanup audit: host integration and repo-wide hygiene

Repo: `/home/tbaldrid/oss/haskill/external/aether-vfs` at master `fcdf874`.
Scope: vfs-embed, vfs-proton, vfs-env, the `vfs-fixture-*` crates, the vfs-embed Proton e2e tests, and repo-level files (bin/, CI, READMEs, docs, workspace manifest, resources/).
Paths below are relative to `rust/crates/` unless they start with `rust/`, `docs/`, `bin/` or `.github/`.

## Checks run

- `cargo clippy -p vfs-embed -p vfs-proton -p vfs-env --all-targets` on Linux: **clean, no warnings.**
- `#[allow(dead_code)]` in scope: only `vfs-proton/src/prefix.rs:338` (the `PrefixLock` field) and three `cfg_attr(..., allow(dead_code))` at `vfs-embed/src/session.rs:3019-3025`.
- TODO, FIXME, `todo!` and `unimplemented!`: **none** in scope.
- `cargo machete` and `cargo udeps` are not installed. A grep for unused dependencies in the in-scope crates found none.
- `proton_fake_runtime` was built (an incremental build of about 2 s) and run 11 times in parallel and once serially. **3 of the 11 parallel runs failed. The serial run passed.** See E4.

## Summary table

| id | category | value | effort | risk | Haskill? |
|---|---|---|---|---|---|
| E1 | session.rs module split | High | M | Low | no |
| E2 | Unix/Windows serve+launch duplication; `ProtonState` | High | M | Med | no |
| E3 | Env handshake defined once (vfs-env) | High | M | Med | no (indirectly: `;` rule) |
| E4 | `proton_fake_runtime` parallel flake: root cause | High | S | Low | helps |
| E5 | One Proton test-support crate | High | M | Low | no |
| E6 | One prerequisite policy for e2e tests (skip vs fail, VFS_HOME, /tmp) | High | S | Low | no |
| E7 | Artefact-location rule; `--release` mismatch bug | Med | S | Low | no |
| E8 | CI coverage gaps | High | S–M | Low | no |
| E9 | `check_geometry` ring-length check never fires in production | Med | S | Low | no |
| E10 | Haskill re-implements Session's prefix bring-up | Med | S | Low | **yes** |
| E11 | Stale architecture doc and READMEs | High | M | Low | no |
| E12 | docs/superpowers sprawl: index and archive | Med | M | Low | 1 doc link |
| E13 | Workspace manifest: no `[workspace.*]`, no lints table, no rust-version | Med | M | Low | no |
| E14 | Profiles | Low–Med | S | Low | build time |
| E15 | vfs-proton oversized files (prefix/launch/nvapi) | Med | M | Low | no |
| E16 | vfs-proton public surface and dead items; `WineLaunch` construction | Low–Med | S | Low | check |
| E17 | Stale comments in vfs-embed: removed Node binding, "no non-Windows body yet" | Med | S | Low | no |
| E18 | Small duplications in vfs-embed | Low | S | Low | no |
| E19 | Error strings with embedded runs of spaces (launch.rs) | Low | S | Low | no |
| E20 | Fixture consistency, merges, escape split | Low–Med | M | Low | no |
| E21 | bin/build-windows robustness and its artefact list | Low–Med | S | Low | artefact list |
| E22 | Committed scratch, raw benchmark dumps, gitignore oddities | Low | S | Low | no |
| E23 | vfs-env classification and drift test | Low–Med | S | Low | no |
| E24 | `LaunchOpts` shape (String paths, no injector field, verbose history) | Low–Med | S | Med | **yes** |
| E25 | Haskill reaches past vfs-embed into vfs-proton | Med | S | Low | **yes** |

---

## High value

### E1. Split `vfs-embed/src/session.rs` (4,023 lines, 112 `cfg` attributes)

**Category:** structure / oversized file.

**Evidence.** One file holds all of the following:
- launch options (`:63-201`);
- staging (`:203-252`, `:1151-1232`, `:1650-1760`);
- composition (`:254-356`, `:892-1290`);
- the `Session` struct with 13 `#[cfg(unix)]` fields (`:365-475`), and the matching 13 cfg'd initialisers in `new()` (`:493-538`);
- Proton configuration (`:561-670`, `:845-859`);
- the registry layer (`:1042-1122`, `:2621-2639`);
- host-side reads (`:1337-1410`);
- the Windows serve and launch (`:1421-1473`, `:1821-1933`, `:3080-3102`);
- the unix serve and launch (`:1505-1648`, `:1991-2351`);
- `Drop` (`:2359-2411`);
- the Proton launch-handle state machine (`:2641-2986`);
- path helpers (`:2988-3157`);
- the tmpfs ring (`:3159-3305`);
- artefact lookup (`:3307-3365`);
- about 830 lines of tests in five modules (`:2413-2619`, `:3376-4023`).

`launch_detached` alone is 245 lines (`:2030-2274`).

**Proposed change.** Make `session/` a directory. The public paths stay the same because `lib.rs` re-exports them.

| new file | moves there | ~lines |
|---|---|---|
| `session/mod.rs` | `Session` struct (unix fields collapsed into one `#[cfg(unix)] proton: proton::ProtonState`); `new`, `Default`, `Drop` (calling `proton::drop_session`); `set_root`, `set_overlay`, `set_state_dir`, `io_workers`; `declare_root`, `declared_roots`, `root_locations`, `root_backing_dir`, `state_dir`, `kernel`, `virtual_root`, `overlay_layer_dir`, `is_serving`, `ipc`, `stop_serve` | 500 |
| `session/opts.rs` | `LaunchOpts`, `StageOpts` | 220 |
| `session/compose.rs` | `compose_root`, `reject_sequential`, `RootComposition`, `mount`, `mount_at`, `claim`, `set_root_mounts`, `set_write_layer[_at]`, `recompose`, `clear_mounts`, `clear_root`, `composed_roots`, `has_write_layer`, `mount_zip`, plus `root_ownership_tests` | 550 |
| `session/read.rs` | a shared `read_whole(&Director, RootId, &str)` used by both `KernelSource` and `read_file_at` (see E18); `readdir`, `getattr`, `rejected_writes` | 120 |
| `session/registry.rs` | `set_registry_layer`, `registry_attached`, `flush_registry`, `RegistrySync`, `registry_sync_for`, `registry_layer_tests` | 330 |
| `session/stage.rs` | `KernelSource`, `stage_launch`, `staged_proxies`, `resolve_launch_image`, `ResolvedImage`, `no_drive_names`, the shim-config writer and `empty_tree_snapshot`, `launch_image_tests`, `snapshot_tests` | 520 |
| `session/windows.rs` (`#[cfg(windows)] mod`) | `LAUNCH_ENV_LOCK`, `serve`, `launch`, `extra_roots_env`, `locate_shim_payload` | 260 |
| `session/proton/mod.rs` (`#[cfg(unix)] mod`) | `ProtonState`; `set_home`, `set_prefix_init`, `set_steam_helper`, `set_steam_state_dir`, `set_prefix_name`, `check_root_location`; `proton_home`, `steam_launch`, `serve`, `wine_session_id`, `link_into_prefix`, `link_roots`, `launch`, `launch_detached`, `stop_launch`, `latest_running`, `reap_detached`; `locate_wine_artifacts`, `wine_cwd`, `join_wine`; consts `RING_FILE`, `WINE_LINK_DIR`, `DEFAULT_ROOT0_LOCATION`, `PROTON_PAYLOAD_CAP`; the unix half of `Drop` | 600 |
| `session/proton/handle.rs` | `AnonPrefix`, `LaunchHandle`, `LaunchStopper`, `StopInner`, `StartingGuard`, `STOP_WAIT`. `LaunchExit` and `STOPPED_EXIT_CODE` stay portable in `mod.rs`, because they are exported on every platform. | 350 |
| `session/proton/ring.rs` | `ring_in_memory`, `remove_memory_ring`, `is_memory_fs`, `memory_fs_in`, `is_private_dir`, `ring_location_tests` | 300 |

Then break `launch_detached` into named steps:
- `ensure_prefix(&self) -> (ProtonRoot, runtime, Prefix, PrefixLock)` (this is the piece E10 exposes);
- `write_shim_config`;
- `nvapi_for_launch(&opts, &runtime, &prefix, &mut notes)`;
- `wine_launch(...) -> WineLaunch`.

Also: the doc comment at `:3367-3375` is attached to the `#[cfg(test)] mod root_ownership_tests`. It should move with the code it describes.

- **Value:** High. This is the file Haskill's integration lives against, and every change to it today touches an unrelated cfg block.
- **Effort:** M. A mechanical move, then the `ProtonState` collapse.
- **Risk:** Low. Pure moves are checked by the compiler. Windows-only bodies are only compiled by the Windows CI job, so let CI run before merging.
- **Haskill:** no (public paths unchanged).

### E2. Shared steps of the Unix and Windows serve and launch

**Category:** duplication.

**Evidence** (session.rs). The two platforms repeat the same steps:

| step | Windows | unix | notes |
|---|---|---|---|
| idempotent "already serving" check and creating root, overlay and state directories | `:1423-1429` | `:1507-1513` | |
| "serve() before launch()" | `:1823-1826` | `:2031-2034` | |
| empty-image refusal | `:1828-1830` | `:2047-2049` | same message |
| writing `shim.cfg` with `empty_tree_snapshot()` | `:1840-1850` (and again in `serve` `:1465-1469`, where a failed write is silently ignored with `let _ =`) | `:2167-2173` | Windows calls `vfs_shim::encode_config_with_overlay`, unix calls `vfs_protocol::shimcfg::encode_config_with_overlay`, which is the same function re-exported |
| clearing `ready.flag` | `:1853-1854` | `:2175-2176` | |

Two more places pick behaviour by platform:
- The ready timeout's 180 s default is applied in the session on Windows (`:1905-1910`) but left to the injector on unix.
- `set_registry_layer` writes `VFS_REGISTRY` into the process env itself on Windows (`:1089-1101`), and so does `apply_env_roots` (see E3).

**Proposed change.**
- Add private helpers `prepare_dirs()`, `require_serving()`, `check_image(&opts)`, `write_shim_config(root, overlay) -> PathBuf` and `fresh_ready_flag() -> PathBuf`.
- Make `vfs-protocol` an unconditional dependency (it is portable and `forbid(unsafe_code)`), and call `vfs_protocol::shimcfg` on both platforms.
- Make the Windows `serve` stop writing `shim.cfg` at all, because `launch` rewrites it anyway (`:1841-1843` says so).
- Collapse the 13 unix-only fields into `ProtonState` (see E1). That removes about 26 cfg lines from the struct and `new()`.

- **Value:** High. Removes most of the per-platform drift surface.
- **Effort:** M.
- **Risk:** Med. The Windows body cannot be exercised on Linux, so it relies on Windows CI.
- **Haskill:** no.

### E3. Define the env handshake once, in vfs-env

**Category:** duplication / correctness.

**Evidence.** The set of `VFS_*` handshake names is hand-maintained in five places, and they already disagree:

1. `vfs-director/src/ipc.rs:400-447` (`IpcServe::apply_env_roots`, Windows) sets and removes:
   - `RING_SECTION`, `RING_BYTES`, `RING_PAYLOAD_CAP`, `ARENA_OFFSET`, `ARENA_LEN`, `SERVER_EV`, `CLIENT_EV`, `FUSE_CFG`, `VIRTUAL_DIR`;
   - `REGISTRY` (set or removed);
   - `VIRTUAL_ROOTS` (set or removed);
   - it removes `RING_PATH`.

   It does **not** clear `INJECT_CWD` or `INJECT_STEAM_HELPER`.
2. `vfs-proton/src/launch.rs:282-393` (`launch_env`) sets the file-backed set plus `INJECT_*`. It never sets `FUSE_CFG`.
3. `vfs-proton/src/launch.rs:585-598` (`stale_env`) is the list of names to remove. It uses **string literals** (`"VFS_RING_SECTION"` and others) rather than `vfs_env` constants, and it omits `VFS_FUSE_CFG`.
4. `vfs-proton/src/launch.rs:416-435` (`is_reserved_env`) omits `VFS_REGISTRY` and `VFS_FUSE_CFG`. So `LaunchOpts::env` can set `VFS_REGISTRY=1` with no registry layer attached:
   - `stale_env` then skips the removal, because the name is in the env;
   - the shim installs the registry hooks;
   - every registry write fails closed.
5. `vfs-embed/src/session.rs:1089-1101` writes `VFS_REGISTRY` itself on Windows, in addition to `apply_env_roots`.

The `id=loc;id=loc` roots encoding is also built twice (`ipc.rs:440-444` and `launch.rs:331-337`). Haskill hard-codes the `;` restriction that this format imposes (`haskill-vfs/src/session.rs:46-53`, `:65`).

`vfs-env/src/lib.rs:487` classifies `REGISTRY` as `Behaviour`, although it is written by the host and read by the child, which makes it a handshake name.

**Proposed change.** In the dependency-free `vfs-env`, add `pub mod handshake`:
- `pub enum Transport { Section { name, server_ev, client_ev, fuse_cfg }, File { path } }`.
- `pub struct Handshake { transport, ring_bytes, payload_cap, arena_offset, arena_len, virtual_dir, virtual_roots: Vec<(u32, String)>, registry: bool, inject_cwd: Option<String>, steam_helper: Option<&'static str> }`.
- `fn entries(&self) -> Vec<(&'static str, Option<String>)>`, covering **every** `Kind::Handshake` name, where `None` means "must be unset".
- `encode_roots` and `parse_roots`.

Then route the existing code through it:
- `apply_env_roots` becomes a loop of `set_var`/`remove_var` over `entries()`.
- `launch_env` inserts the `Some` entries.
- `spawn` calls `env_remove` for the `None` entries. That replaces `stale_env`.
- `is_reserved_env` becomes "any `Kind::Handshake` name, plus `WINEPREFIX` and `PROTONPATH`".
- `set_registry_layer` stops writing env.

Add a vfs-env test that the names in `entries()` equal `ALL.filter(Handshake)`; that makes the hand-sync mechanical. Reclassify `REGISTRY` as `Handshake`. The shim's roots parser should call `parse_roots`.

- **Value:** High. This is the known "kept in sync by hand" issue, plus a real `VFS_REGISTRY` hole.
- **Effort:** M.
- **Risk:** Med. It touches `vfs-director` (Windows path) and the shim parser. The Windows CI job and `proton_fake_runtime` cover both ends.
- **Haskill:** no API change. Haskill's `;` check could later call `vfs_env::handshake::encode_roots` validation.

### E4. `proton_fake_runtime` fails in parallel: root cause

**Category:** test harness / correctness.

**Evidence.** I reproduced the failure: 3 of 11 default-parallel runs failed and the serial run passed. Every failure is a prefix-lock refusal right after the previous holder released it:

```
a_log_file_receives_wines_stdout_and_stderr (proton_fake_runtime.rs:238): "launch: prefix …/vfs-fake-rt-…-log/sessions/fake/compat/pfx is in use by another live launch"
a_prefix_in_use_elsewhere_is_not_set_up_under_it (:866): same, right after `drop(held)`
a_launch_handle_reports_its_end_and_stops_when_dropped (:646): same, right after `h.wait()`
```

The mechanism:
- `Prefix::lock` (`vfs-proton/src/prefix.rs:594-608`) is `File::try_lock`, which is `flock`.
- An `flock` lock belongs to the open file description, and every concurrent `fork()` in another test thread duplicates the descriptor until the child `exec`s (the fd is CLOEXEC).
- That is the same fork-to-exec window `spawn_retrying_busy` already works around for `ETXTBSY` (`prefix.rs:776-797`).
- With 25 tests forking shell scripts at once, a lock dropped in one thread is still held by a sibling's half-forked child for microseconds to milliseconds, and the next `try_lock` sees `WouldBlock`.

Each test uses its own prefix, so the tests do not actually conflict; the error message is a false positive. The same race is reachable in production: Haskill's `prepare_prefix` drops its lock and the launch takes it again straight away, while the director forks Wine.

**Proposed change.** In `Prefix::lock`, retry `WouldBlock` with a short bounded backoff (for example up to 500 ms) before returning `PrefixError::Busy`, as `spawn_retrying_busy` does. A live holder still fails, just 0.5 s later. Add a regression test that holds the lock while another thread forks in a loop.

- **Value:** High. It makes `cargo test -p vfs-embed` reliable, including in the Linux CI job, which runs this file in parallel (see E8).
- **Effort:** S.
- **Risk:** Low.
- **Haskill:** fixes a latent "in use" error after `prepare_prefix`.

### E5. One Proton test-support crate

**Category:** duplication / harness.

**Evidence.** Across the workspace there are 26 copies of `profile_dir`/`tmp`/`scratch`-style helpers. In the vfs-embed Proton tests alone:

- `profile_dir()`: `proton_launch.rs:196`, `proton_registry.rs:58`, `proton_steam.rs:45`, `proton_skyrim.rs:101`, `launch_vfs_content.rs:246`, `fuse_init_gate.rs:26`, plus an inline copy in `proton_nvapi.rs:34`.
- Windows-artefact lookup, in five variants:
  - `proton_launch.rs:210` panics and checks `deps/`;
  - `proton_registry.rs:69` returns `Err` and checks `deps/`;
  - `proton_steam.rs:55` and `proton_nvapi.rs:34` only check the profile directory;
  - `proton_skyrim.rs:113`.
- `steam_client()`: `proton_steam.rs:67` and `proton_nvapi.rs:49`.
- `probe(log)` line parsers: `proton_steam.rs:99` and `proton_nvapi.rs:58`.
- Recording providers: `Loud` (`proton_launch.rs:75`) and the `opened` wrapper (`proton_skyrim.rs:67`).
- The aether-home and runtime lookup, three ways:
  - `proton_registry.rs:98` uses `vfs_proton::Root::from_env` and `installed_dirs`, and makes a throwaway home;
  - `proton_skyrim.rs:160-200` **re-implements** both `Root::from_env` and GE tag ordering (`newest_runtime`, with its own `(N, M)` key);
  - the others assert `VFS_HOME`.
- Scratch directories, four rules:
  - `std::env::temp_dir()`, i.e. `/tmp`: `proton_launch.rs:250`;
  - `CARGO_TARGET_TMPDIR`: `proton_steam.rs:37`, `proton_nvapi.rs:26`, `proton_fake_runtime.rs:43`;
  - `target/tmp` derived from `current_exe`: `session.rs:2470`;
  - `scratch_root()`: `proton_skyrim.rs:123`.
- The two Windows-only `ensure_fixtures()` in `launch_vfs_content.rs:276` and `fuse_init_gate.rs:52` are near-identical, and a comment there explicitly declines to share them (`launch_vfs_content.rs:271-274`).
- `vfs-directord/tests/proton_cli.rs:67,80` duplicates `crc32` and `write_stored_zip` from its own `tests/support/mod.rs:484,496`.

**Proposed change.** Add a `vfs-test-support` crate (`publish = false`, dev-dependency only) that depends on `vfs-proton` (`default-features = false`) and `vfs-provider`. It would provide:
- `scratch!(tag)`: a macro, so that it can capture `env!("CARGO_TARGET_TMPDIR")`. Never `/tmp`.
- `profile_dir()`.
- `windows_artifacts(&[..]) -> Result<Artifacts, Skip>`, which applies the single E7 rule.
- `aether_home() -> Result<Root, Skip>`, via `Root::from_env`.
- `newest_runtime(&Root)`, via `installed_dirs`.
- `throwaway_home()`, taken from `proton_registry`: a home under target whose `runtimes` links to the real one, so prefixes never land in the user's real home.
- `steam_client()`, `probe_lines(log, prefix)` and `RecordingProvider`.
- `ensure_windows_fixtures(&[pkgs])` for the native-Windows tests.
- A `require!(expr, "why")` macro that implements E6.

Use the crate from the vfs-embed, vfs-directord and vfs-proton tests.

- **Value:** High. It is the precondition for a consistent harness, and it removes the hand-rolled runtime ordering in `proton_skyrim`.
- **Effort:** M.
- **Risk:** Low (test-only).
- **Haskill:** no. Haskill's own tests could use it later.

### E6. One prerequisite policy for the e2e tests

**Category:** harness consistency.

**Evidence.** The tests disagree about what happens when a prerequisite is missing:
- `proton_launch.rs:283-287,529,671` **assert** `VFS_HOME`, and `windows_artifacts()` panics (`:233-242`).
- `proton_registry.rs:220-237` skips and passes, and resolves the home with `Root::from_env` (VFS_HOME, else XDG), so it does not need `VFS_HOME`.
- `proton_steam.rs:118` asserts `VFS_HOME`, while `proton_nvapi.rs:70` uses `expect`. Both skip on a missing Steam client.
- `proton_skyrim.rs:229` skips unless `VFS_TEST_SKYRIM_DIR` is set.

`README.md:60-62` says "The Proton tests skip silently (they print why on stderr and pass)", which is false for `proton_launch` and partly false for `proton_steam` and `proton_nvapi`.

`proton_launch` also:
- uses `/tmp` (`:250`);
- boots an **anonymous prefix in the user's real `$VFS_HOME/sessions`** (it calls `Session::new()` with no `set_home`), unlike `proton_registry`'s throwaway home.

**Proposed change.** One policy, implemented in E5:
- Every Proton test stays `#[ignore]`.
- Under `--ignored`, **software** prerequisites must be present, and a missing one fails with an actionable message. Software means the runtime (via `Root::from_env`, never asserting `VFS_HOME`) and the Windows artefacts in this profile.
- **Hardware or account** prerequisites print `SKIP <test>: <reason>` and pass. Those are an NVIDIA GPU, a running Steam client, a local Skyrim install, and `steam_api64.dll`.
- `VFS_TEST_REQUIRE_ALL=1` turns skips into failures, for a fully provisioned machine.
- Every Proton e2e test uses `throwaway_home()`.
- Fix the README paragraph to match.

- **Value:** High. Right now a developer cannot tell a skipped test from a passed one.
- **Effort:** S once E5 exists.
- **Risk:** Low.
- **Haskill:** no.

### E8. CI coverage gaps

**Category:** CI.

**Evidence** (`.github/workflows/ci.yml`):
- **No Linux clippy except for block-store** (`:152`). The workspace clippy with `-D warnings` runs on Windows only (`:59`), so every `#[cfg(unix)]` body is never linted by CI. That covers the whole Proton half of session.rs, vfs-unix, the unix side of vfs-director, and the `proton_*` tests. (It is clean today; nothing holds it there.)
- The `proton-linux` job runs only `proton_launch` and `proton_cli` (`:231`, `:239`). `proton_registry` needs nothing more than that job already provisions (a GE runtime and the artefacts), yet it is not run. It needs the artefacts in its own profile, so either run it in debug or fix E7.
- `vfs-embed` runs in `rust-linux-portable` (`:142`) with the default parallel harness, so the E4 flake lands in CI.
- There is no `cargo fmt --check` and no `cargo doc` with `-D rustdoc::broken_intra_doc_links`. session.rs and lib.rs docs use many intra-doc links, for example `[serve]` and `[launch]` at `session.rs:1304,1309,1765,1937`. Those are likely broken, because they are not `Session::serve` paths.
- There is no cargo caching (for example `Swatinem/rust-cache`), and all three jobs build from scratch.
- Stale comments:
  - `:140-141` says "its one Wine test is #[ignore]d", but there are now seven ignored Wine tests across five files;
  - `:98-104` is a plan-era "DEVIATION from the task 4 brief" note.
- `vfs-redirect` (pure decision logic) is not in the Linux list. It depends on `vfs-win`, so it may not be portable as claimed in architecture.md §3.5; worth checking.

**Proposed change.**
- Add `cargo clippy --all-targets -- -D warnings` on Linux, using the same `-p` list as the test step.
- Add `proton_registry` to `proton-linux`.
- Add `fmt --check` and a docs step.
- Add a rust-cache.
- Trim the narrative comments to one line plus a link to a doc.

- **Value:** High.
- **Effort:** S–M.
- **Risk:** Low. Adding gates might surface latent warnings or broken links, which is the point.
- **Haskill:** no.

### E11. Stale architecture doc and READMEs

**Category:** docs hygiene.

**Evidence.**

`rust/docs/architecture.md`:
- §3.3 (`:157-158`) still places `Session` in vfs-director.
- The crate map (`:705-730`) omits vfs-embed, vfs-proton, vfs-unix, vfs-pe, vfs-ipc's Unix side, vfs-server and vfs-block-store's role.
- §9 says "**Windows x64 only.** … the implementation is not [portable]" (`:739-740`).
- There is no section on the Proton path at all: the file-backed ring is mentioned only at `:221-224`, and the prefix, root locations and launch handle are not mentioned.

`rust/README.md` describes a pre-vfs-embed world:
- `use vfs_director::{LaunchOpts, Session}` (`:36`);
- "`vfs-director` | Kernel + Session + inject launch";
- a `C:\GameLayers` quick start;
- docs links into the older `rust/docs/superpowers`.

`rust/docs/overview.md` is self-flagged "Partly out of date": it covers PE hollowing, the C ABI (`:60`, `:77`) and `vfs-launch`.

`README.md:1-6` frames the project as "Windows game modding" with a PowerShell quick start, although Linux/Proton is the primary consumer (Haskill). Its Proton test paragraph is wrong (see E6).

**Proposed change.**
- Update architecture.md: add §3.x "Proton host path" covering the file-backed ring, prefix, root locations, `LaunchHandle` and the tmpfs ring, and fix §3.3, the crate map and §9.
- Replace `rust/README.md` with a short pointer to the root README and architecture.md, or delete it.
- Move `overview.md` and `vfs-summary.md` under `docs/archive/`.
- Rework the README quick start so it covers both platforms.

- **Value:** High (onboarding, and Haskill readers).
- **Effort:** M.
- **Risk:** Low.
- **Haskill:** no.

---

## Medium value

### E7. One artefact-location rule, and a `--release` bug

**Category:** harness.

**Evidence.**
- `bin/build-windows` copies to `target/<profile>`. Each test looks in its own profile (see E5). `proton_registry` prints a profile hint (`:224-231`); the others do not.
- Bug: the Windows `ensure_fixtures` (`launch_vfs_content.rs:276-343`, `fuse_init_gate.rs:52-97`) always runs a **debug** `cargo build` (no `--release`), but `profile_dir()` is the running test's profile. Under `cargo test --release` it builds into `target/debug` and then fails to find the artefacts in `target/release`.

**Proposed change.** The rule: "artefacts live in `target/<profile>`, and a test looks only in its own profile. If they are missing there but present in the other profile, the message says so and gives the exact command."
- `ensure_fixtures` passes `--release` when `!cfg!(debug_assertions)`.
- Allow an override, `VFS_WINDOWS_ARTIFACTS=<dir>`, used by both the tests and `locate_wine_artifacts`.
- Never silently mix profiles: a shim from a different build can carry another ring `VERSION` and refuse to attach.

- **Value:** Med.
- **Effort:** S.
- **Risk:** Low.
- **Haskill:** no (Haskill passes explicit paths).

### E9. `check_geometry`'s ring-file length check never fires in production

**Category:** correctness / dead check.

**Evidence.**
- `vfs-proton/src/launch.rs:651-662` calls `std::fs::metadata(&l.ring_path)`.
- But `WineLaunch::ring_path` is documented as the path **as Wine sees it** (`:72`).
- Session passes `PathBuf::from(wine_ring)`, i.e. `C:\vfs-session\state\ring.bin` (`session.rs:2145,2216`). On Linux `metadata` fails, so the `_ => Ok(())` arm always wins.
- The unit test (`launch.rs:713-728`) only passes because it puts a **host** path in `ring_path`.

**Proposed change.** Pick one:
- add `ring_host_path: Option<PathBuf>` to `WineLaunch` (Session knows `ring`, `session.rs:2038-2045`) and check that; or
- do the length check in Session before converting the path.

- **Value:** Med. It is a claimed safety net that does not exist.
- **Effort:** S.
- **Risk:** Low.
- **Haskill:** no.

### E10. Haskill re-implements Session's prefix bring-up

**Category:** duplication across repos.

**Evidence.** `haskill/crates/haskill-vfs/src/session.rs:158-181` (`prepare_prefix`) repeats `vfs-embed/src/session.rs:2086-2134` step for step:
1. `installed_dirs(&home)…next()` gives the runtime;
2. `prefix_dir(&home, &name, &init)`;
3. `Prefix{dir}.lock()`;
4. `ensure_with(&home, &runtime, &name, &init)`.

`nvapi_status` (`:259-279`) repeats the "newest runtime" lookup a third time.

**Proposed change.**
- Add `vfs_proton::runtime::newest_installed(&Root) -> io::Result<Option<PathBuf>>`, and use it in Session, Haskill and the tests (it also replaces `proton_skyrim`'s copy).
- Add `Session::prepare_prefix(&self) -> Result<PathBuf, String>` on unix. It is the `ensure_prefix` step from E1's breakdown of `launch_detached`, using the session's own home, name and init.
- Haskill's `prepare_prefix` then becomes: build the `Session` (or a light config), call `prepare_prefix`.

- **Value:** Med. One code path decides "which runtime, which prefix".
- **Effort:** S.
- **Risk:** Low.
- **Haskill:** **yes**. Change `prepare_prefix` and `nvapi_status` in the same change.

### E12. docs/superpowers sprawl

**Category:** repo hygiene.

**Evidence.** There are two trees:
- `rust/docs/superpowers/{specs,plans}`: 43 files, about 18k lines, dated 2026-07-13 to 07-15. This is the pre-extraction spike era: C ABI, hollowing, `spike-b`.
- `docs/superpowers/{specs,plans,reviews}`: 50 files, about 27k lines, dated 07-25 to 10-05.

Many are clearly historical:
- the M1–M5 JVM/Clojure milestones (`2026-07-27-m2-jvm-ffm-ring-server*`), although the Clojure layer was removed;
- the Node/TypeScript/ESM plans (`2026-08-16-stage4-embed-and-node.md`, `2026-08-17-node-typescript-migration.md`, `2026-08-19-esm-migration*`), although `vfs-node` was removed in `1f40e17`;
- all executed plans.

`**Status:**` headers are stale: `proposed, 2026-08-31` and `Approved (design); plan pending` on implemented work.

69 references to these paths exist in code and docs (9 `.rs`/`.toml` files). Haskill's `docs/research/2026-09-30-aether-vfs-integration.md:11` points at `external/aether-vfs/docs/superpowers/specs/`.

**Proposed change.**
- Create `docs/design/INDEX.md`, a table of date, title, status (current / implemented / superseded by X / abandoned) and implementing crate.
- Keep the specs that are still the reference in place: pluggable providers, case-fold contract, Linux portability, wine-hosted shim, block store, vfs-storage, linux-cli, registry overlay, no-bypass/real-roots, daemon rework.
- `git mv` `rust/docs/superpowers`, the JVM/Node specs, **all executed plans** and `reviews/` into `docs/archive/`, each with a one-line header saying "historical".
- Update the 69 references. The code comments in `Cargo.toml`, `session.rs` and `lib.rs` should point at current specs only.
- Fix the status lines as part of building the index.

- **Value:** Med.
- **Effort:** M (mostly link fixing).
- **Risk:** Low.
- **Haskill:** one research-doc link.

### E13. Workspace manifest

**Category:** workspace config.

**Evidence** (`rust/Cargo.toml` is 77 lines: members plus two profiles):
- No `[workspace.package]`. Edition, license and version are repeated in 37 manifests; 37 are `edition = "2021"` and `vfs-block-store` is `2024`.
- `publish = false` is set on only 14 of 38 manifests. The fixtures `escape`, `prefs`, `read`, `staticimp`, `vproxy` and `writepath` lack both it and a `description`.
- **No `[workspace.lints]` and no per-crate `[lints]`.** The unsafe policy is crate-attribute-only and inconsistent:
  - `forbid` in core, zip, protocol, provider, server and redirect;
  - `deny` in director, embed, inject, ipc, shared, shim and unix;
  - nothing in vfs-proton, vfs-env, vfs-storage, vfs-pe, vfs-compose or the fixtures. vfs-proton and vfs-env contain no `unsafe` and could `forbid`.
- No `[workspace.dependencies]`. Path dependencies and `windows-sys = "0.61"` (with features) are repeated, and version specs drift: `tempfile` `"3"` vs `"3.27"`, `blake3` `"1"` vs `"1.8"`, `proptest` `"1"` vs `"1.11"`.
- **No `rust-version`.** Code needs at least 1.89: `File::try_lock` (`prefix.rs:600`), and `Option::is_none_or` (`session.rs:3197`) needs 1.82. Only block-store declares 1.89. CI uses floating `stable`.
- A stale comment at `rust/Cargo.toml:21-23` says block-store is "MIT OR Apache-2.0, as it was upstream", but `vfs-block-store/Cargo.toml:7` is `GPL-3.0-only`.

**Proposed change.**
- Add `[workspace.package]` with edition 2021, license, `rust-version = "1.89"` and `publish = false`, and have the crates inherit it with `.workspace = true`. Block-store keeps its own edition.
- Add `[workspace.lints.rust] unsafe_code = "deny"`, plus a small clippy set, for example `dbg_macro`, `todo` and `undocumented_unsafe_blocks` (warn). Every crate gets `[lints] workspace = true`. The FFI crates (shim, shim-dll, inject, win and the fixtures) `#![allow(unsafe_code)]` at module level, which makes the unsafe surface explicit.
- Add `[workspace.dependencies]` for the shared external crates and for the internal path crates.
- Fix the comment.

- **Value:** Med.
- **Effort:** M (38 manifests, mechanical).
- **Risk:** Low. A `deny` that newly fires in a crate is a compile error caught at once.
- **Haskill:** no. Haskill's own `rust-version` should be at least 1.89 to match.

### E15. vfs-proton oversized files

**Category:** structure.

**Evidence.**
- `prefix.rs` (1,212 lines; about 800 of code) mixes:
  - prefix initialisation (`ensure`, `ensure_with`, `run_proton_init`, `run_wineboot`, `:97-335`);
  - links and the manifest (`:341-603`);
  - the lock (`:594`);
  - wineserver control (`:621-665`);
  - drive mapping (`:674-705`, test-only, see E16);
  - process helpers (`run_bounded`, `own_process_group`, `spawn_retrying_busy`, `:725-797`), which `launch.rs` reaches into as `pub(crate)`.
- `launch.rs` (1,206 lines; 682 of code) mixes `WineLaunch`, env building, the injector report protocol and spawning.
- `nvapi.rs` (1,071 lines; 595 of code) mixes GPU and driver detection, status reporting (used by Haskill's options UI) and prefix install.

**Proposed change.**
- `prefix/{mod.rs (Prefix, PrefixError, lock), init.rs, links.rs, wineserver.rs}` and a crate-level `process.rs` for the spawn helpers.
- `launch/{mod.rs (WineLaunch, spawn, finish), env.rs (launch_env, reserved and stale, merge_dll_overrides, which shrinks after E3), injector.rs (error path and report)}`.
- `nvapi/{detect.rs, status.rs, install.rs}`.

Keep the module paths that Haskill uses (`nvapi::status`, `NvapiStatus`, `Capability`, `GpuModel`, `prefix::{Prefix, ensure_with, prefix_dir, PrefixInit}`, `layout::Root`, `runtime::*`) re-exported from the same paths.

- **Value:** Med.
- **Effort:** M.
- **Risk:** Low.
- **Haskill:** no, provided the paths are kept.

### E17. Stale comments in vfs-embed and its tests

**Category:** docs / dead narrative.

**Evidence.**
- "Session::serve has no non-Windows body yet" or "Windows-only until the Proton path lands":
  - `vfs-embed/src/lib.rs:381-384`;
  - `tests/embed_api.rs:59,329`;
  - `tests/zip_serve_integrity.rs:200`;
  - `tests/launch_vfs_content.rs:179,345`;
  - `tests/fuse_init_gate.rs:13-17`.

  Some of these tests could now run on unix. For example, `lib.rs:387` `session_serve_and_ring_read` only needs `serve()` and `ipc().client()`, and `IpcServe::client` (`vfs-director/src/ipc.rs:344`) is not Windows-gated.
- `tests/proton_launch.rs:299-303` says "staging a graph-only image is not wired to the Proton path (`launch` refuses it by name)". The unix `launch` now stages (`session.rs:1733-1747`).
- The Node binding was removed in `1f40e17`, but `Node` is still named as a current host:
  - session.rs `:50-51`, `:103-114`, `:1379-1382`, `:1810-1811`;
  - lib.rs `:4-5`, `:87-100`.
- The docs carry a lot of history narrative ("gate 4, Task 6b", "used to…", "this increment does not make"), for example `session.rs:266`, `:391-392`, `:740-745`, `:3316-3318`. Haskill developers read these rustdocs.

**Proposed change.**
- Delete or fix the stale lines.
- Un-gate the tests that are now portable.
- Rewrite the `LaunchOpts::shim_dll` doc (`:100-123`) and the lib.rs "What a host still has to build" section around "any embedding host" rather than Node and Python.
- Move the history into commit messages and the spec index (E12).

- **Value:** Med.
- **Effort:** S.
- **Risk:** Low.
- **Haskill:** no.

### E25. Haskill reaches past vfs-embed into vfs-proton

**Category:** API seam.

**Evidence.**
- `vfs-embed/src/lib.rs:7-10` says "A host is expected to name only this crate".
- Haskill imports directly:
  - `vfs_proton::layout::Root`, `prefix::{Prefix, ensure_with, prefix_dir}` and `runtime::installed_dirs` (`haskill-vfs/src/session.rs:13-15`);
  - `nvapi::{status, NvapiStatus, Capability, GpuModel}` (`haskill/src/options/gpu.rs`);
  - `prefix::{PrefixInit, prefix_dir}` and `layout` (`haskill/src/paths.rs`);
  - `runtime::{cmp_tags, verify_ge}` (`haskill-vfs/src/runtime.rs`);
  - `session.kernel()` in its tests (`haskill-vfs/tests/session.rs:257,285`).

**Proposed change.** Decide on one of two options:
- **(a)** Declare vfs-proton a supported second host crate and document it in lib.rs.
- **(b)** Re-export from vfs-embed (unix): `pub mod proton { pub use vfs_proton::{layout::Root, nvapi::{status, NvapiStatus, Capability, GpuModel}, runtime::{newest_installed, verify_ge, cmp_tags}, prefix::prefix_dir}; }`. Together with `Session::prepare_prefix` (E10), Haskill then names only vfs-embed for launching.

Option (b) matches the stated design.

- **Value:** Med.
- **Effort:** S.
- **Risk:** Low.
- **Haskill:** **yes**. Change the imports in the same change.

---

## Low value / nitpicks

### E14. Profiles

`rust/Cargo.toml` has only `panic = "unwind"`.
- Proposal: add `[profile.dev.package."*"] opt-level = 2` (or a targeted set: `zstd-sys`, `redb`, `blake3`, `ring`). Debug tests and the debug Proton e2e then move closer to release speed, which removes the main reason the docs push `--release` for e2e.
- Proposal: add `[profile.release] debug = "line-tables-only"`, so that Wine crash reports and core dumps from Haskill users symbolise.
- This also sets the profile rule for E7.

**Value:** Low–Med. **Effort:** S. **Risk:** Low. **Haskill:** Haskill has its own workspace profiles, so it is not affected; mirror the change there if wanted.

### E16. vfs-proton public surface, dead items and `WineLaunch` construction

- `src/lib.rs:31-46` re-exports about 40 items flat, while every module is also `pub`. All external callers (vfs-embed and Haskill) use the module paths. Trim the flat re-exports, or document one style.
- Items with no production caller:
  - `launch::run` (`launch.rs:519`; only `tests/launch_log.rs:133`);
  - `Prefix::map_drive` and `unmap_drive` (`prefix.rs:674-705`; only its own tests, because Session uses `drive_c` symlinks; `PrefixError` docs still mention them, `:21`);
  - `prefix::ensure` (only called by `ensure_with`).

  Remove them, or make them `pub(crate)` and `#[cfg(test)]`.
- `WineLaunch` has 25 `pub` fields and no constructor. It is built literally in four places: `session.rs:2207-2236`, `session.rs:2440-2465` (test), `launch.rs:741` (test `sample()`) and `tests/launch_log.rs:~40-60`. Every new field touches all four. Add `WineLaunch::new(runtime, prefix, artefacts, target, ring_geometry, …)` with `Default`-like optionals, or group the fields into a `RingGeometry` struct and an `Artifacts` struct.
- `PrefixLock(#[allow(dead_code)] File)` (`prefix.rs:338`): use a named `_file` field instead of the allow.

**Value:** Low–Med. **Effort:** S. **Risk:** Low. **Haskill:** check `vfs_proton::Root`-style flat paths (none found) before trimming.

### E18. Small duplications in vfs-embed

- `KernelSource::read` (`session.rs:229-252`) and `Session::read_file_at` (`:1355-1373`) are the same open/read-loop/close. Share one `read_whole` helper.
- `EMPTY_TREE_SNAPSHOT_HEX` plus a hand-written hex decoder (`:3047-3078`). Use a `const [u8; 128]` literal, or build it from vfs-shared. The comment "so vfs-director does not need…" is stale, because this code now lives in vfs-embed.
- `PROTON_PAYLOAD_CAP` restates `vfs_ipc::DEFAULT_PAYLOAD_CAP` (`:3121-3130`). Re-export it from `vfs_director::ipc`. The API is also asymmetric: the file-backed `start` takes a cap, while the section `start` hard-codes it (`ipc.rs:154`).
- The ready timeout's 180 s default lives in the session on Windows (`:1905-1910`), separately from the injector's default.
- `ResolvedImage` carries `cfg_attr(..., allow(dead_code))` on all three fields (`:3013-3030`), and `resolve_for_test` (`:1754`) exists only to read a field that unix ignores. Return what each platform needs, or keep `host` and drop the allows.

**Value:** Low. **Effort:** S. **Risk:** Low. **Haskill:** no.

### E19. Error strings with embedded runs of spaces

`vfs-proton/src/launch.rs:647` and `:653` continue a string literal onto the next line without the trailing `\`. The messages therefore read `"…the child would              map a view…"` and `"…of a file              faults on touch…"`.

**Fix:** add `\` at the line ends. **Value:** Low. **Effort:** S. **Risk:** Low. **Haskill:** no.

### E20. Fixtures

All nine fixture crates are used:

| fixture | used by |
|---|---|
| read | vfs-embed Windows and Proton tests, directord `e2e`/`proton_cli` |
| writepath, escape, prefs | directord `e2e` (Windows) |
| staticimp, vproxy | vfs-inject tests and build.rs |
| steam | `proton_steam` |
| nvapi | `proton_nvapi` |
| registry | `proton_registry` |

None are dead. The inconsistencies:
- **Platform gating differs.** read, steam, nvapi and registry gate their Windows code and build a stub elsewhere. escape (1,550-line `main.rs`, plus a 468-line `ffi.rs`), prefs, writepath and staticimp do not. That is one reason `cargo build --workspace` fails on Linux, and it keeps them out of any Linux clippy.
- **Env names** are read as raw literals (`vfs-fixture-prefs/src/main.rs:361-419`, `vfs-fixture-writepath/src/main.rs:46,267`), although vfs-env has no dependencies and has a constant for each.
- **Output protocols differ:**
  - exit code only: read, writepath, staticimp;
  - one line per vector: escape;
  - `x-probe: k=v`: steam, nvapi;
  - `reg: …`: registry.
- **Merge candidates** that would cut cross-build link steps and the artefact lists (E21):
  - steam and nvapi become one `vfs-fixture-probe <steam|nvapi>`. Both load a DLL by name, print `k=v` probe lines and are Proton-only;
  - read, writepath and possibly prefs become `vfs-fixture-io <read|write|ini>`. All three are Win32 file-I/O targets driven by `VFS_FIXTURE_*`.
  - Keep escape, registry and staticimp/vproxy separate, because they have distinct link or ABI needs.
- **Split** `vfs-fixture-escape/src/main.rs` into `vectors/{names,devices,handles,links,case}.rs` plus `outcome.rs`. The vector functions sit at `:558-1100+`.

**Value:** Low–Med. Build-time gain is modest, about one link per removed binary. **Effort:** M. **Risk:** Low. **Haskill:** no.

### E21. bin/build-windows

- It has `set -euo pipefail` and a usage line, but:
  - it does not check for prerequisites (`cargo xwin`, the rustup target, clang and lld), so a missing tool surfaces deep in cargo;
  - any first argument other than `--release` silently builds debug (`:21`), so `--relase` gives a debug build;
  - there is no `--help`.
- The artefact list is hand-maintained in five places, which drift independently:
  - the script (`:46-55`);
  - `locate_wine_artifacts` (`session.rs:3337-3347`);
  - each test's list (E5);
  - Haskill's `haskill-vfs/src/artifacts.rs:15-19`;
  - the Windows CI `cargo build -p` list (`ci.yml:82`).

**Proposal:**
- Check the tools up front with clear messages, reject unknown arguments, and add `--help`.
- Write a manifest `target/<profile>/windows-artifacts.txt` (name and build hash) that tests and Haskill can read.
- Or add `pub const WINDOWS_ARTIFACTS: &[&str]` in vfs-embed (unix), which Haskill and the tests import.

**Value:** Low–Med. **Effort:** S. **Risk:** Low. **Haskill:** yes, if the constant is adopted.

### E22. Committed scratch and odd files

- `rust/scratchpad/spike-b/`: 11 tracked files with their own `Cargo.lock`, a 2026-07-13 instrumentation-callback spike that was never a workspace member. Delete it; it stays in history.
- `rust/docs/benchmarks/`: 12 raw timestamped dumps (`fuse-rpc-2026-07-15T14-52-22…md` …) next to `fuse-rpc-latest.md`, plus the Node-era `node-*.md`. Keep the summaries and archive the raw runs.
- `rust/.gitignore` starts with a UTF-8 BOM (`\ufefftarget/`), and the BOM may stop that `target/` line from matching. It also has an unexplained `mcps/` entry.
- The root `.gitignore` still carries Clojure entries (`.cpcache/`, `.nrepl-port`, `.clj-kondo`) after the JVM removal.
- `resources/*.edn` is a Clojure-era format but still a live golden file regenerated in CI. Keep it, and say so in `bin/regen-protocol`.
- The largest tracked files are source: `vfs-shim/src/hook.rs` at 328 KB and session.rs at 180 KB. There are no stray binaries.

**Value:** Low. **Effort:** S. **Risk:** Low. **Haskill:** no.

### E23. vfs-env

- `REGISTRY` is `Kind::Behaviour` (`lib.rs:487`) but is a handshake name; fold this into E3.
- The drift test (`:699-721`) catches unknown names but not:
  - names spelled as string literals instead of constants. There are about 30 in tests and fixtures (see E20) plus `launch.rs:587-592`. Add a test that no non-test, non-fixture source file contains a `"VFS_…"` literal;
  - handshake completeness (E3).
- The skip condition in `visit` (`:763-767`) is a convoluted `||`/`&&` mix that tests the same thing twice. Simplify it to `p.ends_with("vfs-env/src/lib.rs")`.
- The tests mutate process env with `set_var` while running in parallel threads (unique names, but still the unsound API in 2024 terms). Acceptable; note it for the edition-2024 migration.

**Value:** Low–Med. **Effort:** S. **Risk:** Low. **Haskill:** no.

### E24. `LaunchOpts` shape

- `shim_dll` and `payload_dll` are `Option<String>` (`session.rs:123-124`), while `log_file` is `Option<PathBuf>`.
- The injector has no field: its doc says "adding one is a change to a public struct, which this increment does not make" (`:3316-3318`), so it is looked for beside `shim_dll`.
- `declare_root` means a host directory on Windows and a `C:\` location on unix (`:755-760`), and the same field `extra_roots` stores either.
- Proposal:
  - make the paths `Option<PathBuf>`;
  - add `injector: Option<PathBuf>`;
  - on unix, consider `declare_root_location` and keep `declare_root` as an alias.

**Value:** Low–Med. **Effort:** S. **Risk:** Med (public API). **Haskill:** **yes**: `launch_opts` (`haskill-vfs/src/session.rs:302-303`) and `artifacts.rs`.

---

## Suggested order of work

1. **Quick correctness fixes**, each a small PR:
   - E4: `Prefix::lock` bounded retry, which ends the flake;
   - E19: string continuations;
   - E9: the ring-length check uses the host path.
2. **Gates.** E8: Linux clippy, `proton_registry` in CI, fmt and docs, cache. Land it after E4, or CI goes red on the flake.
3. **Doc truth.** E17 and E11: stale comments, architecture.md, READMEs, and the README's Proton-test paragraph. These are cheap, and they stop misleading readers before the bigger moves.
4. **Harness.** E5, then E6 and E7: the test-support crate, the prerequisite policy and the artefact rule. This makes the next refactors safe to verify locally.
5. **Env single source.** E3 (with E23), which needs the gates from step 2 and the harness from step 4.
6. **session.rs.** E1 as a pure move PR first, then E2 (shared steps, `ProtonState`) and E18.
7. **vfs-proton.** E15 (split), then E16 (surface trim, `WineLaunch` constructor).
8. **API changes landed in lockstep with Haskill** (one aether-vfs PR, then the submodule bump plus the Haskill PR):
   - E10: `Session::prepare_prefix`, `runtime::newest_installed`;
   - E25: vfs-embed `proton` re-exports;
   - E24: `LaunchOpts` path types and injector field;
   - E21: the shared artefact list.
9. **Workspace config.** E13 (workspace package, lints, deps, rust-version) and E14 (profiles).
10. **Hygiene.** E12 (docs index and archive), E20 (fixture tidy and merges), E21 (script robustness), E22 (scratch and gitignore).

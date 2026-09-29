# The `vfs` CLI and daemon on Linux — design

**Status:** approved in conversation 2026-09-29; this document is the written
form for review.

## 1. Goal

`vfs launch` and `vfs up --config` work on a Linux host: the daemon composes a
session, and the game — a Windows program — runs under GE-Proton with the shim
injected, served by the native Linux Director. The purpose is to make a real
game testable on Linux through the same CLI used on Windows. Which game comes
later; this increment makes the CLI path general.

**Done means:** a Proton-hosted program launched by `vfs up --config` on Linux
is **staged** out of the provider graph, reads its content through the Director
(including from a zip source), and writes into a **second root** whose bytes
land in that root's overlay — proved by an e2e test that CI runs.

## 2. Where things stand

Measured 2026-09-29, not assumed:

- `cargo build -p vfs-directord --bin vfs` **already succeeds on Linux.** The
  daemon reaches everything through `vfs_embed::Session` (enforced by
  `registry.rs`'s `daemon_names_only_the_embed_api`), talks gRPC over loopback
  TCP, discovers via a JSON file, and checks liveness via `/proc` on unix. None
  of that is Windows-specific.
- Only the `skyrim-live` bin fails to compile (`std::os::windows::fs::MetadataExt`
  at `skyrim-live.rs:1879`, `vfs_win` at `:631`).
- A smoke run of `vfs launch --source disk:<dir>@/ --exec fixture.exe` on Linux
  auto-spawns the daemon, creates the session and streams the launch, then
  fails: *"…not a real file — root 0's provider graph does serve that vpath, so
  staging is what is missing"*.

So the port is not a transport or build problem. It is three gaps in the unix
`Session::launch` plus some CLI hygiene.

## 3. Staging on Linux

The unix `launch` resolves `opts.image` the way the Windows body does:

1. absolute Windows path (`C:\…`, `\\?\…`, UNC) — handed through, as today;
2. relative, and a real file under the managed root — launched from there;
3. relative, and a vpath root 0's graph serves — **staged** with
   `Session::stage_launch`, then launched;
4. otherwise — refused by name, as today.

`stage_launch` already stages **into the virtual root at the image's own
vpath** (`stage_launch_into(.., &self.virtual_root, ..)`) and mounts the
staging directory back under the curated graph. On unix the virtual root is
already linked into the prefix as `C:\vfs-session\root`, so the staged image is
a real file Wine can open, and `wine_target` accepts it through its existing
"absolute host path inside the managed root" case. No new path rewriting.

Changes:

- `KernelSource` (the `ImageSource` over the session's own graph) loses its
  `#[cfg(windows)]`; it is portable Director calls only.
- `LaunchOpts::stage_also` and `stage_fallback_dirs` take effect on unix.
  `stage_fallback_dirs` names **host** directories on both targets (it is read
  by the host process, not the child).
- `wine_target`'s refusal of graph-only images, and its doc comment, go.

Staging the image and its **PE import closure** is required under Wine for the
same reason as on Windows: the loader maps the image and resolves static
imports before any hook exists.

## 4. Roots beyond root 0

Today the unix `launch` refuses any declared root beyond 0, because the Wine
launch env carries no root map (`launch_env` sets `VFS_VIRTUAL_DIR` and
explicitly clears `VFS_VIRTUAL_ROOTS`).

- `WineLaunch` gains `virtual_roots: Vec<(u32, String)>`. `launch_env` emits it
  as `VFS_VIRTUAL_ROOTS` in the **same `id=path;id=path` format**
  `IpcServe::apply_env_roots` writes on Windows, so the shim's parser is
  unchanged. Empty → the variable stays explicitly cleared, as today.
- On unix an extra root **must be declared as a drive-letter Windows path** —
  the path the game uses (e.g. `C:\users\steamuser\Documents\My Games\X`). A
  host path is refused at launch with a message naming the root and the
  required form. (Mapping host paths to prefix paths is possible later; nothing
  needs it now.)
- Before launch, each extra root on drive C is created as an **empty real
  directory** inside the prefix (`drive_c/…`). Root 0 is a real, empty
  directory for the same reason: Wine's path walk and parent enumeration must
  find the directory itself, while its contents are the shim's to serve. Roots
  on other drive letters are refused (no such drive exists in our prefixes,
  which drop `Z:` and map nothing else).
- The refusal "this session declares N root(s) beyond root 0" goes.

Roots are case-insensitive on the Windows side; the shim already folds per the
case-fold contract, so the declared spelling does not need normalizing here.

## 5. Prefix lifetime

`wine_session_id()` hashes `state_dir`, and the daemon puts every session's
state under a fresh `temp_dir()/vfs-daemon-<pid>-<seq>-<id>`. So **every daemon
session boots a brand-new ~627 MB prefix** (a slow `wineboot`) **and nothing
ever deletes it**. Four such prefixes were already on the development machine,
all from test runs. For a CLI that creates a session per `vfs up`, that is both
a disk leak and a per-run boot cost, and a game's own prefix state (registry,
settings) never survives a run.

- **Named prefixes.** `Session::set_prefix_name(&str)` (unix-only) selects a
  persistent prefix at `$VFS_HOME/sessions/<name>/`. It is reused across runs
  and never deleted by aether-vfs. The name is validated with the same
  traversal-safe rule the hashed id already satisfies (one plain path
  component).
- **Anonymous prefixes** (no name set) keep the hashed id — unique to the
  session's `state_dir`, so no other session uses it — and are **deleted when
  the `Session` is dropped**. The launch has already returned by then, since
  unix launches always wait. Tests and embedders stop leaking.
- **The daemon** names a session's prefix after its session name — `[session]
  name` in a config, or a new `--name` on `vfs launch`. `vfs launch` without
  `--name` is anonymous.
- **Exclusive use.** `launch` takes an exclusive advisory lock (a lock file in
  the prefix directory) for the duration of the launch. A second live session
  on the same prefix fails fast with a message naming the prefix, instead of
  both rewriting `drive_c/vfs-session/*` links under each other.

## 6. CLI, daemon and CI

- `skyrim-live` builds on Windows only: its body is gated `#[cfg(windows)]`,
  and non-Windows gets a `main` that exits with a one-line message. Porting it
  is out of scope.
- `vfs-directord`'s Windows-only e2e tests (the launch-based ones in
  `tests/e2e.rs`; `staging.rs`'s
  `production_launch_stages_a_relative_image_before_create_process`) are gated
  `#[cfg(windows)]`, so `cargo test -p vfs-directord` runs cleanly on Linux.
  The portable tests (`composition.rs`, `copy_on_write_daemon.rs`, the unit
  tests, the two registry-only tests in `e2e.rs`) keep running on both.
- The auto-spawned daemon is put in its **own process group** on unix
  (`CommandExt::process_group(0)`, std-only), so Ctrl-C in the terminal that
  started it does not kill it. This mirrors `CREATE_NEW_PROCESS_GROUP |
  DETACHED_PROCESS` on Windows.
- `vfs launch --wait` is `#[arg(long, default_value_t = true)] wait: bool`,
  which clap treats as a set-true flag: the CLI can never send `wait: false`.
  It becomes `--no-wait` (a real opt-out). On unix a no-wait launch is still
  refused by `Session::launch`, with the existing message.
- The daemon finds `vfs-injector.exe`, `vfs_shim_dll.dll` and
  `vfs_payload.dll` beside `current_exe()` — the `vfs` binary — which is
  where `bin/build-windows` already copies them. No new lookup.
- README's daemon section gains the Linux form of `vfs up` / `vfs launch`.

## 7. Verification

| gate | proves | where |
|---|---|---|
| `cargo test --no-fail-fast` on Windows | no Windows regression | CI `rust-windows` |
| `cargo clippy --all-targets -- -D warnings` | lint parity | CI, and locally on Linux for touched crates |
| `cargo test -p vfs-directord` on Linux | daemon + portable tests run on Linux | CI `rust-linux-portable` (added) |
| `proton_launch` (existing) | the embed path still works | CI `proton-linux` |
| **new** `vfs-directord/tests/proton_cli.rs` (`#[ignore]`d, `#![cfg(unix)]`) | the whole CLI path | CI `proton-linux` |

The new e2e drives the **real `vfs` binary** with `vfs up --config`, against a
config with:

- a disk source containing only the fixture **exe** — it is not under the
  managed root, so the launch must **stage** it;
- a zip source containing the data file the fixture reads;
- a second root at `C:\users\steamuser\vfs-e2e-save` with a write layer, into
  which the fixture writes a file.

Asserted: the fixture exits 0 having read the expected bytes (it checks length
and fill itself); the written file exists in **root 1's write layer on the
host**, and nowhere under the prefix's real `drive_c`. The fixture is
`vfs-fixture-read` if it can take a write target by env; otherwise
`vfs-fixture-writepath`, whichever needs no new fixture code — decided by
reading both in the plan, not here.

Also: a unit test that `launch_env` emits `VFS_VIRTUAL_ROOTS` in the
`apply_env_roots` format, and one that an anonymous session's prefix is gone
after drop while a named one survives.

## 8. What must not regress

- **No Windows behaviour change.** Staging, roots and the launch are the
  Windows bodies' existing semantics brought to unix; the Windows `launch` is
  untouched. `KernelSource` losing its cfg changes nothing on Windows.
- **No wire change.** `VFS_VIRTUAL_ROOTS`' format is reused, not extended.
  `bin/regen-protocol` + `git diff --exit-code resources/` stay clean.
- **No unregistered env switch.** Nothing new is read; `VFS_VIRTUAL_ROOTS` is
  already in `vfs_env::ALL`.
- `vfs-embed` public API: additive only (`Session::set_prefix_name`).

## 9. Out of scope

- Detached (`wait: false`) launches on Linux.
- Porting `skyrim-live` or `tools/gamectl.ps1`.
- Host-path extra roots; drive letters other than `C:`.
- Launching through Steam, umu or pressure-vessel.
- Picking and running a real game — the next step, which this unblocks.

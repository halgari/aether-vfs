# The `vfs` CLI and daemon on Linux, and rooted launches — design

**Status:** implemented (approved in conversation 2026-09-29): the `vfs` CLI and daemon on Linux.

## 1. Goal

A user **constructs a VFS setup** — roots at the locations the program will
see, sources and write layers composed onto them — and then **launches a
program by its path inside that setup**. That works the same on Windows and on
Linux, where the program runs under GE-Proton with the shim injected, served
by the native Linux Director.

The immediate purpose is to make a real game testable on Linux through the
same CLI used on Windows. Which game comes later; this increment makes the
path general.

**Done means:** on Linux, `vfs up --config` brings a two-root setup up, and
`vfs exec --session e2e '{Games}\fixture.exe'` launches a program that exists
only in the provider graph: it is **staged**, reads content through the
Director (including from a zip source), and writes into a **second root** whose
bytes land in that root's write layer — proved by an e2e test that CI runs. On
Windows, a rooted launch of a graph-only program stages it, proved by a
Windows e2e test.

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
- The unix `Session::launch` also refuses roots beyond root 0, and puts root 0
  at a fixed `C:\vfs-session\root` — not where a game expects its install.
- Every session gets a new ~627 MB Wine prefix that is never deleted (§6).

So the port is not a transport or build problem. It is the launch model,
staging, roots and prefix lifetime, plus CLI hygiene.

## 3. Roots have locations

Every root, **including root 0**, has a **location**: the absolute path the
launched program sees.

- **Windows:** a host path (the program and the host share a namespace).
- **Linux:** a drive-letter path inside the prefix, `C:\…`. Other drive letters
  are refused — our prefixes drop `Z:` and map nothing else.

Declaring:

- `[[root]]` may now declare **id 0**. `SessionRegistry::declare_root` stops
  refusing it; the session summary the daemon reports (`root`) is updated to
  the declared location.
- Root 0 undeclared keeps today's default: the daemon's temp directory on
  Windows, `C:\vfs-session\root` on Linux. Existing configs, tests and
  `skyrim-live` behave exactly as before.
- Every `[[root]]` keeps its `name`. Names must be unique within a session
  (case-insensitively) — `SessionConfig::validate_roots` enforces it — because
  §4 resolves them.

How a location is realized:

- **Windows:** used directly, as declared extra roots already are. Created if
  missing. Anything already in it is hidden by the shim, which serves the root
  fully virtually. Staging into it writes only files it creates, never
  overwrites an existing file, and removes only what it wrote
  (`vfs_director::stage`'s existing `owns_dir: false` rules).
- **Linux:** each location is backed by a **host directory**, symlinked into the
  prefix's `drive_c` at the location. Root 0 is backed by the session's managed
  root (`set_root`, as today); extra roots by an empty `state_dir/roots/<id>`.
  Missing parent directories under `drive_c` are created. The prefix gains only
  the links (and those parents); links are removed when the session drops.
  This replaces the fixed `drive_c/vfs-session/root` link; overlay and state
  keep their `vfs-session` links.

`Session` API (additive): on unix, `declare_root(id, location)` takes the
program-visible location; `set_root` remains root 0's **host** backing
directory. On Windows both mean what they mean today.

Roots reach the shim as today: root 0 as `VFS_VIRTUAL_DIR`, the rest as
`VFS_VIRTUAL_ROOTS` in the `id=path;id=path` format `IpcServe::apply_env_roots`
already writes on Windows. On Linux, `WineLaunch` gains `virtual_roots:
Vec<(u32, String)>` and `launch_env` emits it (empty → explicitly cleared, as
today). The refusal "N root(s) beyond root 0" goes. Locations are compared
case-insensitively; the shim already folds per the case-fold contract.

## 4. Launching by path

`LaunchOpts::image` (and the CLI's program argument) takes three forms, on both
platforms:

1. **`{Name}\rest`** — `{Name}` is a root's `[[root]]` name, replaced by that
   root's location; resolution continues as form 2. An unknown name is refused,
   listing the session's root names. Names match case-insensitively. This lets
   `vfs exec` reuse a session's mappings without restating them. Expansion is
   the **daemon's** job: names are config-level, and `Session` knows only ids
   and locations.
2. **Absolute path.**
   - **Inside a root's location** (longest matching location wins if roots
     nest): the remainder is the vpath within that root.
     - A real file at that path → launched as is.
     - Otherwise, a vpath the root's graph serves → **staged**, then launched
       from the staged path. Staging is **root 0 only** for now: a graph-only
       image under another root is refused by name.
     - Otherwise → refused: the root does not serve that vpath.
   - **Outside every root** → launched as given: a real program, not VFS
     content. This keeps `exec = "C:/tools/my-probe.exe"` working on Windows
     and lets a prefix's own programs run on Linux.
3. **Relative** — shorthand for root 0 (`{root 0's location}\rest`), exactly as
   today. The Windows e2e suite and `skyrim-live` keep using it.

Staging (`Session::stage_launch`) is unchanged: it writes the image and its
**PE import closure** into root 0's directory at the image's own vpath, mounts
that directory back **under** the curated graph, and holds it for the
session's lifetime. On Linux that directory is root 0's host backing dir, which
Wine sees at root 0's location. Under Wine staging is needed for the same
reason as on Windows: the loader maps the image and resolves static imports
before any hook exists.

Changes this implies:

- `KernelSource` (the `ImageSource` over the session's graph) loses its
  `#[cfg(windows)]`.
- One portable resolver, `resolve_image`, implements forms 2–3 for both
  `launch` bodies, returning either "launch this real path" or "stage this
  vpath of root 0". Each body then maps the result to its target (a host path
  on Windows; a `C:\` path on Linux). `wine_target`'s graph-only refusal goes.
- `LaunchOpts::stage_also` and `stage_fallback_dirs` take effect on Linux.
  `stage_fallback_dirs` names **host** directories on both (the host reads
  them, not the child).
- **Windows behaviour change, deliberately:** an absolute image inside root 0
  that is not a real file was launched as given and failed in
  `CreateProcess`; it is now staged. Every other Windows launch resolves as
  before.

## 5. CLI

- **`vfs up --config`** without a `[launch]` block brings the setup up and
  leaves it running (it already creates the session before launching; the
  session outlives the command).
- **`vfs exec --session <id|name> <path> [-- args…]`** (new) launches into a
  live session. The gRPC `Launch` RPC already takes a session id, so this is a
  client command plus name-or-id lookup and `{Name}` expansion in the daemon.
  A name shared by two live sessions is refused as ambiguous, listing their
  ids.
  `--env KEY=VALUE` is repeatable, as on `launch`.
- **`vfs launch`** (the one-shot form) gains `--root ID=NAME=LOCATION`
  (repeatable) and `--name`, so it can express the same setup as a config.
  `--exec` accepts all three path forms.
- `--wait` is `#[arg(long, default_value_t = true)] wait: bool`, which clap
  treats as a set-true flag, so the CLI can never send `wait: false`. It
  becomes `--no-wait`. On Linux a no-wait launch is still refused by
  `Session::launch`, with the existing message.

## 6. Prefix lifetime (Linux)

`wine_session_id()` hashes `state_dir`, and the daemon puts every session's
state under a fresh `temp_dir()/vfs-daemon-<pid>-<seq>-<id>`. So **every daemon
session boots a brand-new ~627 MB prefix** (a slow `wineboot`) **and nothing
ever deletes it**. Four were already on the development machine, all from test
runs. For a CLI that creates a session per `vfs up`, that is both a disk leak
and a per-run boot cost, and a game's prefix state (registry, settings) never
survives a run.

- **Named prefixes.** `Session::set_prefix_name(&str)` (unix-only) selects a
  persistent prefix at `$VFS_HOME/sessions/<name>/`, reused across runs and
  never deleted by aether-vfs. The name must be one plain path component (the
  traversal-safe rule the hashed id already satisfies).
- **Anonymous prefixes** (no name) keep the hashed id — unique to the
  session's `state_dir` — and are **deleted when the `Session` drops**. A
  launch has always returned by then, since Linux launches wait.
- **The daemon** names a session's prefix after its session name (`[session]
  name`, or `vfs launch --name`). No name → anonymous.
- **Exclusive use.** `launch` holds an exclusive advisory lock (a lock file in
  the prefix directory) while it runs. A second live session on the same
  prefix fails fast, naming the prefix, instead of both rewriting the same
  root links under each other.

## 7. Daemon, build and CI hygiene

- `skyrim-live` builds on Windows only: its body is gated `#[cfg(windows)]`,
  and non-Windows gets a `main` that exits with a one-line message. Porting it
  is out of scope.
- `vfs-directord`'s Windows-only e2e tests (the launch-based ones in
  `tests/e2e.rs`; `staging.rs`'s
  `production_launch_stages_a_relative_image_before_create_process`) are gated
  `#[cfg(windows)]`, so `cargo test -p vfs-directord` runs on Linux. The
  portable tests (`composition.rs`, `copy_on_write_daemon.rs`, the unit tests,
  the two registry-only tests in `e2e.rs`) run on both.
- The auto-spawned daemon gets its **own process group** on unix
  (`CommandExt::process_group(0)`, std-only), so Ctrl-C in the terminal that
  started it does not kill it — the counterpart of `CREATE_NEW_PROCESS_GROUP |
  DETACHED_PROCESS` on Windows.
- The daemon finds `vfs-injector.exe`, `vfs_shim_dll.dll` and
  `vfs_payload.dll` beside the `vfs` binary, where `bin/build-windows` already
  copies them. No new lookup.
- README: the root/location model, the three launch forms, `vfs up` + `vfs
  exec`, and the Linux form of each.

## 8. Verification

| gate | proves | where |
|---|---|---|
| `cargo test --no-fail-fast` on Windows | no Windows regression, plus the new Windows e2e | CI `rust-windows` |
| `cargo clippy --all-targets -- -D warnings` | lint parity | CI; locally on Linux for touched crates |
| `cargo test -p vfs-directord` on Linux | daemon + portable tests run on Linux | CI `rust-linux-portable` (added) |
| `proton_launch` (existing) | the embed path still works | CI `proton-linux` |
| **new** `vfs-directord/tests/proton_cli.rs` (`#[ignore]`d, `#![cfg(unix)]`) | the whole CLI path on Linux | CI `proton-linux` |

**Linux e2e** — drives the real `vfs` binary. `vfs up --config` with:

- root 0, name `Games`, at `C:\Games\Fixture`, sourced from a disk directory
  holding only the fixture **exe** (so it must be staged) and a zip holding the
  data file it reads;
- root 1, name `Saves`, at `C:\users\steamuser\vfs-e2e-save`, with a write
  layer;
- `[session] name = "e2e"`, no `[launch]`.

Then `vfs exec --session e2e '{Games}\fixture.exe'`. Asserted: the fixture
exits 0 having read the expected bytes (it checks length and fill itself); the
file it writes under `C:\users\steamuser\vfs-e2e-save` exists in **root 1's
write layer on the host**; the prefix's real `drive_c` holds only the root
links. Then a second `vfs exec` with the absolute form
`C:\Games\Fixture\fixture.exe` also succeeds — the session's mappings are
reused, not rebuilt. The fixture is `vfs-fixture-read` if it can be told to
write by env; otherwise `vfs-fixture-writepath` — whichever needs no new
fixture code, decided by reading both in the plan.

**Windows e2e** (in the existing `e2e.rs` suite): a session with root 0
declared at a fresh directory, a disk source serving the fixture exe, and a
launch by the absolute `{root 0}\fixture.exe` and by `{Name}\fixture.exe` —
both stage and run. Plus: a launch outside every root still runs as given.

**Unit tests:** `resolve_image` across the three forms (inside/outside/nested
roots, real vs graph-only vs missing, non-root-0 graph-only refusal);
`{Name}` expansion (unknown name, case-insensitive match); `launch_env` emits
`VFS_VIRTUAL_ROOTS` in the `apply_env_roots` format; an anonymous session's
prefix is gone after drop while a named one survives; duplicate root names are
rejected by `validate_roots`.

## 9. What must not regress

- **Windows:** every launch resolves as before except the one deliberate change
  in §4 (absolute image inside root 0, not a real file → staged). The existing
  Windows suite is the check.
- **No wire change.** `VFS_VIRTUAL_ROOTS`' format is reused, so
  `bin/regen-protocol` + `git diff --exit-code resources/` stay clean. The
  gRPC `LaunchReq` needs no new field either: the daemon resolves its
  `session_id` string as an id first, then as a session name, and expands
  `{Name}` in `exec` — only `exec`'s comment in `director.proto` changes.
  `DeclareRootReq` does gain an additive `string name = 4`: the daemon cannot
  resolve `{Name}` without learning root names, and today that RPC carries
  only an id and a path. `director.proto` is not part of the regenerated
  protocol descriptor, so this is a compatible gRPC change, not wire drift.
- **Windows config at launch.** Windows `serve` writes `shim.cfg` and
  `fuse.cfg` with root 0's location at session creation, before a config can
  declare root 0. The Windows `launch` rewrites both from the current
  location, so a declared root 0 is what the shim is told.
- **No unregistered env switch.** `VFS_VIRTUAL_ROOTS` is already in
  `vfs_env::ALL`; nothing new is read.
- **`vfs-embed` API:** additive only (`Session::set_prefix_name`; `declare_root`
  on unix taking a location is a meaning for a call that was not usable there).

## 10. Out of scope

- Detached (`wait: false`) launches on Linux.
- Staging images that live in roots other than root 0.
- Host-path roots on Linux; drive letters other than `C:`.
- Porting `skyrim-live` or `tools/gamectl.ps1`.
- Launching through Steam, umu or pressure-vessel.
- Picking and running a real game — the next step, which this unblocks.

# aether-vfs

Userspace virtual filesystem for **Windows game modding**, on Windows and on
Linux under GE-Proton: compose base game + mods from pluggable sources (disk,
zip, or a host's own providers), inject a thin NT-API shim into the game, and
serve remapped I/O from a **Rust director** embedded in a host program.

A host composes a session in code against `vfs-embed` — see
[Embedding](#embedding) below. The data plane is a shared-memory ring between
the director and the injected shim.

> Pure Rust. The former Clojure/JVM layer has been removed (M4).

## Add-ons

Optional crates live in [`addons/`](addons/README.md), a separate Cargo
workspace: storage (block store, pull-through cache, named layers) and
downloaders for Steam, Nexus Mods, the Wabbajack CDN and GOG, each with an
optional `Provider` adapter. The core never depends on them.

## Documentation

| Document | For |
|---|---|
| [Architectural overview](rust/docs/architecture.md) | Engineers: how the system fits together, and how the hard parts are solved |
| [Product overview](docs/product-overview.md) | Non-technical: what it does and why it matters |
| [Benchmarks](rust/docs/benchmarks/) | Measurements and the analysis behind them |
| [Durability](rust/docs/durability.md) | What is durable when, for named layers and the registry overlay |
| [Shim invariants](docs/shim-invariants.md) | The rules the NT hooks keep, and the incidents behind them |
| [Design docs](docs/superpowers/README.md) | Index of specs, plans and reviews (current and archived) |
| [Code audit](rust/docs/audit-2026-08-13.md) | Historical full-tree review (names as of 2026-08-13) |
| [vfs-summary.md](rust/docs/vfs-summary.md) | Earlier long-form technical narrative (historical) |

## Quick start

```powershell
cd rust
cargo build -p vfs-embed -p vfs-shim-dll -p vfs-fixture-read
```

### Linux (Proton)

On Linux the game is still a Windows program, and so is the shim: it is injected
into the game inside GE-Proton's Wine, while the Director runs natively on Linux
and serves it over a file-backed ring. So the Windows half is cross-built, and
the Linux half is plain `cargo`:

```bash
# one-time toolchain (Arch; other distros: clang, lld, llvm)
sudo pacman -S --needed clang lld llvm
rustup target add x86_64-pc-windows-msvc
cargo install --locked cargo-xwin

bin/build-windows                       # injector, shim, payload, fixture -> rust/target/debug/
cd rust && cargo run -p vfs-proton -- install   # verified GE-Proton under ~/.local/share/aether-vfs

# end to end: a Windows fixture under Proton reads a file only the Linux Director serves
cargo test -p vfs-embed --test proton_launch -- --ignored

# end to end: the registry overlay (the same scripted registry run with and without a
# registry layer gives the same transcript, and the real registry is left unchanged).
# The test looks for the Windows artefacts in its own profile's directory, so build
# both in the same profile:
bin/build-windows --release
cd rust && cargo test --release -p vfs-embed --test proton_registry -- --ignored
```

The Proton tests are all `#[ignore]`d. Run with `-- --ignored`: a test whose prerequisite is
missing (runtime, Windows artifacts, Steam client, `steam_api64.dll`, local Skyrim, NVIDIA
GPU) prints `SKIP <test>: <reason>` on stderr, names the command to run, and passes; use
`--nocapture` to see it. `VFS_TEST_REQUIRE_ALL=1` turns every skip into a failure, for a
fully provisioned machine. The tests never use `/tmp` or your aether home: scratch, Wine
prefixes and sessions live under `rust/target/tmp`. The runtime is only read: it is the
newest verified one under `VFS_HOME` (else the XDG data home, `~/.local/share/aether-vfs`),
or the directory `VFS_TEST_PROTON_RUNTIME` names. `VFS_HOME` is not required.

The Windows artifacts live in `target/<profile>` (`bin/build-windows` for debug,
`bin/build-windows --release` for release), and a test looks only in its own profile, because
a shim from another build can carry another ring `VERSION` and refuse to attach. If they are
missing there but present in the other profile, the skip message says so. Set
`VFS_WINDOWS_ARTIFACTS=<dir>` to use one directory instead. The ring layout is versioned
(ring `VERSION` 4 since the registry overlay added the `reg_gen` header field): rerun
`bin/build-windows` after pulling a ring change.

Other test variables: `VFS_TEST_STEAM_CLIENT` (default `~/.local/share/Steam`),
`VFS_TEST_STEAM_API_DLL`, `VFS_TEST_STEAM_APP_ID`, `VFS_TEST_SKYRIM_DIR` (and the
`VFS_TEST_SKYRIM_*` knobs in `proton_skyrim.rs`), `VFS_TEST_WINEDEBUG`.

The first `bin/build-windows` downloads the MSVC CRT and Windows SDK via
`cargo-xwin` (accepting Microsoft's license). Wine also needs a 32-bit loader
(`lib32-glibc`, `lib32-gcc-libs` on Arch). Windows-only crates (`vfs-inject`,
`vfs-shim`, …) do not build for a Linux *host*, so a bare
`cargo build --workspace` on Linux fails; build the Linux crates by name.
(The `skyrim-live` harness lives in `vfs-bench` and is Windows-only; it just exits on Linux.)

### Tests

Run from `rust/`. Which crates build depends on the host: `vfs-inject`, `vfs-shim`,
`vfs-shim-dll` and `vfs-win` are Windows code and do not build for a Linux target, so
on Linux name the crates (CI's list is in `.github/workflows/ci.yml`):

```bash
cd rust
cargo test -p vfs-ipc -p vfs-protocol -p vfs-provider -p vfs-compose -p vfs-pe \
  -p vfs-zip -p vfs-core -p vfs-env -p vfs-unix -p vfs-proton \
  -p vfs-embed -p vfs-director -p vfs-block-store -p vfs-storage \
  -p vfs-registry -p vfs-redirect -p vfs-ntlayout
```

On Windows, `cargo test --workspace` builds everything. `vfs-embed`'s Windows
end-to-end tests (`escape_matrix`, `enumeration`, `launch_scenarios`, `staging`,
`profile_api`) inject real fixture processes and build the shim and fixtures they
need themselves.

**Proton end-to-end tests** (`vfs-embed`'s `proton_*` tests, `#[ignore]`d) follow the
policy in [Linux (Proton)](#linux-proton) above: a missing prerequisite prints
`SKIP <test>: <reason>` and passes, `VFS_TEST_REQUIRE_ALL=1` turns every skip into a
failure, and the Windows artefacts come from the test's own profile, so a release
test needs `bin/build-windows --release`.

**The shim's tests run under Wine.** `vfs-shim`'s tests install the real hooks, so they
are Windows executables. `bin/wine-shim-tests` cross-builds them with cargo-xwin and
runs each binary in a Wine prefix under `rust/target`, printing `PASS`/`FAIL` per binary:

```bash
bin/wine-shim-tests                 # build, then run every shim test binary
bin/wine-shim-tests seal registry   # only these binaries
bin/wine-shim-tests --no-build      # reuse the last build
```

It takes Wine from the newest installed GE-Proton runtime (`VFS_TEST_PROTON_RUNTIME`
names one), else a system `wine`; `WINE=/path/to/wine` overrides both. Do not run it
while a game is running under Wine. The same steps by hand:
`cargo xwin test --no-run --target x86_64-pc-windows-msvc -p vfs-shim`, then
`WINEPREFIX=$PWD/target/wine-prefix wine target/x86_64-pc-windows-msvc/debug/deps/<name>-*.exe`.

**clang-cl build workaround.** `libudis86-sys` (reached through `retour`, i.e. the
shim) calls `memset` without including `string.h`; `cl.exe` tolerates that and
clang-cl, which cargo-xwin uses, rejects it. Any direct `cargo xwin` build, test or
clippy of the shim needs
`CFLAGS_x86_64-pc-windows-msvc=-Wno-error=implicit-function-declaration` in the
environment (`bin/build-windows` and `bin/wine-shim-tests` set it for you). The name
is hyphenated, so in bash pass it through `env`:

```bash
env CFLAGS_x86_64-pc-windows-msvc=-Wno-error=implicit-function-declaration \
  cargo xwin clippy --target x86_64-pc-windows-msvc -p vfs-shim --all-targets
```

### Roots and launches

A root has an id and a **location**: where the program sees it, set with
`Session::declare_root(id, location)`; root 0 is the managed root. On Windows a
location is a host directory. On Linux it is a `C:\...` path inside the
session's Wine prefix: a symlink into `drive_c` that the session's first launch
creates (and the session removes when it goes down), so the prefix holds links,
not content. A location Wine cannot hold that way (another drive, a host path,
`C:\` itself, `..`) is refused; `Session::check_root_location` says so before a
launch does. Each root composes its own provider graph (`Session::mount_at`,
`Session::set_root_mounts`, `Session::set_write_layer_at`).

`LaunchOpts::image` takes the program in one of three forms: an absolute path
inside a root's location (resolved to that root, and staged to real disk if only
the composed graph serves it), an absolute path outside every root (launched as
given), or a path relative to root 0. A path containing `..` is refused in every
form. On Linux the session holds a detached program, and dropping the session
stops it (a Proton launch cannot outlive the session whose ring it reads
through).

A session named with `Session::set_prefix_name` keeps a persistent Wine prefix
at `$VFS_HOME/sessions/<name>/prefix`, so later launches reuse it; an unnamed
session's prefix is deleted when the session goes down. An existing real
directory at a root location is refused, never replaced — and so is a symlink
aether-vfs did not create (the ones it did are listed in the prefix's
`.aether-vfs-links`).

A host does all of this without environment variables:
`Session::set_home` chooses the aether home (runtimes and prefixes),
`Session::set_prefix_init(PrefixInit::Proton { steam_client, app_id })` sets a
prefix up with Proton's own `proton run` (DXVK, vkd3d-proton, the DirectX and
Visual C++ redistributables and Steam's bridge — what a game needs; the
default `wineboot` prefix is enough for console programs), and
`LaunchOpts::env` reaches only the child. `Session::launch_detached` returns a
`LaunchHandle` a host can poll and stop. `crates/vfs-embed/tests/proton_skyrim.rs`
is the whole shape end to end.

A Proton launch runs for as long as anything runs in the session's Wine
prefix, not just the program it started: a launcher that starts the game and
exits (`skse64_loader.exe` → `SkyrimSE.exe`) keeps the launch — its
`LaunchHandle`, a waited `launch`, and the prefix lock — alive until the game
exits (plus `wineserver`'s few seconds of persistence), and `stop_launch`
stops the game. The exit code reported is the launcher's. A handle kept from
`launch_detached` counts as the session's running launch: a second launch is
refused, and `stop_launch` or dropping the session stops it.

Wine's own output — its `err:`/`warn:` channels, and the "Unhandled
exception" report it prints when the game crashes — goes to the `wine`
child's stdout and stderr, which a launch inherits from the host. A host that
draws its own terminal UI loses it there; `LaunchOpts::log_file` sends both
streams of the `wine` child (and so of `wineserver`, the injector, the game
and anything it starts, which inherit them) to a file instead, created and
truncated at launch. A launch runs with `WINEDEBUG=-all` unless
`LaunchOpts::env` sets `WINEDEBUG`, so a host that wants crash reports in the
file sets both, e.g. `WINEDEBUG=err+all,warn+seh,fixme-all`. The internal
`wineserver -w` a launch uses to wait for the prefix to go quiet keeps its
output discarded.

**Upgrade notes (changes that affected embedders):**

- `vfs-injector`'s default ready timeout is now **180 s** (was 20 s), on
  Windows too; `VFS_READY_TIMEOUT_SECS` or `LaunchOpts::ready_timeout`
  overrides it.
- `vfs_inject::InjectError` has a new variant, `TargetExited(code)`: the
  target exited before the shim reported ready. An exhaustive `match` on it
  needs an arm.
- The shim caches small reads of files the director serves immutable
  (`VFS_SHIM_READ_CACHE=0` turns it off; see the architecture overview,
  §3.5). A provider says which handles qualify through
  `Provider::is_immutable(h)`, which defaults to its `immutable` capability;
  a composition that maps handles to children should forward it.
  `vfs_protocol::OpenResp` has two new fields (`immutable`, `mount_gen`), so
  a struct literal of it needs them or `..Default::default()`.
- `LaunchOpts` has new fields (`cwd`, `ready_timeout`, `log_file`), and
  `vfs_proton::launch::WineLaunch` has `log_file`; build `LaunchOpts` with
  `..Default::default()` so later additions do not break the build.

#### Storage: the source cache and named layers

A host that wants caching or persistent layers opens a `Storage` (`vfs-storage`,
re-exported by `vfs-embed`) on a directory: a deduplicating, compressing block
store that holds two things. A `Session` owns none; one process uses a storage
directory at a time.

- **A pull-through cache** for sources that declare themselves **immutable and
  slow**: `Storage::cached(provider, SourceKey)` wraps such a source and returns
  any other unchanged. The first read of a file fetches its blocks from the
  source, and later reads, in this session or any later one, are served from
  the store. Local `disk` and `zip` sources are never cached, because the OS
  page cache already serves them and a mod folder you edit must not go stale.
  A cached file is keyed by the source key, its path, its size and its mtime.
  A change on the source that alters the file's size or mtime gives it a new
  key, so the new version is fetched rather than served stale. The limitation:
  a file changed in place that keeps both its size and its mtime is served
  stale from the cache. The cache has a budget (`StorageConfig::cache_max_bytes`).
  Past it, the least recently used files are evicted until it is back under 90%.
- **Named layers**: persistent write layers. `Storage::layer(name)` is a
  provider a root can take as its write layer. The session's writes (saves,
  edited INIs, copied-up files) land in the store under that name and survive
  the session and the process. Layer data never counts against the cache
  budget and is never evicted. New files and namespace changes reach disk
  durably (fsynced) in batches, not on every file close: when the storage is
  closed, and otherwise at most every five minutes while the layer keeps
  changing. A crash can therefore undo up to the last few minutes of those
  changes, but the store always reopens consistent: a file created since the
  last durable point is gone whole rather than left half-written, and a
  removed or replaced file comes back. Rewriting an existing file in place is
  made durable when the program closes it. Replaced files keep their space
  until the next durable point.

## Embedding

**`vfs-embed` is the seam.** It owns one session — its roots, the provider graph
each root serves, the ring the injected shim talks over, and the launch — and it
is the *only* crate a host is meant to name. Everything above it is a host
(a launcher, a language binding, the `skyrim-live` harness); everything
below it is the engine (the director kernel, the provider contract, the
composition primitives). If a host has to reach past it, the fix belongs in
`vfs-embed` rather than in the host, and that is enforced by tests on both sides
of the line.

A host composes its graph from code — `layered`, `overlay`, `router`, `cached`,
`readonly`, `seekable`, `disk`, `memory` — and writes only its own data source.
Config is a serialization of that graph, not the other way round.

```rust
use std::sync::Arc;
use vfs_embed::{DiskProvider, LaunchOpts, Session};

let mut session = Session::new();
session.set_root(r"C:\vfs\root");                 // an empty directory
session.mount("", Arc::new(DiskProvider::new(r"C:\content")))?;
session.serve()?;
session.launch(&LaunchOpts { image: "MyGame.exe".into(), ..Default::default() })?;
println!("{:?}", session.rejected_writes());
```

The gRPC daemon, the `vfs` CLI and the out-of-process source plugins
(`vfs-directord`, `vfs-control`, `vfs-source`) were removed on 2026-10-08: no
host used them.

## Architecture (short)

| Piece | Crate |
|-------|--------|
| **The seam.** Embeddable API: session lifecycle, roots, composition, launch | `vfs-embed` |
| Provider contract, capabilities, conformance suite | `vfs-provider` |
| Layered / router / overlay (copy-up writes) | `vfs-compose` |
| Storage: pull-through cache for slow sources, named persistent write layers | `vfs-storage` |
| Deduplicating, compressing block store (under `vfs-storage`) | `vfs-block-store` |
| Director kernel + ring server + staging | `vfs-director` |
| GE-Proton install, prefix and Wine launch (Linux host) | `vfs-proton` |
| Registry overlay model | `vfs-registry` |
| Inject / shim | `vfs-inject`, `vfs-shim` |

Docs: [rust/docs/](rust/docs/) (start at the
[architecture overview](rust/docs/architecture.md), which has the full crate map);
design docs are indexed in [docs/superpowers/README.md](docs/superpowers/README.md).

## Packaging

A host ships its own binary with the natives beside it:

```powershell
cd rust
cargo build --release -p vfs-shim-dll
# target/release/vfs_shim_dll.dll
```

`Session::launch` looks for the shim beside the host's executable unless
`LaunchOpts::shim_dll` names it (on Linux, `bin/build-windows --release` builds
the shim and `vfs-injector.exe`, and `LaunchOpts::injector` can name the latter).

## License

GPL-3.0-only. See [LICENSE](LICENSE).

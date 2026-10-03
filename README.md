# aether-vfs

Userspace virtual filesystem for **Windows game modding**: compose base game +
mods from pluggable sources (disk, zip, remote gRPC plugins), inject a thin
NT-API shim into the game, and serve remapped I/O from a long-lived **Rust
director daemon**.

The control plane is **gRPC** (any language). The data plane is the existing
shared-memory ring + inject/payload/shim stack. It is also **embeddable**: a host
program composes a session in code against `vfs-embed` instead of talking to the
daemon — see [Embedding](#embedding) below.

> Pure Rust. The former Clojure/JVM layer has been removed (M4).

## Documentation

| Document | For |
|---|---|
| [Architectural overview](rust/docs/architecture.md) | Engineers: how the system fits together, and how the hard parts are solved |
| [Product overview](docs/product-overview.md) | Non-technical: what it does and why it matters |
| [Benchmarks](rust/docs/benchmarks/) | Measurements and the analysis behind them |
| [Code audit](rust/docs/audit-2026-08-13.md) | Full-tree review: findings, what was fixed, what was not |
| [vfs-summary.md](rust/docs/vfs-summary.md) | Earlier long-form technical narrative |

## Quick start

```powershell
cd rust
cargo build -p vfs-directord -p vfs-shim-dll -p vfs-fixture-read
cargo build --manifest-path crates/vfs-payload/Cargo.toml --target-dir target   # separate workspace
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
```

The first `bin/build-windows` downloads the MSVC CRT and Windows SDK via
`cargo-xwin` (accepting Microsoft's license). Wine also needs a 32-bit loader
(`lib32-glibc`, `lib32-gcc-libs` on Arch). Windows-only crates (`vfs-inject`,
`vfs-shim`, …) do not build for a Linux *host*, so a bare
`cargo build --workspace` on Linux fails; build the Linux crates by name.
`vfs-directord` — the `vfs` CLI and daemon — is one of them and builds on
Linux (its `skyrim-live` harness is Windows-only and just exits there).

### Daemon + CLI (`vfs`)

```powershell
# Foreground daemon (clients also auto-spawn when needed)
.\target\debug\vfs.exe daemon

# Health / stats
.\target\debug\vfs.exe health
.\target\debug\vfs.exe stats

# Config-driven session
.\target\debug\vfs.exe up --config scenario.toml

# Flag-driven (precedence is declaration order — the second wins on a shared path)
.\target\debug\vfs.exe launch `
  --source disk:C:\content@/ `
  --source zip:C:\GameLayers\base.zip@/ `
  --write-layer C:\content\overwrite `
  --exec C:\path\to\tool.exe --env KEY=VAL
```

Example `scenario.toml`:

```toml
[session]
name = "demo"

[[source]]
type  = "disk"
path  = "C:/content"
mount = "/"

# Where the session's writes go. A write to content a read-only source holds
# (an archive) is copied up into this directory instead of being refused;
# without one, every source is content and the root is effectively read-only.
[[source]]
type        = "disk"
path        = "C:/content/overwrite"
write_layer = true

[launch]
exec      = "C:/tools/my-probe.exe"
wait      = true
```

#### Root locations and `vfs exec` (Linux under Proton, or Windows)

A `[[root]]` gives a root an `id`, a `name` and a `path`: the location the
program sees. On Linux that is a `C:\...` path inside the session's Wine prefix:
a symlink into `drive_c` that the session's first launch creates (and the
session removes when it goes down), so the prefix holds links, not content. A
location Wine cannot hold that way (another drive, a host path, `C:\` itself,
`..`) is refused by `vfs up`. Root 0 may be declared like any other. Sources
attach to a root with `root = <id>`.

```toml
[session]
name = "demo"

[[root]]
id = 0
name = "Games"
path = 'C:\Games\Fixture'

[[root]]
id = 1
name = "Saves"
path = 'C:\users\steamuser\saves'

[[source]]
type = "disk"
path = "/home/me/game"
root = 0

[[source]]
type = "zip"
path = "/home/me/data.zip"
root = 0

[[source]]
type = "disk"
path = "/home/me/saves-layer"
root = 1
write_layer = true
```

Bring the session up (no `[launch]`, so it stays running), launch into it as
often as you like, then take it down:

```sh
vfs up --config demo.toml
vfs exec --session demo '{Games}\game.exe' --env KEY=VAL -- --some-arg
vfs down --session demo
```

`vfs exec` takes the program in one of three forms: `{RootName}\rel` (a path
under a named root), an absolute path (resolved to the root that contains it,
and staged to real disk if only the composed graph serves it), or a path
relative to root 0. A path containing `..` is refused in every form — on
Windows too, where an absolute image with `..` used to be launched as given.
`--no-wait` returns once the program has started. On Linux the session holds
the running program, and `vfs down` stops it (a Proton launch cannot outlive
the session whose ring it reads through).

One live session per name: a second `vfs up` of the same config is refused
until the first is down, and a config that fails half-way leaves no session
behind. `vfs down` is refused while a waited launch in that session is still
running; a detached one is stopped first instead. The one-shot `vfs launch`
takes its session down once a waited launch returns, and a daemon stopped
with Ctrl-C / SIGTERM takes every session down before it exits.

A named session keeps a persistent Wine prefix at
`$VFS_HOME/sessions/<name>/prefix`, so later `vfs exec` calls reuse it; an
unnamed session's prefix is deleted when the session goes down. An existing
real directory at a root location is refused, never replaced — and so is a
symlink aether-vfs did not create (the ones it did are listed in the prefix's
`.aether-vfs-links`).

An embedding host can do all of this without environment variables:
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

**Upgrade notes (this branch):**

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

The daemon keeps one storage directory, a deduplicating, compressing block
store (`vfs-storage`), which holds two things:

- **A pull-through cache** for sources that declare themselves **immutable and
  slow**. Today that means a remote (gRPC) source that declares both; an HTTP
  source will, once it exists. The first read of a file fetches its blocks
  from the source, and later reads, in this session or any later one, are
  served from the store. Local `disk` and `zip` sources are never cached,
  because the OS page cache already serves them and a mod folder you edit must
  not go stale. The reference `vfs-source-plugin` serves a disk directory and
  declares it mutable, so it is not cached either. A cached file is keyed by
  the source (its remote endpoint, or `cache_key = "..."` on its `[[source]]`
  to pin one), its path, its size and its mtime. A change on the source that
  alters the file's size or mtime gives it a new key, so the new version is
  fetched rather than served stale. The limitation: a file changed in place
  that keeps both its size and its mtime is served stale from the cache. The
  cache has a budget, `--cache-max-gib` (default
  32). Past it, the least recently used files are evicted until it is back
  under 90%.
- **Named layers**: persistent write layers. A root's write layer can be a
  layer instead of a directory. The session's writes (saves, edited INIs,
  copied-up files) land in the store under that name and survive `vfs down`
  and a daemon restart. Layer data never counts against the cache budget and
  is never evicted. New files and namespace changes reach disk durably
  (fsynced) in batches, not on every file close: when the session ends, when
  the daemon stops, and otherwise at most every five minutes while the layer
  keeps changing. A crash (a killed daemon as much as a power loss) can
  therefore undo up to the last few minutes of those changes, but the store
  always reopens consistent: a file created since the last durable point is
  gone whole rather than left half-written, and a removed or replaced file
  comes back. Rewriting an existing file in place is made durable when the
  program closes it. Replaced files keep their space until the next durable
  point.

```toml
[[source]]
type = "layer"
name = "skyrim-profile-a"   # created on first use
root = 1
write_layer = true          # a layer source is always the write layer
```

With flags: `vfs launch ... --write-layer layer:skyrim-profile-a`
(`--write-layer DIR` still means a disk directory).

Layers are managed through the daemon, which holds the store:

```sh
vfs layer list                          # NAME, file count, bytes
vfs layer export skyrim-profile-a DIR   # write it out as plain files (DIR empty or absent)
vfs layer import DIR new-profile        # create a layer from a tree (NAME must not exist)
vfs layer delete skyrim-profile-a       # refused while a live session writes into it
vfs stats                               # second line: layers, pack/live bytes, cached bytes
```

**Where it lives:** `--storage-dir DIR` on `vfs daemon`, else
`$VFS_STORAGE_DIR`, else `$VFS_HOME/storage`. `VFS_HOME` defaults to
`$XDG_DATA_HOME/aether-vfs` or `~/.local/share/aether-vfs` on Linux, and
`%LOCALAPPDATA%\aether-vfs` on Windows. Only one daemon can use a storage
directory at a time. A second daemon pointed at the same directory refuses to
start, and says so. A daemon that a `vfs` command auto-spawns takes no flags,
so choose its directory with `VFS_STORAGE_DIR` in that command's environment.
If an auto-spawned daemon cannot start, the command reports why, and the
daemon's stderr is in `<discovery file>.daemon.log`.

## Embedding

**`vfs-embed` is the seam.** It owns one session — its roots, the provider graph
each root serves, the ring the injected shim talks over, and the launch — and it
is the *only* crate a host is meant to name. Everything above it is a host
(`vfs.exe` and its daemon, and any language binding after it); everything
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

### Out-of-process source plugin

```powershell
cargo run -p vfs-source --bin vfs-source-plugin -- --root C:\data --bind 127.0.0.1:0
# prints endpoint=127.0.0.1:PORT — pass as remote source
```

Any language can implement `vfs-source/proto/source.proto` (`Source` service).

## Architecture (short)

| Piece | Crate |
|-------|--------|
| Control gRPC + config schema | `vfs-control` |
| Daemon + `vfs` CLI | `vfs-directord` |
| **The seam.** Embeddable API: session lifecycle, roots, composition, launch | `vfs-embed` |
| Provider contract, capabilities, conformance suite | `vfs-provider` |
| Provider builders, gRPC SourceService | `vfs-source` |
| Layered / router / overlay (read) | `vfs-compose` |
| Storage: pull-through cache for slow sources, named persistent write layers | `vfs-storage` |
| Deduplicating, compressing block store (under `vfs-storage`) | `vfs-block-store` |
| Director kernel + ring server + staging | `vfs-director` |
| Inject / shim / payload | `vfs-inject`, `vfs-shim`, `vfs-payload` |

Docs: [rust/docs/](rust/docs/), design
[docs/superpowers/specs/2026-08-11-director-daemon-rework-design.md](docs/superpowers/specs/2026-08-11-director-daemon-rework-design.md).

## Packaging

Release build of the daemon and natives:

```powershell
cd rust
cargo build --release -p vfs-directord -p vfs-shim-dll -p vfs-source
cargo build --release --manifest-path crates/vfs-payload/Cargo.toml --target-dir target   # separate workspace
# Artifacts under target/release/:
#   vfs.exe, vfs_shim_dll.dll, vfs_payload.dll, vfs-source-plugin.exe
```

Ship those four next to each other (the daemon locates the DLLs beside the
`vfs` binary when launching children).

## License

GPL-3.0-only. See [LICENSE](LICENSE).

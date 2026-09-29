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
On Windows `--no-wait` returns once the program has started; on Linux it is
refused, because a Proton launch always waits for the program to exit.

One live session per name: a second `vfs up` of the same config is refused
until the first is down, and a config that fails half-way leaves no session
behind. `vfs down` is refused while a launch in that session is still running.
The one-shot `vfs launch` takes its session down once a waited launch returns,
and a daemon stopped with Ctrl-C / SIGTERM takes every session down before it
exits.

A named session keeps a persistent Wine prefix at
`$VFS_HOME/sessions/<name>/prefix`, so later `vfs exec` calls reuse it; an
unnamed session's prefix is deleted when the session goes down. An existing
real directory at a root location is refused, never replaced — and so is a
symlink aether-vfs did not create (the ones it did are listed in the prefix's
`.aether-vfs-links`).

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

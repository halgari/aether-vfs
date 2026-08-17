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

## Embedding

**`vfs-embed` is the seam.** It owns one session — its roots, the provider graph
each root serves, the ring the injected shim talks over, and the launch — and it
is the *only* crate a host is meant to name. Everything above it is a host
(`vfs.exe` and its daemon, the Node addon, a Python binding after it); everything
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

### The Node addon (`aethervfs`)

`rust/crates/vfs-node` is an N-API addon over `vfs-embed`, on the same footing as
the daemon: it composes graphs, launches injected processes, and lets **a plain
JavaScript object be a first-class provider** — held to the workspace's own Rust
conformance suite via `assertConformance()`, not to a reimplementation of it.

```powershell
cd rust\crates\vfs-node
npm run build      # four cargo builds + four copies; no npm dependencies
npm test           # examples, JS suites, and the .d.ts drift check
```

```js
const { Session, disk, layered, readonly, providerWorker } = require('aethervfs');

const s = new Session('demo');
s.addRoot(0, 'game', s.virtualRoot);
await using cdn = await providerWorker({ module: require.resolve('./my-cdn.cjs') });
s.mount(0, layered(readonly(cdn.provider), disk(modsDir)));
s.launch('MyGame.exe');
s.close();
```

A provider is serviced by the event loop that registered it, and a blocking call
issued *on* that loop can never settle — so `providerWorker()` is the recommended
shape, and the alternative is refused with an explanation rather than hanging.
`index.d.ts` carries the full API.

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
| Node addon (`aethervfs`): a JS object as a provider | `vfs-node` |
| Provider contract, capabilities, conformance suite | `vfs-provider` |
| Provider builders, gRPC SourceService | `vfs-source` |
| Layered / router / overlay (read) | `vfs-compose` |
| Block cache (RAM + disk) | `vfs-cache` |
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

Private / unlicensed for external use unless otherwise stated.

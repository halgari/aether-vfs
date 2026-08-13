# aether-vfs

Userspace virtual filesystem for **Windows game modding**: compose base game +
mods from pluggable sources (disk, zip, remote gRPC plugins), inject a thin
NT-API shim into the game, and serve remapped I/O from a long-lived **Rust
director daemon**.

The control plane is **gRPC** (any language). The data plane is the existing
shared-memory ring + inject/payload/shim stack.

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
cargo build --manifest-path crates/vfs-payload/Cargo.toml   # separate workspace
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

# Flag-driven
.\target\debug\vfs.exe launch `
  --source disk:C:\content@/#0 `
  --source zip:C:\GameLayers\base.zip@/#10 `
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
layer = 0

[launch]
exec      = "C:/tools/my-probe.exe"
wait      = true
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
| Source builders, SourceService, conformance | `vfs-source` |
| Layered / router / overlay (read) | `vfs-compose` |
| Block cache (RAM + disk) | `vfs-cache` |
| FUSE kernel + Session launch | `vfs-director` |
| Inject / shim / payload | `vfs-inject`, `vfs-shim`, `vfs-payload` |

Docs: [rust/docs/](rust/docs/), design
[docs/superpowers/specs/2026-08-11-director-daemon-rework-design.md](docs/superpowers/specs/2026-08-11-director-daemon-rework-design.md).

## Packaging

Release build of the daemon and natives:

```powershell
cd rust
cargo build --release -p vfs-directord -p vfs-shim-dll -p vfs-source
cargo build --release --manifest-path crates/vfs-payload/Cargo.toml   # separate workspace
# Artifacts under target/release/:
#   vfs.exe, vfs_shim_dll.dll, vfs_payload.dll, vfs-source-plugin.exe
```

Ship those four next to each other (the daemon locates the DLLs beside the
`vfs` binary when launching children).

## License

Private / unlicensed for external use unless otherwise stated.

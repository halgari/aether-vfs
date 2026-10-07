# VFS (Rust workspace)

Userspace virtual filesystem for Windows game modding: serve base game + mods **from Stored ZIP
archives** (and disk folders, and remote sources) without extracting PE/BSA/ESP content, stage
the launch PE closure, and inject a thin NT-API shim into the game. The game runs on Windows, or
on Linux under GE-Proton.

The top-level [README](../README.md) is the main guide (build, Linux/Proton, tests, the `vfs`
CLI, storage, embedding). This file is the short map of the workspace.

## Docs

| Document | Description |
|----------|-------------|
| **[docs/architecture.md](docs/architecture.md)** | How the system fits together; the crate map |
| [docs/durability.md](docs/durability.md) | What is durable when (named layers, registry overlay) |
| [../docs/shim-invariants.md](../docs/shim-invariants.md) | The rules the NT hooks keep, and the incidents behind them |
| [../docs/superpowers/README.md](../docs/superpowers/README.md) | Index of design specs and plans (current and archived) |
| [docs/benchmarks/](docs/benchmarks/) | Measurements |
| [docs/overview.md](docs/overview.md), [docs/vfs-summary.md](docs/vfs-summary.md) | Earlier narratives (historical) |

## Embedding

A host names one crate, `vfs-embed`, and builds a `Session`:

```rust
use std::{path::Path, sync::Arc};
use vfs_embed::{DiskProvider, LaunchOpts, Session, ZipProvider};

fn run() -> Result<(), String> {
    let st = |status: i32| format!("status {status}");   // mount calls return a status code
    let mut session = Session::new();
    session.set_root(r"C:\GameLayers\runtime");          // an empty directory
    let base = ZipProvider::open(Path::new(r"C:\GameLayers\1. Skyrim Special Edition.zip"))
        .map_err(|e| e.to_string())?;
    session.mount("", Arc::new(base)).map_err(st)?;
    session.mount("", Arc::new(DiskProvider::new(r"C:\GameLayers\mods"))).map_err(st)?; // later mounts win
    // Writes to content the layers hold copy up into this directory.
    session.set_write_layer(Arc::new(DiskProvider::new(r"C:\GameLayers\overwrite"))).map_err(st)?;
    session.serve()?;
    session.launch(&LaunchOpts {
        image: "skse64_loader.exe".into(),
        wait: true,
        ..Default::default()
    })?;
    Ok(())
}
```

See the top-level README's *Embedding* section and `vfs-embed`'s crate docs for the full API,
including the Proton (Linux) options.

Control plane is gRPC-only. Prefer the `vfs` CLI / `vfs-directord` daemon:

```sh
cargo run -p vfs-directord -- daemon
cargo run -p vfs-directord -- up --config scenario.toml
```

## Build and test

```sh
cargo build -p vfs-directord -p vfs-shim-dll -p vfs-fixture-read    # Windows
cargo build --manifest-path crates/vfs-payload/Cargo.toml --target-dir target   # separate workspace
```

On Linux the Windows half is cross-built with `../bin/build-windows`; the shim's own tests run
under Wine with `../bin/wine-shim-tests`; the Proton end-to-end tests are `#[ignore]`d and skip
(or, with `VFS_TEST_REQUIRE_ALL=1`, fail) when a prerequisite is missing. The commands, the
test policy and the clang-cl workaround
(`CFLAGS_x86_64-pc-windows-msvc=-Wno-error=implicit-function-declaration`) are in the
[top-level README](../README.md#tests).

## Workspace

Rust 2021 Cargo workspace, `panic = "unwind"` throughout. The `no_std` early payload
(`vfs-payload`) needs `panic = "abort"`, so it is excluded from the workspace and carries its own
profile; `vfs-protocol/tests/unwind.rs` pins both.

The workspace members are listed in [`Cargo.toml`](Cargo.toml) and described one by one in the
crate map of [docs/architecture.md](docs/architecture.md#8-crate-map). The ones a reader meets
first:

| Crate | Role |
|-------|------|
| `vfs-embed` | The embeddable API: `Session`, roots, composition, serve, launch |
| `vfs-directord` | Daemon + `vfs` CLI |
| `vfs-director` | Kernel: root to provider table, ring server, staging |
| `vfs-compose`, `vfs-zip`, `vfs-source` | Providers and composition (overlay copy-up, layered, router, zip, remote) |
| `vfs-storage`, `vfs-block-store` | Pull-through cache and named write layers on a deduplicating block store |
| `vfs-proton` | GE-Proton install, prefix and Wine launch (Linux host) |
| `vfs-shim`, `vfs-inject`, `vfs-payload` | The Windows half: NT detours, injection, early payload |

## License

GPL-3.0-only. See [LICENSE](LICENSE).

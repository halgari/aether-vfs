# VFS

Userspace virtual filesystem for Windows game modding: serve base game + mods **from Stored ZIP archives** without extracting PE/BSA/ESP content, inject a thin NT-API shim into the game, and hollow a real Steam host image so CreateProcess and DRM still work.

## Docs

| Document | Description |
|----------|-------------|
| **[docs/overview.md](docs/overview.md)** | Short product/architecture overview |
| **[docs/vfs-summary.md](docs/vfs-summary.md)** | Full technical summary (whitepaper-oriented, multi-page) |
| [docs/superpowers/specs/](docs/superpowers/specs/) | Feature design specs |
| [docs/benchmarks/](docs/benchmarks/) | FUSE RPC benchmarks |

## Quick start (Skyrim SE + SKSE + SkyUI layout)

Expected layout under `C:\GameLayers` (or pass `--layers`):

```text
1. Skyrim Special Edition.zip
2. SKSE 2.2.6.zip
3. SkyUI 6.11.zip
runtime\     # managed virtual root (empty of payload files)
overlay\
vfs-state\
```

```powershell
cargo build -p vfs-shim-dll -p vfs-payload -p vfs-launch --release
cargo run -p vfs-launch --release
# optional:
cargo run -p vfs-launch --release -- --probe   # VFS reads only
cargo run -p vfs-launch --release -- --wait    # wait for game exit
```

## Host API sketch

```rust
use vfs_director::{LaunchOpts, Session};

let mut s = Session::new();
s.set_root(r"C:\GameLayers\runtime");
s.set_overlay(r"C:\GameLayers\overlay");
s.set_state_dir(r"C:\GameLayers\vfs-state");
s.mount_zip(r"C:\GameLayers\1. Skyrim Special Edition.zip")?;
// … more layers …
s.serve()?;
s.launch(&LaunchOpts {
    image: "skse64_loader.exe".into(),
    wait: true,
    hollow_pe: true,
    ..Default::default()
})?;
```

C headers: `crates/vfs-director/include/vfs.h` (`vfs_director_*`, `vfs_launch`).

## Workspace

Rust 2021 Cargo workspace. `panic = "abort"` is workspace-wide for the `no_std` early payload.

Private development: `github.com/halgari/vfs`.

## License

Private / unlicensed for external use unless otherwise stated.

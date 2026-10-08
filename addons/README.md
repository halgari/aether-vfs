# aether-vfs add-ons

Optional crates on top of the core (`../rust`). A host takes the ones it needs;
the core never depends on an add-on. This is its own Cargo workspace, with its
own CI job.

| Crate | What it is |
|---|---|
| `aether-block-store` | Deduplicating, compressing block store (redb index + zstd packs) |
| `aether-storage` | The block store as pull-through cache and named write layers |
| `aether-archive` | Random-access archive formats (seekable zstd, zip directories, 7z/zip extraction), `Xxh64`, `RangeRead` |
| `aether-net` | Shared HTTP base: connection limits, retries, events, ranged and bulk downloads |
| `aether-steam` | Steam login, depot manifests and random-access depot reads |
| `aether-nexus` | Nexus Mods API and random access into repacked zips |
| `aether-wj-cdn` | The Wabbajack CDN |
| `aether-gog` | GOG Galaxy login, depot manifests and random-access depot reads |

The downloaders are libraries first. With the `provider` feature, the depot and
archive crates also expose a `vfs_provider::Provider`, so a host can mount their
content as a layer. See `docs/superpowers/specs/2026-10-08-addons-design.md`.

# Add-ons workspace — Design Spec

**Status:** Approved design.
**Date:** 2026-10-08
**Consumers:** Haskill (Wabbajack launcher), the Vortex Node binding, any other host of `vfs-embed`.

---

## 1. Goal

Make aether-vfs modular. The core is the VFS engine and its embedding API. Everything a host may or
may not want — persistent storage, game-store and mod-site downloaders, VFS sources built on them —
lives in a separate **add-ons workspace** in this repo. The core never depends on an add-on.

Three moves get us there:

1. **Delete the daemon stack** (`vfs-directord`, `vfs-control`, `vfs-source`). Nothing uses it:
   Haskill and the Vortex binding both host `vfs-embed` directly.
2. **Move storage out of core** (`vfs-block-store`, `vfs-storage`). Vortex does not use it.
3. **Move Haskill's downloaders in** (Steam, Nexus Mods, the Wabbajack CDN) and add **GOG**.

Each downloader is a plain library first; a thin `vfs_provider::Provider` adapter sits on top behind
a cargo feature, so a host can either call the library or mount its content as a layer.

## 2. Layout

```
aether-vfs/
  rust/                  # core workspace (unchanged location)
  bindings/node/         # Vortex binding (own project, per 2026-10-07 spec)
  addons/
    Cargo.toml           # own workspace: edition 2024, GPL-3.0-only, rust-version 1.96
    crates/
      aether-block-store/  # was rust/crates/vfs-block-store
      aether-storage/      # was rust/crates/vfs-storage (+ registry_sync_for)
      aether-net/          # shared HTTP base, from haskill-sources
      aether-archive/      # random-access archive formats, from haskill-formats + haskill-sources
      aether-steam/        # was haskill-steam
      aether-nexus/        # was haskill-sources::nexus
      aether-wj-cdn/       # was haskill-sources::cdn
      aether-gog/          # new
```

Add-ons depend on core crates by path (`../rust/crates/...`). Core CI is untouched by add-ons; the
add-ons get their own CI job (`cargo test --workspace`, clippy, fmt in `addons/`). Network-touching
tests stay `#[ignore]`, as they are in Haskill today.

Naming: `aether-*` for add-ons, `vfs-*` for core, so a crate's name says which side it is on.

## 3. Core changes

### 3.1 Daemon removal

- Before deleting, port the daemon e2e tests that cover **core** behaviour to `vfs-embed` tests:
  `escape_matrix`, `enumeration`, `staging`, `launch_scenarios`, `copy_on_write_daemon` (where not
  already covered by `copy_on_write_composition`), `composition`. Tests of daemon-only surface
  (`daemon_spawn`, `layer_rpc`, `profile_api`, `session_config`, `proton_cli`,
  `storage_pull_through`, `shim_report_parsing` if daemon-only) are dropped; their core assertions,
  if any, are ported.
- Delete `vfs-directord`, `vfs-control`, `vfs-source` and their proto files. `SourceSpec` and any
  other type a remaining crate still needs moves to that crate.
- Update `.github/workflows/ci.yml`, `README.md` (no more "gRPC control plane, any language" or `vfs`
  CLI), `rust/docs/architecture.md` (§3.8 and the crate table).
- `vfs-bench`'s `skyrim-live` and any bench that drove the daemon either moves to `vfs-embed` or is
  removed.

### 3.2 Storage removal

- `git mv` `vfs-block-store` → `addons/crates/aether-block-store` and `vfs-storage` →
  `addons/crates/aether-storage` (package and lib names renamed; history kept).
- `vfs-embed` drops its `vfs-storage` dependency and the `Storage`, `StorageConfig`, … re-exports.
  `registry_sync_for` moves to `aether-storage` (it only needs `vfs_embed::RegistrySync`, which stays).
  Doc comments in `vfs-embed` that mention `Storage::cached` are reworded generically ("a caching
  wrapper such as `aether-storage`'s").
- Core tests that use storage (`vfs-embed`'s `proton_launch` storage case, `registry.rs`
  `registry_layer_tests`, `vfs-bench`'s ring-bench "names" case) move to `aether-storage` tests or
  are reworked to use a plain disk/memory layer when storage is not the thing under test.
- CI: the storage/block-store steps (`test-hooks`, `crash-points`, model test, bench `--no-run`)
  move to the add-ons job. `rust/docs/durability.md` moves with storage or is updated to point there.

## 4. Add-on crates

### 4.1 `aether-net`

From `haskill-sources`: `Http`, `HttpConfig`, `Limiter`, `Permit`, `RetryPolicy`, `Events`,
`JobId`, `SourceEvent`, `BulkHttp`, `BulkHttpConfig`, `RangeBody`, `HttpFile`, `Downloaded`,
`BlobSink`, `MemorySink`, and the one shared error type `SourceError` (kept whole so Haskill's
matches on `SourceError::Status`, `ArchiveChanged`, … keep working; Nexus and CDN code return it).
Host-specific wording leaves it: `NexusUnauthorized` no longer names `haskill login nexus`, and the
`Bethesda { status, code, msg }` variant becomes `Refused { service: &'static str, status, code,
msg }`. Depends on `aether-archive` for `Xxh64` and `FormatError`.

### 4.2 `aether-archive`

No network dependencies. Owns the two neutral types everything else shares:

- `Xxh64(pub u64)` — Wabbajack's `Hash`, same `Display`/`FromStr` and `of(&[u8])`.
  `haskill-wabbajack` does `pub use aether_archive::Xxh64 as Hash;` so Haskill code is unchanged.
- `RangeRead` — the trait from `haskill_formats::range`, with `read_vec` / `RangeCursor`.

From `haskill-formats`: `range`, `seekable` (zstd seekable format), `zip` (central directory,
ZIP64), `path::fold`, and the error type. From `haskill-sources`: `extract` (`ArchiveKind`,
`extract`, `extract_threads`, `sniff`).

`haskill-formats` keeps `bsa` and `octodiff` and depends on `aether-archive` for `RangeRead`,
`path::fold` and its shared error helpers.

Overlap with core `vfs-zip` (Stored-only `ZipProvider`) is noted, not merged in this work: the two
readers serve different needs (vfs-zip serves stored entries as a provider; aether-archive parses
Nexus repacks with seekable-zstd members). A later change may unify them.

### 4.3 `aether-steam`

`haskill-steam` minus its Haskill knowledge:

- `SteamGame::open(content, &GameDb, game, version)` and `app_for_game` (Wabbajack game names →
  app ids) move to Haskill (`haskill-store` or a small module next to its Steam use).
  `SteamGame::from_depots` stays.
- `haskill_wabbajack::Hash` → `aether_archive::Xxh64`; `haskill_formats::RangeRead` →
  `aether_archive::RangeRead`.
- Public API otherwise unchanged (it already hides `steamroom`).

### 4.4 `aether-nexus`, `aether-wj-cdn`

`haskill-sources::nexus` (API client, signed links, `NexusArchive`, `SpanLease`) and
`haskill-sources::cdn` (`CdnDefinition`, `CdnFile`, `CdnPart`, `remap_cdn_url`) as their own
crates on `aether-net` + `aether-archive`. The Nexus quota rules in Haskill's memory (links on
demand, never sign ahead, respect the hourly quota) are crate behaviour and are kept.

### 4.5 What stays in Haskill

`haskill-wabbajack`, `haskill-gamedb`, `haskill-resolve`, `haskill-store`, `haskill-vfs`,
`haskill-formats` (bsa, octodiff), texture/GPU/DDS crates, the CLI. `haskill-sources` shrinks to
`bethesda/` (CKM / Creation Club) plus re-exports of `aether-net`, `aether-wj-cdn` and
`aether-nexus` (as `nexus`), so Haskill's `haskill_sources::…` paths keep working. Likewise
`haskill-formats` re-exports `aether_archive`'s `range`, `seekable`, `zip`, `path` modules and
`FormatError`. `haskill-steam` is deleted and its users import `aether_steam`. Haskill depends on add-ons by path through the
`external/aether-vfs` submodule, replacing its `vfs-storage` workspace dep with `aether-storage`.

### 4.6 `aether-gog`

Port of NexusMods.App's `NexusMods.Networking.GOG` (GPL-3.0; attributed in the crate README),
Galaxy content-system v2 only:

- **Login.** `GogLogin` prints `https://auth.gog.com/auth?client_id=…&redirect_uri=…&response_type=code&layout=client2`,
  reads back the pasted redirect URL or bare `code`, exchanges it at `auth.gog.com/token`, stores
  access + refresh token in a credential file (same file-permission rules as Steam's), refreshes on
  expiry.
- **`GogContent`.** `builds(product, os)` (`content-system.gog.com/products/{id}/os/{os}/builds?generation=2`),
  `build_details(build)` (zlib JSON: depots, some from other product ids), `depot(manifest)`
  (`cdn.gog.com/content-system/v2/meta/ab/cd/{id}`, zlib JSON: files → chunk lists with
  compressed/uncompressed MD5 and sizes). Manifests cached on disk.
- **`GogDepotFile`.** Random access: map an offset range to chunks, fetch each chunk from the
  secure link (`…/secure_link?generation=2&_version=2&path=/` → CDN URL + `/ab/cd/{md5}`), check
  the compressed MD5, inflate, serve. In-memory chunk LRU. Uses `aether-net` for limits, retries
  and events. Same shape as `SteamDepotFile`.
- **Out of scope:** installer archives (makeself / mojosetup `.sh`, Windows installers), v1
  content system, a GOG CLI in Haskill.

### 4.7 Provider adapters (feature `provider`)

- `aether_steam::provider::DepotProvider` / `aether_gog::provider::DepotProvider`: one or more
  depot manifests as a read-only tree; earlier depots win on path collisions; directory listing from
  the manifests, reads from the depot file readers. Paths case-folded as the manifests require.
- `aether_nexus::provider::ArchiveProvider`: one repacked Nexus zip as a read-only tree.
- No adapter for `aether-wj-cdn` (whole archives, nothing to mount until extracted).
- Each adapter passes `vfs-provider`'s conformance suite against **offline fixtures** (a fake
  CDN/HTTP server or an injected chunk source), never the network. Persistence is the host's
  choice: wrap in `aether_storage::Storage::cached`.

## 5. Order and parallelism

1. Skeleton: `addons/Cargo.toml` workspace, CI job, README.
2. In parallel, three streams:
   - **A — core daemon removal** (§3.1).
   - **B — storage move** (§3.2).
   - **C — downloader move** (§4.1–4.4).
   A and B both edit `rust/Cargo.toml`, `ci.yml`, `vfs-embed` and `vfs-bench`; conflicts are
   resolved at merge.
3. After C: **D — GOG** (§4.6) and **E — Haskill repoint** (§4.5) in parallel. E also needs B.
4. After D and C: **F — provider adapters** (§4.7).

## 6. Done means

- `cargo test --workspace` and clippy `-D warnings` pass in `rust/` and in `addons/`.
- No crate in `rust/` depends on anything in `addons/`.
- Haskill: `cargo test --workspace` and clippy pass against the add-ons; the moved crates are gone
  from `crates/`.
- `vfs-embed`'s Proton e2e tests (`proton_launch`, `proton_registry`) still pass on Linux.
- Every adapter passes conformance offline.
- Ignored network tests for GOG exist (login refresh, build list, one file read) for manual runs.

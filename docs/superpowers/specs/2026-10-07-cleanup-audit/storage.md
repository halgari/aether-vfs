# aether-vfs cleanup audit: storage, composition, providers, core types

Repo: `external/aether-vfs` @ `fcdf874`, Rust workspace under `rust/`. All paths below are relative to `rust/crates/` unless they start with `rust/` or `docs/`.
Scope: vfs-storage, vfs-block-store, vfs-compose, vfs-provider, vfs-zip, vfs-core, vfs-registry.
This was a read-only audit. No files in the repo were changed.

## Tool results

- `cargo clippy --all-targets` reports **0 warnings** for each of these crates: `vfs-storage`, `vfs-block-store`, `vfs-compose`, `vfs-provider`, `vfs-zip`, `vfs-core` and `vfs-registry`.
- It also reports 0 warnings with `vfs-storage --features test-hooks` and with `vfs-block-store --features crash-points`.
  - I did not build `gpu-zstd`, because it needs a git dependency.
- The only `#[allow]`s are:
  - eleven `clippy::type_complexity`, nine of them on test-hook fields (`storage.rs:194-213`, `cached.rs:167,470`, `layer.rs:2235`, `overlay.rs:1137`, `block-store/store.rs:81`);
  - two `single_range_in_vec_init`, in reconcile tests;
  - `#![allow(dead_code)]` in `vfs-block-store/tests/common/mod.rs`, which is the normal pattern for shared test helpers.
- There are no `TODO`, `FIXME` or `todo!` markers and no commented-out code in these crates.
- Feature flags:
  - `test-hooks` (vfs-storage) is live. Haskill uses it in `haskill-store/Cargo.toml:37` and calls it from `haskill-store/tests/warm_bulk.rs:365`. However, aether-vfs CI never compiles it (`.github/workflows/ci.yml:143`). See T6.
  - `crash-points` (block-store) is live and runs in CI (`ci.yml:150-153`).

## Haskill API surface (checked against `/home/tbaldrid/oss/haskill/crates`)

- **Haskill uses from vfs-storage:**
  - types: `Storage`, `StorageConfig`, `StorageError`, `CloseOutcome`, `ScratchDir`, `SourceKey`, `SpaceUsage`, `Usage`, `WriteClass`, `with_write_class`, `WriteStats`, `BulkCompression`, `StoreConfig`, `Gpu*`;
  - `Storage` methods: `layer`, `put_files`, `sync`, `close`, `delete_layer`, `clear_cache`, `space_usage`, `write_stats`, `compression`;
  - the test-only `crash_on_drop_for_tests`.
- **Haskill does not use from vfs-storage:** `Catalog`, `EntryRec`, `CacheRec`, the `ids` functions, `StoreIdKind`, `RamTier` and `RamStats`.
- **Haskill uses from vfs-provider:**
  - the `Provider` trait, which it implements three times: `haskill-vfs/src/provider.rs:740`, `haskill-store/src/content.rs:303` and `haskill/src/options/apply.rs:515`;
  - `VPath`, `RootId`, `Stat`, `DirEntry`, `Handle`, `Capabilities`, `Access`, the `KIND_*`, `OPEN_*` and `ST_*` constants, and `stored_name`;
  - from the conformance module: `RwMemFixture` and `FIXTURE_FILES`.
- Haskill does not use vfs-compose, vfs-zip, vfs-core or vfs-registry directly.
- So any change to the `Provider` method signatures, to `Result<_, i32>`, or to the conformance module's location touches Haskill. Changes to vfs-storage internals do not.

---

## High-value findings

### T1 — Stale docs: the overlay is described as read-only, but it does copy-up writes
- **Category:** stale docs
- **Evidence:**
  - `rust/docs/architecture.md:133-137`: "read-only for now: it always declares `Access::Read` and rejects `OPEN_WRITE`; copy-up writes are a later stage".
  - `architecture.md:716` (crate map): "read-only provider combinators".
  - `architecture.md:743-744`: "Copy-on-write is partial … create/write-through is outstanding".
  - `vfs-compose/src/lib.rs:1-4`: "create/write-through is M-Write follow-up".
  - `vfs-provider/README.md:246,259-260`: "`overlay` exists but is read-only for now (`OverlayProvider::open` rejects `OPEN_WRITE`".
  - The code says otherwise:
    - `overlay.rs:1-21` documents whole-file copy-up, `.cu.` staging and `.wh.` removal;
    - `overlay.rs:526-537` declares `Access::ReadWrite`;
    - `overlay.rs:641-644` routes `OPEN_WRITE` to `open_for_write`;
    - `vfs-compose/src/memory.rs` is a read-write provider.
- **Proposed change:**
  - Rewrite the vfs-compose crate doc, the architecture.md §3.2 bullet, the crate-map row and the §9 limitation, and the README §§ around lines 246-260.
  - While editing, drop the "(Clojure `…`)" provenance notes (`lib.rs:1`, `layered.rs:1`, `router.rs:1`, `inline.rs:1`) and the "Stage 1" wording (`router.rs:23-25`, `vfs-provider/src/path.rs:10`).
- **Value:** High. This is the first thing a reviewer reads, and it contradicts the code.
- **Effort:** S
- **Risk:** Low (docs only).
- **Touches Haskill API:** No.

### T2 — Optional `Provider` methods are silently dropped by every pass-through wrapper (`stored_name`), and nothing checks this
- **Category:** abstraction / performance
- **Evidence:**
  - `vfs-provider/src/provider.rs:59-73` documents `stored_name` as the fast path. The fallback, `vfs_compose::stored_name` (`vfs-compose/src/lib.rs:50-73`), lists the whole parent directory on every call.
  - These implement it:
    - `layer.rs:764`;
    - `overlay.rs:792`;
    - `vfs-director/src/mount_graph.rs:154`;
    - `haskill-vfs/src/provider.rs:775`.
  - These do **not** forward it: `layered.rs`, `router.rs`, `subdir.rs`, `readonly.rs`, `seekable.rs` and `vfs-storage/src/cached.rs`.
  - `ZipProvider` (`vfs-zip/src/backend.rs:141-286`) does not implement it, although it already holds the exact index it needs (`by_fold`, `backend.rs:46,94-113`).
  - So the main game mount (a zip under `SubdirProvider`, under `LayeredProvider` or `OverlayProvider`) always takes the full-listing fallback. `MountGraph::stored_name` (`mount_graph.rs:166-168`) then repeats it per mount.
  - The conformance suite never checks `stored_name` or `is_immutable` (`grep` finds no hits in `vfs-provider/src/conformance.rs`).
  - `seekable.rs` also does not forward `is_immutable`.
- **Proposed change:**
  1. Forward `stored_name` in `subdir` (with path mapping), `readonly`, `seekable` and `cached` (to the inner provider).
  2. In `layered`, try top then bottom, using the same spelling rule as its `readdir` (see T21).
  3. In `router`, use the matched route.
  4. Implement `stored_name` in `ZipProvider` from `by_fold`.
  5. Add a conformance case: when `stored_name` is not `ST_NOT_SUPPORTED`, it must agree with the parent listing for every fixture path in mixed case, and the root must give `None`.
  6. Optionally add a case that `is_immutable(h)` implies `capabilities().immutable || <composite>`.
- **Value:** High. This is a real per-final-name-query cost on large directories such as `Data`, and the `Provider` contract invites the omission.
- **Effort:** S–M
- **Risk:** Low. The behaviour is defined as "same answer as the listing", and the new conformance case pins that.
- **Touches Haskill API:** No signature change. Haskill's providers gain a new conformance assertion; `HaskillProvider` already implements the method.

### T3 — Registry depth cap is enforced only on decode, so a deep tree saves fine and then the whole profile is set aside on the next load
- **Category:** vfs-registry correctness and tests
- **Evidence:**
  - `vfs-registry/src/format.rs:17-18` sets `MAX_DEPTH = 1024`, and `decode_node` rejects deeper trees (`format.rs:186-188`).
  - `Overlay` has no depth check:
    - `valid_path` (`overlay.rs:104-109`) checks only the `\Registry` prefix and that no component is empty;
    - `create_key`, `set_value` and `ensure_chain` accept any depth.
  - `encode_node` (`format.rs:61-98`) recurses without a bound.
  - The director's load path (`vfs-director/src/registry.rs:404-421`) renames an undecodable `overlay.reg` aside as corrupt. So a process that creates keys more than 1024 deep loses the **entire** per-profile registry layer at the next start.
  - An NT key path can carry roughly 16k one-character components, so on a director thread the unbounded `encode_node` recursion is also a stack-overflow risk.
  - There is no depth test anywhere: `grep depth|nested` finds only the decode guard.
  - Long names are tested at the overlay level (`overlay.rs:687-712`, 255/256 and 16383/16384) and for a 256-character key in decode (`format.rs:503-506`). There is no `encode→decode` round trip at the limits with multi-byte names (3-byte BMP or surrogate pairs), and no layout test of a maximum-length name against `ST_REPLY_TOO_LARGE`.
- **Proposed change:**
  1. Add one `MAX_KEY_DEPTH` (512, Windows' own limit) in `overlay.rs` and check it in `valid_path`, or a `check_depth` called from `create_key`, `set_value`, `delete_*` and `rename_key`. Return `RegError::InvalidPath`, or a new `TooDeep` that maps to `STATUS_INVALID_PARAMETER`.
  2. Make `format::MAX_DEPTH` refer to the same constant.
  3. Make `encode` iterative, or keep it recursive now that depth is bounded.
  4. Add tests:
     - create at depth 512 succeeds and 513 fails;
     - an encode/decode round trip at 512;
     - round trips of 255-unit key names and 16383-unit value names built from `é` and `😀`.
- **Value:** High (silent data loss).
- **Effort:** S
- **Risk:** Low. The only behaviour change is refusing keys deeper than Windows allows.
- **Touches Haskill API:** No.

### T4 — The overlay marker convention (`.wh.` and `.cu.`) is hand-copied across crates
- **Category:** duplication / leaky layering
- **Evidence:**
  - `vfs-storage/src/manage.rs:20-28` says: "`OverlayProvider` … does not export it as a constant). Duplicated here; keep the two in step." More literals follow at `manage.rs:626-636`.
  - `vfs-compose/src/overlay.rs:170,173,181,184,200,323,595,601` hard-code `".wh."` and `".cu."`.
  - The shim spells its own convention differently (`vfs-shim/src/overlay.rs:24-42,363`).
- **Proposed change:** Add to `vfs-provider/src/layout.rs`, which already hosts the cross-process layout convention `overlay_layer_dir` and has no dependencies:
  - `pub const WHITEOUT_PREFIX`;
  - `pub const COPY_UP_PREFIX`;
  - `pub fn whiteout_name(name)`;
  - `pub fn copy_up_name(n, name)`;
  - `pub fn is_overlay_marker(name)`.

  Use them in `overlay.rs` and `manage.rs`.
- **Value:** Med–High. If the copies drift, `export_layer` would export markers as real files.
- **Effort:** S
- **Risk:** Low.
- **Touches Haskill API:** No (additive).

### T5 — Durability policy: about eight special cases, documented in six places, with state spread over two structs
- **Category:** durability / fsync coherence
- **Evidence (the special cases):**
  - the Deferred interval;
  - the 10,000-commit bound;
  - the rewrite-in-place exception (`fresh` plus epoch);
  - the scratch-dir exception to that exception;
  - durable points on layer create, import, delete and drop;
  - skipping the fsync when nothing is unflushed;
  - deferred deletes (`doomed`);
  - the clean-close token and dirty flag that let open skip reconcile;
  - retry on failure, which pushes `last` back a year (`storage.rs:285-293`).
- **Evidence (where the docs live):**
  - `vfs-storage/src/lib.rs:9-17` (summary);
  - `config.rs:7-80` (the `Durability` enum, the fullest version);
  - `layer.rs:21-53` (a near-duplicate of `config.rs`);
  - `storage.rs:136-160` (gate and lock order), `storage.rs:328-344` (token skip) and `storage.rs:470-484` (clean close);
  - the `catalog.rs:17-30` "## Durability" section;
  - the `reconcile.rs:1-55` module doc.
- **Evidence (inconsistencies):**
  - `config.rs:28` and `layer.rs:34` hard-code "10,000", or name `storage::DEFERRED_MAX_COMMITS`, although the bound is the configurable `max_deferred_commits` (`config.rs:118`).
  - The spec (`docs/superpowers/specs/2026-09-29-vfs-storage-design.md:169-203`) does not mention the clean-close token, `scratch_dirs` or the `fresh`/epoch rule.
- **Evidence (where the state lives):**
  - `Storage` holds `gate`, `clock`, `doomed`, `dirty` and `shut` (`storage.rs:161-187`).
  - `LayerProvider` holds `fresh` (`layer.rs:159-163`).
  - The decision itself is in `LayerProvider::changed`, `is_fresh` and `is_scratch` (`layer.rs:455-517`).
  - Aliases add a layer of indirection:
    - `Storage::flush_durably` is identical to `durable_point` (`storage.rs:575-579`);
    - `LayerProvider::durable_point` only wraps it (`layer.rs:448-453`).
- **Verdict:** The logic is coherent. I found no ordering bug: the gate keeps rows and data paired, deletes wait for a durable point, and the token handshake is sound. But it is not documented in one place, and the policy decision is split between `layer.rs` and `storage.rs`.
- **Proposed change:**
  1. Create `vfs-storage/src/durable.rs` owning:
     - `DurableClock`;
     - the gate guards (`SharedGate`, `gate_exclusive`);
     - `doomed`;
     - `dirty` and `shut` with `needs_reconcile`;
     - the clean-close token (`clean_close_token`, `close_cleanly`);
     - a single `fn after_change(&self, layer, rewrote: Option<&FileCell>, fresh, scratch) -> Result<()>` holding the policy now in `LayerProvider::changed`.
  2. Make its module doc the canonical policy table: the trigger → action matrix, the crash outcomes and the lock order.
  3. Cut `lib.rs`, `layer.rs` and the `catalog.rs` section down to one-line links. Keep `config.rs` to the user-facing semantics.
  4. Delete `flush_durably`.
  5. Pre-fold `scratch_dirs` at open; today `fold(&d.dir)` runs on every close (`layer.rs:495`).
  6. Update the spec, or point it at the module.
- **Value:** Med–High for maintainability.
- **Effort:** M
- **Risk:** Med. It moves lock-holding code, so the lock-order comment must move with it. The existing durability tests are thorough (`layer.rs:1609-2160`, `storage.rs:861-1200`) and act as the safety net.
- **Touches Haskill API:** No.

### T6 — Crash-simulation hooks are partial and duplicated, and `test-hooks` is not built in aether CI
- **Category:** durability / test hooks
- **Evidence:**
  - `Storage::crash_on_drop_for_tests` (`storage.rs:551-558`) only sets `shut`. But `LayerProvider::drop` still runs `durable_point()` (`layer.rs:984`), and so does `durable_point` itself, which never checks `shut`. A "crash" after a layer drop therefore makes everything durable.
  - `vfs-embed/src/session.rs:2540-2563` documents exactly this and re-implements a directory snapshot (`killed_copy`).
  - The same snapshot exists as `vfs-storage/src/test_util.rs:10-21` and as `layer.rs:1317` (`killed_copy`).
  - There are two other crash simulations as well: `#[cfg(test)] close_unclean` (`storage.rs:544-549`) and the block-store `crash-points` subprocess tests.
  - CI's Linux test step (`ci.yml:143`) never enables `test-hooks`, so Haskill is the only thing that compiles it.
- **Proposed change:**
  1. Under `test-hooks`, make "crashed" a real state:
     - `durable_point` and `LayerProvider::drop` return early once `shut` is set by the crash hook;
     - or, better, use a separate `crashed: AtomicBool` that both check.
  2. Export `pub fn snapshot_as_killed(dir, to)` (from `test_util::snapshot`) under `test-hooks`, so vfs-embed and Haskill stop copying it.
  3. Add a `cargo clippy -p vfs-storage --features test-hooks` step to CI.
- **Value:** Med. Tests that believe they simulate a crash may not.
- **Effort:** S
- **Risk:** Low (test-only code).
- **Touches Haskill API:** Yes, test-only. Haskill's `warm_bulk.rs:365` keeps working; the semantics become stricter, so that test should be re-run.

### T7 — vfs-storage exports internals that nobody uses, including an unguarded `Catalog`, and has a dead `durable` parameter
- **Category:** dead pub items / leaky API
- **Evidence:**
  - `vfs-storage/src/lib.rs:33-40` re-exports `Catalog`, `CacheRec`, `EntryRec`, `cache_file_id`, `classify_store_id`, `layer_file_id`, `new_guid`, `Guid`, `StoreIdKind`, `RamTier` and `RamStats`.
  - Outside the crate, none of these are used: not in Haskill and not in any aether crate. `vfs-embed/src/lib.rs:196-199` re-exports only the `Storage`-level types.
  - `Catalog`'s public `put`, `remove`, `rename`, `drop_layer` and `put_many` bypass the durability gate and the layer `ns` lock. That is a footgun if anyone picks them up.
  - The `durable: bool` parameter of `Catalog::put`, `put_many` and `remove` (`catalog.rs:336,352,375`) is `false` at every production call site. It is `true` only in one test (`catalog.rs:908`), and a durable commit without a prior store flush would break the spec §6 invariant.
  - `RamTier::new` (`ram.rs:123`) is used only by tests.
- **Proposed change:**
  - Make these `pub(crate)`, along with `pub mod`-level items as needed.
  - Drop the `durable` parameter; `commit_durable` already exists.
  - Keep `ReconcileReport`, `LayerInfo`, `StorageStats`, `CacheStats`, `ClearReport` and `SpaceUsage` public.
- **Value:** Med
- **Effort:** S
- **Risk:** Low. The compiler will show any hidden user.
- **Touches Haskill API:** No (verified).

---

## Medium-value findings

### T8 — Split `vfs-storage/src/layer.rs` (2,986 lines: about 1,000 of code and about 2,000 of tests)
- **Category:** oversized file
- **Evidence:**
  - code spans lines 1-997 and the tests 999-2986;
  - the tests cover several unrelated themes: I/O (1177-1300), durability and kill tests (1317-2160), concurrency (2204-2607), namespace rules (2608-2768), and overlay spelling (2769-2986);
  - path parsing (`LPath`, `folded_path`, 92-140) is generic and is duplicated elsewhere (T12).
- **Proposed layout:**
  ```
  layer/mod.rs         LayerProvider struct, new, Drop, small helpers (get/put/st_err)
  layer/namespace.rs   ensure_parents/rollback/create/rename_rows/set_attr_impl/doom
  layer/provider.rs    impl Provider
  layer/batch.rs       put_files
  layer/path.rs        LPath, folded_path (or move to vfs-core, T12)
  layer/tests/{io,durability,concurrency,namespace,overlay}.rs
  ```
  The `changed`, `is_fresh` and `is_scratch` functions move to `durable.rs` (T5).
- **Value:** Med
- **Effort:** M
- **Risk:** Low (mechanical).
- **Touches Haskill API:** No.

### T9 — Split `vfs-storage/src/cached.rs` (1,996 lines), and move the crate-wide utilities out of it
- **Category:** oversized file / layering inside the crate
- **Evidence:**
  - It mixes three things:
    - the shared cache bookkeeping (`CacheState`, `AccessLog` and `impl Storage { cached, cached_coverage, cache_stats, cache_acquire, touch, commit_access, budget_snapshot, remove_cache_row }`, lines 104-512);
    - the per-source provider (`CachedSource`, 516-888);
    - 1,100 lines of tests.
  - Eviction and `clear_cache` tests live here (`cached.rs:1532-1653`, `1897-1972`), although the code is in `evict.rs`.
  - Generic helpers live in `cached.rs` and are imported crate-wide:
    - `pub(crate) fn lock` (poison-ignoring, `cached.rs:173`), used by `storage.rs:13`, `manage.rs:15` and `layer.rs:989`;
    - `now_minute` (`cached.rs:177`).
  - `layer.rs:72-84` defines a second `lock` with the opposite poison policy (poison becomes `ST_IO_ERROR`). The crate therefore has two poison policies with no stated reason.
- **Proposed change:**
  - Use `cached/{mod.rs (CachedSource), state.rs (CacheState + Storage cache ops), tests.rs}`.
  - Move the eviction tests to `evict.rs`.
  - Create `util.rs` holding `lock`, `lock_status` and `now_minute`.
  - Document the poison policy once.
- **Value:** Med
- **Effort:** M
- **Risk:** Low.
- **Touches Haskill API:** No.

### T10 — Split `vfs-compose/src/overlay.rs` (1,804 lines), and replace its private in-memory upper
- **Category:** oversized file / duplication
- **Evidence:**
  - The whiteout index is about 250 lines (`overlay.rs:42-95`, `195-380`).
  - Copy-up is `384-525`.
  - Tests run from 803 to 1804.
  - `pub(crate) struct MemUpper` (`overlay.rs:855-1089`, about 235 lines) is another hand-written read-write in-memory provider. `subdir.rs:213-218` reaches into `overlay::tests` for it, while `MemoryProvider` exists in the same crate.
- **Proposed layout:**
  ```
  overlay/mod.rs        OverlayProvider, impl Provider
  overlay/whiteout.rs   WhiteoutIndex + scan/walk/note/invalidate/clear/write
  overlay/copy_up.rs    CopyGuard, copy_up_if_needed, copy_file_up, copy_bytes/loop, open_for_write
  overlay/tests.rs
  ```
  - Replace `MemUpper` with `MemoryProvider`, keeping a thin counting wrapper where a test needs counts.
- **Value:** Med
- **Effort:** M
- **Risk:** Low.
- **Touches Haskill API:** No.

### T11 — Handle-table and forwarding boilerplate is repeated in about 10 providers
- **Category:** duplication
- **Evidence:**
  - The same three pieces appear in each: a `next: AtomicU64` counter, an `opens: Mutex<HashMap<u64, T>>` table, and track/lookup/close functions with `.lock().map_err(|_| map_io_err())`.
  - Locations:
    - `layered.rs:23-24,57-61,137-142,157-163`;
    - `router.rs:29-30,80`;
    - `inline.rs:25-26,172`;
    - `memory.rs:253-254,391`;
    - `seekable.rs:85-86,171`;
    - `overlay.rs:45-46,148-165`;
    - `vfs-zip/src/backend.rs:233-286`;
    - `cached.rs:531-540,763-768`;
    - `layer.rs:156-158,519-533`;
    - `vfs-director/src/disk.rs:18-26`.
  - The `.lock().map_err(...)` idiom appears 44 times in vfs-compose alone.
  - Every pass-through wrapper (`subdir`, `readonly`, `seekable`) also hand-forwards about 15 methods. That is how T2 happened.
- **Proposed change:**
  1. Add `vfs_provider::HandleTable<T>` with `insert(T) -> Handle`, `get(h) -> Result<T, i32>` (where `T: Clone`), `remove(h)` and `with(h, f)`. Bad handles return `ST_BAD_FH` and poison returns `ST_IO_ERROR`.
  2. Optionally add a `delegate_provider!` macro, or a `ProviderWrapper` trait with forwarding defaults, so a new wrapper inherits `stored_name` and `is_immutable` instead of dropping them.
- **Value:** Med
- **Effort:** M
- **Risk:** Low (each provider is covered by conformance).
- **Touches Haskill API:** Additive only. Haskill could adopt it but does not have to.

### T12 — Path normalisation and splitting are reimplemented about 10 times, with differing `..` semantics
- **Category:** duplication
- **Evidence:**
  - `vfs-compose/src/memory.rs:49-51` and `inline.rs:98-100` are byte-identical (`replace('\\', "/").trim_matches('/')`).
  - The same expression appears inline in `subdir.rs:20-24` and `62`.
  - `glob.rs:25-29` is the same again, plus a fold.
  - `layer.rs:101-113` (`LPath::parse`) and `layer.rs:128-140` (`folded_path`) do the same job twice in one file. Both refuse `.` and `..`.
  - `vfs-core/src/path.rs:18-40` (`normalize_vpath`) *resolves* `..` and strips NT prefixes instead.
  - `vfs-core/src/finalname.rs:19-26` has its own `components`.
  - `vfs-zip/src/lib.rs:187` and `backend.rs:81,120` use their own backslash handling and parent splitting.
  - `overlay.rs:167-192` has three `rsplit_once('/')` helpers.
  - `vfs-director/src/mount_graph.rs:155` has another `normalize`.
  - In vfs-registry:
    - `path.rs:103-105` defines `fold` as a pure forward to `vfs_core::fold`. That is harmless, but there are now two import paths;
    - `layout.rs:109-111` duplicates `path::leaf` (`path.rs:114-116`).
  - Case folding itself is properly centralised in `vfs_core::fold` (`casefold.rs:41`). `vfs-compose/src/casefold.rs` adds component-wise helpers on top of it, which is fine.
- **Proposed change:**
  - Add to `vfs-core::path`:
    - `rel_components(&str) -> Result<impl Iterator<&str>, PathError>` (accepts either separator, drops empty components, refuses `.` and `..`);
    - `normalize_rel(&str) -> Result<String, PathError>`;
    - `split_parent(&str) -> (&str, &str)`.
  - Keep `normalize_vpath` for callers that need `..` resolution, and name the distinction in its doc.
  - Use the new helpers in compose, storage, zip and director.
  - Delete `registry::layout::leaf`.
- **Value:** Med
- **Effort:** M
- **Risk:** Med. The semantics differ today: some callers trim silently, some refuse `..`, and the zip code trims only leading `/`. Each replacement needs a test of the edge it used to have.
- **Touches Haskill API:** No.

### T13 — Status and error mapping is ad hoc and loses information
- **Category:** duplication / abstraction
- **Evidence:**
  - Every provider method returns `Result<_, i32>`.
  - `vfs_provider::map_io_err()` takes no argument and only returns `ST_IO_ERROR` (`status.rs:32`). It has 107 call sites across storage, compose and provider, mostly as `.lock().map_err(|_| map_io_err())`.
  - `ok()` (`status.rs:28`) has no callers anywhere.
  - There is no `io::Error → status` helper:
    - `vfs-zip/src/backend.rs` maps every I/O error, including `NotFound`, to `ST_IO_ERROR` (7 sites);
    - `vfs-director/src/disk.rs:383` maps `NotFound` properly;
    - `vfs-directord/src/service.rs:409` special-cases `StorageError::Io(NotFound)` itself.
  - `StorageError::to_status` (`storage.rs:61-74`) is the only typed mapping.
- **Proposed change:**
  - Add `vfs_provider::status::from_io(&io::Error) -> i32`, covering `NotFound`, `AlreadyExists`, `IsADirectory`, `NotADirectory` and the storage-full error.
  - Add `vfs_provider::lock_or_status`.
  - Rename `map_io_err` to `io_error()` and keep a deprecated alias.
  - Remove `ok()`.
  - Do **not** introduce a `Status` newtype now. That would change all three Haskill `Provider` impls and the ring codecs (L effort, High risk), for limited gain.
- **Value:** Med
- **Effort:** S (helpers) or L (newtype).
- **Risk:** Low for the helpers.
- **Touches Haskill API:** The helpers are additive. The `map_io_err` rename is non-breaking with an alias. A newtype would break Haskill.

### T14 — vfs-zip has dependency, API and concurrency leftovers
- **Category:** dead code / abstraction / performance
- **Evidence:**
  - `vfs-zip/Cargo.toml:10` makes `vfs-protocol` a normal dependency, but it is used only in a test (`lib.rs:343`). Use `vfs_provider` there.
  - `open_backend` (`lib.rs:19-21`) is an unused alias of `ZipProvider::open`.
  - `read_layer` (`lib.rs:75`), the "legacy" `vfs-core` `Layer` path, is used only by `vfs-launch/src/bin/vfs-fuse-bench.rs:556`.
  - `ZipError` has no `Display` or `Error` implementation (`lib.rs:24-35`).
  - `read_at` serialises **all** reads of **all** handles on one `Mutex`, plus a `seek` and a non-exact `read` (`backend.rs:264-279`). The comment there says "Revisit with per-handle File handles later." Positional reads (`std::os::unix::fs::FileExt::read_at` / `std::os::windows::fs::FileExt::seek_read`) on a per-handle `File`, read under a shared lock, remove the global serialisation.
  - Tests use hard-coded `C:\GameLayers\…` archives (`lib.rs:414-436`) and `std::env::temp_dir()` instead of `tempfile`.
  - `close` returns `Ok` for unknown handles (`backend.rs:281-285`).
  - The missing `stored_name` is covered in T2.
- **Proposed change:**
  - Move `vfs-protocol` to dev-dependencies, or drop it.
  - Delete `open_backend`.
  - Put `read_layer` behind a `legacy-layer` feature, or move it into the bench.
  - Implement `Display` and `Error` for `ZipError`.
  - Switch to positional reads.
  - Use `tempfile` in tests and an environment variable for the corpus path.
- **Value:** Med (the read lock is the main item).
- **Effort:** S–M
- **Risk:** Med for the read-path change: 0xC0000409 history. Positional reads have no shared cursor, which was the original race, but the change needs the corpus integrity test (`vfs-embed/tests/zip_serve_integrity.rs`).
- **Touches Haskill API:** No.

### T15 — vfs-registry: deferred review items, duplicate types, and splitting `layout.rs`
- **Category:** vfs-registry
- **Evidence and proposals:**
  - **`deleted_at` grows without bound** (`overlay.rs:94-98`, written in `tomb` at `overlay.rs:438-442`).
    - It is in memory only: `decode` rebuilds with version 1 and an empty `deleted_at`. So it grows for the life of a session, by one entry per distinct deleted path.
    - Proposal: have the director pass the oldest generation any client may still ask about (`changed_since`'s lower bound), and prune entries at or below it in `tomb` and periodically.
    - Simpler alternative: cap at N entries and, on overflow, bump a "global deletion" floor version that makes `changed_since` answer `true` for versions below it. That is conservative, costing a spurious notify and never a missed one.
    - S effort, Low risk.
  - **Weak checksum** (`format.rs:42-51`): a wrapping sum of u64 words is blind to reordered words and to inserted zero words.
    - The crate depends only on vfs-core. Either add `crc32fast` (tiny) or implement CRC-32C inline, then bump `MAGIC` to `\x02`. `decode` should accept `\x01` with the old sum so existing layers still load.
    - S effort, Low risk if the old version is still read.
  - **Per-node memory.** Each `Entry` stores its full stored-spelling path (`overlay.rs:76-85`), the `BTreeMap` key repeats the full folded path, and the parent's `children` repeats the leaf spelling.
    - This is three copies of names per node, and full paths are O(depth²) bytes per subtree.
    - Fine for today's sizes. If it matters, store the leaf spelling only and rebuild paths in `walk` and `encode`, or intern paths in an arena with parent indices.
    - Low value now, M effort.
  - **One fact stored twice.** A tombstoned child is recorded both in the parent's `children` as `Child::Tombstone` and in `Overlay::tombs` (`overlay.rs:88-94`, `insert_node` at 197, `delete_key` at 315, `rename_key` at 342). Every mutation has to keep the two in step.
    - Proposal: derive `tombs` lookups from the parent's `children`, or keep `tombs` only.
    - M effort, Med risk; the tests are good.
  - **`RealKey` and `MergedKey` have identical fields** (`merge.rs:7-28`).
    - Make one type (`KeyView`), or a type alias.
    - S effort, Low risk.
  - **NTSTATUS constants duplicated.** `layout.rs:16-20` repeats `vfs-shim/src/ntdef.rs:13,66,252,543`, and several shim tests (`regkeys.rs:37-42`, `regquery.rs:39-44`, `regwrite.rs:36-42`) redefine them again.
    - Have the shim import `vfs_registry::layout::STATUS_*`, or add a tiny `nt-status` module in a dependency-free crate.
    - S effort.
  - **Splitting `layout.rs`** (1,179 lines: about 485 of code and about 690 of tests). Yes, split it by information class:
    ```
    layout/mod.rs     Out, Written, ValueEntry, MultipleWritten, KeyInfoClass, ValueInfoClass,
                      STATUS_*, helpers (len32, utf16le, utf16_bytes, align, status, too_small)
    layout/key.rs     counts, class_bytes, key_basic/node/full/name/cached/zeroed,
                      write_key_info, write_subkey_info            (layout.rs:99-330)
    layout/value.rs   write_value_info, write_multiple_values, KEY_VALUE_ENTRY_SIZE (332-485)
    layout/tests/{key,value}.rs
    ```
    M effort, Low risk.
  - **Public surface.** Every module is `pub mod` *and* re-exported (`lib.rs:6-15`). Consumers use both `vfs_registry::Lookup` and `vfs_registry::overlay::…`. Pick one.
  - **Case-folding nit.** NT compares registry names by *upcasing* (`RtlUpcaseUnicodeChar`), while `fold` lowercases with Unicode rules: `ſ`, `K` (Kelvin sign) and `İ` behave differently. Document this as a known divergence, or add a registry-specific fold. Low value.
- **Touches Haskill API:** No.

### T16 — Several in-memory providers that overlap
- **Category:** the same type existing more than once
- **Evidence:**
  - `vfs-provider`: `MemFixture` (`conformance.rs:38`), `RwMemFixture` (`conformance.rs:214`, used by Haskill tests), and `SeqFixture`, plus test-only fixtures.
  - `vfs-compose`: `InlineProvider` (read-only, immutable), `MemoryProvider` (read-write, `memory.rs`, with a long justification at `memory.rs:10-35`), and `MemUpper` (overlay tests, T10).
- **Proposed change:**
  - Implement `InlineProvider` as `MemoryProvider` in a read-only, immutable mode, keeping the public name and its contract.
  - Delete `MemUpper`.
  - Leave the vfs-provider fixtures alone: they exist so the suite can test itself without dependencies.
- **Value:** Low–Med
- **Effort:** M
- **Risk:** Low–Med (many tests key off `InlineProvider`'s declared capabilities).
- **Touches Haskill API:** No.

### T17 — `SeqRead` / `read_next` is close to dead and has a latent bug
- **Category:** a `Provider` trait method that no production provider implements
- **Evidence:**
  - Implementations of `read_next` exist only in test fixtures (`conformance.rs:202,1293`, `seekable.rs:408`) and in pass-throughs (`readonly.rs:87`, `seekable.rs:211`).
  - `RemoteProvider` maps wire access `0` to `Access::SeqRead` (`vfs-source/src/remote.rs:48`) but implements only `read_at` (`remote.rs:163`). A sequential-only gRPC plugin would therefore fail every read once wrapped in `SeekableProvider`.
  - `vfs-embed` refuses `SeqRead` mounts outright (`vfs-embed/src/session.rs:292-316`).
- **Proposed change:** Pick one:
  - (a) implement `read_next` in `RemoteProvider` over the wire contract; or
  - (b) remove `SeqRead`, `read_next` and `SeekableProvider` from the contract and reject access value `0` in the remote handshake.

  Option (b) is the cleanup. Option (a) keeps the design promise.
- **Value:** Med (it removes a trap).
- **Effort:** S–M
- **Risk:** Low.
- **Touches Haskill API:** No. Haskill implements neither `read_next` nor `SeqRead`, and removing a defaulted method does not break its impls.

---

## Low-value findings and nitpicks

### T18 — vfs-core has three identical kind enums and duplicate `Stat`/`DirEntry` names
- **Category:** duplicate types
- **Evidence:**
  - `EntryKind` and `NodeKind` are identical (`vfs-core/src/model.rs:12-17,45-50`), and `WalkNodeKind` is a third (`tree.rs:305-329`).
  - `vfs_core::{Stat, DirEntry}` (`model.rs:52-65`) shadow the names of `vfs_provider::{Stat, DirEntry}`, which have a different shape (`kind: u8`). The vfs-core versions are unused outside the crate.
  - The tree model (`VfsTree`, `Layer`, `InputEntry`, `SourceId`, `cachekey`) serves only `vfs-server`, `vfs-shared` and `vfs_zip::read_layer`. The provider stack uses only `fold`, `normalize_vpath`, `wildcard_match` and `finalname`.
  - `vfs-server` and `vfs-unix` are missing from the architecture crate map (`architecture.md:705-730`).
- **Proposed change:**
  - Merge `EntryKind` and `NodeKind`.
  - Rename the vfs-core `Stat` and `DirEntry` to `TreeStat` and `TreeEntry`, or make them private.
  - Longer term, split `vfs-core` into `vfs-path` (fold, path, wildcard, finalname) and the tree model.
  - Add the missing crates to the crate map.
- **Value:** Low
- **Effort:** S (merges) to M (crate split).
- **Risk:** Low.
- **Touches Haskill API:** No.

### T19 — The conformance suite is compiled into every consumer's release build
- **Category:** organisation
- **Evidence:** `vfs-provider/src/lib.rs:7` has an unconditional `pub mod conformance;` (1,355 lines, with fixture providers and `assert!`s). It reaches the director and, via vfs-protocol, the shim.
- **Proposed change:** Put it behind a `conformance` feature that consumers enable in `[dev-dependencies]`. In practice the linker discards most of the unused code, so the gain is clarity rather than size.
- **Value:** Low
- **Effort:** S
- **Risk:** Low.
- **Touches Haskill API:** Yes. `haskill-vfs/tests/common/mod.rs:24` and `haskill/src/options/launch.rs:516` would need `features = ["conformance"]` on a dev-dependency.

### T20 — Overlay copy-up waits by spinning, keyed without the root
- **Category:** code quality
- **Evidence:**
  - `overlay.rs:399-407` loops on `yield_now()` while another thread copies a file, which can be multi-MB, so the waiter burns CPU.
  - The in-flight key is `fold(p.rel)` with no `RootId`, so the same relative path under two roots serialises unnecessarily.
- **Proposed change:** Use a `Mutex<HashSet<(u32, String)>>` plus a `Condvar`.
- **Value:** Low
- **Effort:** S
- **Risk:** Low.
- **Touches Haskill API:** No.

### T21 — Merge spelling rules differ between `LayeredProvider` and `OverlayProvider`
- **Category:** inconsistency between overlay and layered
- **Evidence:**
  - `layered.rs:108-114`: the top layer's entry, including its spelling, overwrites the bottom's.
  - `overlay.rs:604-618`: the base layer's spelling is kept, and only the stat comes from upper. That rule was introduced as a fix, because "letting it win renamed `Data` to `data`".
  - A writable `LayeredProvider` (whose `write_target`, `layered.rs:44-53`, picks the top) can show the same respelling bug.
- **Proposed change:**
  - Extract one `merge_listing(lower, upper, policy)` into `vfs-compose/src/lib.rs`, next to `sorted_by_folded_name`.
  - Share it between overlay, layered and `MountGraph`.
  - Use it for T2's `stored_name`.
- **Value:** Med (correctness of spelling) once layered stacks are written through.
- **Effort:** S–M
- **Risk:** Med (visible spelling changes in listings).
- **Touches Haskill API:** No.

### T22 — Small nits
- **Category:** nitpicks
- **Edition and workspace settings.** Editions are mixed: `vfs-block-store` is 2024, everything else 2021. There is no `[workspace.package]` or `[workspace.lints]`. `publish = false` is set on only three of these seven crates.
- **Pointless wrapper.** `LayerProvider::set_attr` only calls `set_attr_impl` (`layer.rs:759-761, 930`). Inline it.
- **Test hook bypassed.** `layer.rs:389` and `layer.rs:865` call `storage.store.delete` directly instead of `Storage::store_delete`, so the `fail_deletes` hook (`storage.rs:530-538`) does not cover rollbacks.
- **Duplicate constant.** `RUN_BLOCKS = 64` is defined in both `reconcile.rs:113` and `layer_io.rs:60`. Share it from `layer_io`.
- **Formatting.** The one-line functions in `vfs-provider/src/status.rs:28-37` are not rustfmt-shaped, which suggests rustfmt is not run on this crate.
- **Dead-code allow.** `vfs-block-store/tests/common/mod.rs:1` has a blanket `#![allow(dead_code)]`. This is acceptable; per-item `#[allow]` would be tidier.
- **Stale comment.** The provider README and `caps.rs` mention a "future FUSE mount", which is fine. But the `VPath` doc at `path.rs:18-24` still narrates an "earlier version of this comment". Trim the history.
- **Value:** Low
- **Effort:** S
- **Risk:** Low.
- **Touches Haskill API:** No.

---

## Test organisation summary

- vfs-storage, vfs-compose and vfs-registry have no `tests/` directory; everything is inline. Inline tests suit white-box durability tests that poke at `pub(crate)` hooks. The problem is file size: tests are about 65% of `layer.rs`, `cached.rs` and `overlay.rs`, and about 58% of `registry/layout.rs`. Moving them into sibling `tests` submodules (T8, T9, T10, T15) is the fix. They do not need to become integration tests.
- vfs-block-store already has a good `tests/` split (crash, compact, model, lifecycle and others).
- Crash-simulation helpers are copied three times (T6).
- Conformance gaps: `stored_name` and `is_immutable` (T2).
- Registry gaps: depth limits, and round trips at the name-length limits (T3).
- `vfs-zip` tests depend on Windows-only archive paths (T14).

## Suggested order of work

1. **T3** registry depth cap and tests (silent data loss, S).
2. **T1** stale docs (S, unblocks reviewers).
3. **T2** forward `stored_name`, implement it for zip, and add the conformance case (perf, S–M). Do **T21** together with it, since both need the shared merge or spelling rule.
4. **T4** marker constants into `vfs-provider::layout` (S).
5. **T7** shrink the vfs-storage public surface and drop the `durable` parameter (S, no Haskill impact).
6. **T6** crash hooks made real, a shared snapshot helper, and `test-hooks` in CI (S; tell Haskill).
7. **T5** `durable.rs` consolidation plus one canonical durability doc (M). Do it before the file splits so they move code only once.
8. **T8, T9, T10** mechanical splits of `layer.rs`, `cached.rs` and `overlay.rs`, including the `util.rs` lock helpers and replacing `MemUpper` (M each, parallelisable).
9. **T15** registry follow-ups: checksum v2, `deleted_at` bound, `layout/` split, `RealKey`/`MergedKey` merge, NTSTATUS sharing.
10. **T11, T12, T13** shared `HandleTable`, path helpers and status helpers (M; additive for Haskill).
11. **T14** vfs-zip cleanups; schedule the positional-read change with the corpus integrity test.
12. **T17** decide SeqRead's fate; then **T16, T18, T19, T20, T22** as time allows.

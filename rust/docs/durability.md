# vfs-storage durability

This is the one place the durability policy of `vfs-storage` is written down. The
code is in `crates/vfs-storage/src/durable.rs` (rendered as that module's rustdoc);
the user-facing knobs are documented where they are set, on
`StorageConfig` and `Durability` in `config.rs`. Other docs link here instead of
repeating the rules.

`Storage` keeps two stores that commit separately: the block store (pack files
plus its own index) and the catalog (a redb database). Everything below exists to
keep the pair consistent across a crash while paying for as few fsyncs as
possible.

## Terms

- **Durable point.** `BlockStore::flush()` (fsync of pack data and a durable index
  commit), then the catalog's durable commit, both under the exclusive gate, then
  the store deletes the commit made safe (the *doomed* list). A durable point that
  finds nothing non-durable (`!store.has_unflushed()` and no non-durable catalog
  commits) skips both fsyncs but still counts, still bumps the epoch and still
  deletes the doomed files. Entry point: `Storage::durable_point`; `Storage::sync`
  is "commit batched cache access times, then a durable point".
- **Non-durable commit.** Every catalog row write commits with redb
  `Durability::None`. A later durable commit publishes all earlier ones.
- **Gate.** `Storage::gate`, an `RwLock<()>`. Held **shared** around every "write
  store data, then write the catalog row that describes it" pair (layer commit,
  layer file create, a cache fetch's first block). Held **exclusive** around
  flush + durable commit (a durable point, `delete_layer`, `close`, and
  reconciliation). No row can land between a flush and the durable commit that
  would publish it ahead of its data.
- **Epoch.** `DurableClock::epoch`, bumped by every durable point (fsynced or not),
  under the exclusive gate.
- **Fresh file.** A layer file whose create saw the current epoch: its row is not
  durable yet, so its whole content is non-durable. Tracked per layer in
  `FreshFiles`; a set from an older epoch counts as empty.
- **Scratch directory.** A top-level directory of a named layer listed in
  `StorageConfig::scratch_dirs`, holding temporaries the host deletes after a crash.
  Folded once, at `Storage::open`.

## The invariants

1. Every durable catalog row references durable store data (flush before durable
   commit, both under the exclusive gate).
2. A file whose row is removed or replaced is deleted from the store only after a
   durable point has made the row's removal durable (catalog first, store second),
   and only once no handle has it open.
3. A create writes the row (non-durable) before the store file; the store file
   before any data.
4. A layer is made durable at creation, before any of its data can be written, so
   the store never holds layer data under a catalog with no durable layer (which
   `open` refuses as a lost catalog).

## Policy: trigger to action

`LayerProvider::changed` calls `Storage::after_change` after a handle `flush`, the
`close` of a handle that wrote, and every namespace change (`mkdir`, `remove`,
`rename`, a size change by `set_attr`, `put_files`). It runs with no lock held.

| Trigger | `OnEveryClose` | `Deferred { max_interval }` (default) |
|---|---|---|
| Handle `flush`, `close` of a handle that wrote, namespace change | durable point now | nothing, unless one of the rows below applies |
| Same, and the file is a **rewrite in place** (see below) | durable point now | durable point now |
| Same, and a durable point is **due** | durable point now | durable point now |
| `Storage::sync`, `Storage::close`, last provider of a layer dropped | durable point | durable point |
| Layer create, `import_layer`, `delete_layer` | durable point | durable point |
| Reconciliation at open (when it runs) | flush + durable commit | flush + durable commit |

A durable point is **due** when the last one is at least `max_interval` old
(default five minutes), or the catalog holds `StorageConfig::max_deferred_commits`
non-durable commits (default 10,000, `DEFERRED_MAX_COMMITS`; redb keeps their
bookkeeping in memory until a durable commit). Due points are *claimed*
(`DurableClock::claim_if_due` restarts the interval) so concurrent writers do not
all run one. No background thread exists: a store that stops changing stays
non-durable until the next change, `sync`, `close` or provider drop.

### The rewrite-in-place exception

Under `Deferred`, the `close`, `flush` or `set_attr` size change of a file that
**wrote** to a file whose row is already durable runs a durable point at once.
Rewriting such a file in place changes store data a durable row describes; left
non-durable, a store auto-flush mid-rewrite could publish a store state the
durable row does not match, and a crash would leave the file emptied or torn.

A file is exempt (stays deferred) when it is **fresh** (created since the last
durable point: its row is not durable, so a crash drops it whole) or in a
**scratch directory** (below). Namespace changes and file creates are never a
rewrite and stay deferred. So the cheap path is creating files, or writing a
temporary file and renaming it over the real one.

A race between the freshness check and a durable point answers "not fresh", which
only costs an extra durable point.

### The scratch-directory exception

A file under a scratch dir of its layer never makes the rewrite-in-place durable
point of its own. Without this, a host that writes many large files at once into a
temporary directory and renames them (an installer) pays a chain of durable
points: every file open across one makes another at its close, which every other
open file then spans. A scratch file's partial content after a crash is harmless
because the host deletes the directory. Every other layer, and every other
directory of that layer, keeps the rule. Scratch status is judged by the file's
current catalog path (the first component, case-folded), so a rename out of a
scratch dir stops being exempt.

### Skipping the fsyncs

A durable point with nothing non-durable skips the fsyncs. This is why an idle
`sync`, drop or close costs nothing, and why a durable point is "free" for a layer
whose files were all published by someone else's point.

### Deferred deletes (`doomed`)

A removed or replaced layer file's GUID is pushed to `Storage::doomed` (a leaf
lock) right after the row's removal committed, or, if a handle still has the file
open, when the last handle closes. The next durable point takes the list under the
exclusive gate, commits, and only then deletes the store data (and invalidates the
RAM tier). On failure of the flush or commit the GUIDs are put back. A store
delete that fails is left for reconciliation (`needs_reconcile`). GUIDs doomed
while a durable point runs wait for the next one. A rename over a file therefore
holds both versions (about twice the file) until the next durable point.

### Retry after failure

A claimed durable point that fails calls `DurableClock::retry`, which pushes the
last-point time a year into the past so the very next change finds one due.

## Clean close and the reconcile skip

Reconciliation at open (a lookup per file, slow on a large store) repairs what a
crash between the two halves' commits left. It is skipped after a clean close:

- **Close.** `Storage::close`, or the drop of the last reference (unless the thread
  is panicking, or a close was already tried: `shut`), runs `Storage::sync`, then
  the block store's `shutdown_with_token(token)` (its final durable commit, which
  covers the deletions that sync made), then the catalog's `mark_clean_close(token)`
  in one durable commit. Each step is durable before the next, so a crash anywhere
  between leaves no matching mark. The token is random and at least 2 (0 is
  "not clean", 1 is the store's plain clean shutdown).
- **No mark is left** (and the block store still shuts down cleanly, without the
  token) when the session is **dirty**: `Storage::needs_reconcile` was called
  (a store delete failed, a commit failed, a layer block or file was found
  missing or of the wrong length, a write panicked while holding the shared gate,
  a durable point panicked and poisoned the gate, corruption was found at open or
  a repair failed). If the block store's writer lock is poisoned nothing is
  attempted at all.
- **Open.** The catalog must have existed before this open, and its token and the
  block store's must be equal and present. Then reconciliation is skipped
  (`ReconcileReport::skipped_after_clean_close`). Either way the catalog's token
  is **removed, durably, as the first write** of the open, and every block store
  open overwrites the store's token at once; so a crash of this open, or anything
  else that opened the store since (any build), or a store from before tokens
  existed, makes the next open reconcile.
- A process that exits without a close (`std::process::exit` runs no destructors)
  leaves no mark and the next open reconciles, as after a crash.

## Lock order, outermost first

- Layers: a file cell's `state`, then `gate`, then the layer's `ns`, then the
  layer's leaf locks (`cells`, `handles`, `fresh`, a cell's `path` and
  `mtime_override`) and the storage's `doomed`.
- The `layers` registry, then `gate` (a new layer is made durable while the
  registry is held; nothing holding the gate takes the registry).
- Cache: `gate`, then `open_counts`, then `access`.

The gate is never taken recursively (shared or exclusive) by a thread that holds
it. The exclusive holder takes nothing else during the fsyncs: a durable point
takes `doomed` only briefly, before them, and no layer lock at all. A panic while
the shared gate is held may leave a write pair half done, so the guard marks the
session dirty (an `RwLock` reader's panic does not poison it).

## What a crash leaves

A crash (process kill as much as power loss: the catalog's non-durable commits
live only in the process) reopens the store as of its last durable point;
reconciliation then repairs the rest.

- A file created since then is gone whole (its row was never durable, so its
  store data is an orphan and is deleted). It is never visible under its name with
  part of its data.
- A removal, rename or `mkdir` since then is undone; a removed or replaced file
  comes back with its data.
- A rewrite in place of an older file was made durable when its handle closed. One
  still open at the crash can come back old, new or mixed (as under
  `OnEveryClose`).
- Under `OnEveryClose` a crash loses only writes on handles still open.
- The store's own auto-flush and compaction commits can make a store state durable
  mid-commit; reconciliation repairs those (missing blocks written as zeros past a
  durable length, rows taking the store's length, orphan ids deleted, rows without
  store files dropped). Blocks missing wholly below a durable length are
  corruption, never served as zeros. See `reconcile.rs`.

## Crash points and test hooks

- `vfs-block-store` feature `crash-points`: `crash::point(name)` aborts the
  process when `BLOCK_STORE_CRASH_AT` equals `name`. Points: `flush_before_commit`,
  `write_after_append`, `compact_before_retire`, `compact_after_retire`,
  `compact_after_copy`. Exercised by `vfs-block-store/tests/crash.rs`
  (`cargo test -p vfs-block-store --features crash-points --test crash`).
- `vfs-storage` feature `test-hooks`: `Storage::crash_on_drop_for_tests` marks the
  process "dead" (no durable point runs from then, whoever asks: provider drop,
  `sync`, or the storage's own drop; no clean-close mark) and, on Unix, copies the
  directory so the drop puts it back as it was, because redb publishes its
  non-durable commits as its database closes. On Windows only the "no durable
  point" part holds. `snapshot_as_killed(from, to)` copies a live directory as a
  kill would leave it; quiesce writers first.
- `DurableClock` test hooks (`advance`, `points`, `set_max_commits`) are
  `#[cfg(test)]`: tests count fsynced durable points and move the clock.

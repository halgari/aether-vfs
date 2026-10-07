# Shim invariants and incident notes

The hook code under `rust/crates/vfs-shim/src/hook/` keeps each invariant in two to five lines of
comment next to the code it protects. The history behind it (the traces, the measurements, the
wrong turns) lives here. Each subsection quotes the comment that used to sit in the code, as it
stood when it moved; "above" and "below" in the quoted text refer to the code it was written in.
Dates and "gate N / task M" labels are the project's own history markers.

## Close-path locking

The per-handle tables (`DIR_TABLE`, `HANDLE_PATHS`, `IDENTITY_TABLE`, `PATH_TABLE`) are
cleaned up in `close_hook_body` with `try_lock`, never `lock`. A blocking acquisition in the
close path can hang the whole process.

Those four tables are now one: `HANDLES`, a `HandleTable` of `HandleInfo` records with the
fields `dir` (was `DIR_TABLE`), `opened_as` (was `HANDLE_PATHS`, still bounded by
`HANDLE_PATHS_MAX`), `identity` (was `IDENTITY_TABLE`) and `under_root` (was `PATH_TABLE`). A
close takes one `try_lock` and drops the whole record. The names below, in the incident
narratives, are the old ones.

There is one policy for every table `NtClose` touches, the file handle tables and the registry
tables (`SYNTH`, `PASS`, `NOT_OURS`, the enumeration and notification state):
`sync::lock_for_close(&mutex, &CloseLock)`. It only ever `try_lock`s, a bounded number of times
(a `yield_now` between two attempts that found the lock held), and then gives up and returns
`None`; a poisoned lock is given up at once. It never blocks. The budget is the one thing that
differs, in two named constants:

- `CloseLock::FILE`: one attempt, silent. Every `NtClose` of every handle pays for it.
- `CloseLock::REGISTRY`: up to 10,000 attempts, each give-up counted in `hookstats`
  (`reg_close_lock_given_up`), because a lost registry record leaves a stale key handle behind.

### `close_hook_body`: `try_lock`, never `lock`

From `hook/close.rs`.

**`try_lock`, never `lock`.** This is best-effort reclamation, and a
blocking acquisition here hangs the process permanently.

Traced 2026-09-02 with `VFS_SHIM_BREADCRUMB`, three reproductions:
`threads=1`, `current=NtClose`, `mark=TABLE_HANDLE_PATHS`,
`holder=CLOSE_HOOK`, `entries - exits = 2`, zero CPU, and immune to
`TerminateProcess`. The holder is `close_hook_body` — but the only
statement it holds the guard across is a `BTreeMap<isize, String>`
remove, which cannot close a handle and so cannot be a live outer frame
re-entering. The holder is a **dead** frame.

A thread terminated while holding a `std::sync::Mutex` leaves it locked
**forever, and not poisoned** — so every `if let Ok(..)` in this crate is
no defence against it. At process exit Windows terminates every thread
but one before `DLL_PROCESS_DETACH`, and the surviving thread then closes
handles on its way out. `vfs_shim_dll`'s own `DllMain` records this exact
hazard for a different lock: "one killed mid-write leaves a lock the flush
waits on forever".

`try_lock` cannot deadlock. Losing a reclamation is harmless: the entry
is keyed by a handle value that is about to become invalid, `HANDLE_PATHS`
is bounded by `HANDLE_PATHS_MAX` against unbounded growth, and the
process is on its way out in the case that matters.

## Enumeration containment

A directory listing under a managed root is the director's `readdir`, whole and unmerged. The
real directory behind the mount never appears in one. The caller's buffer is filled after the
table lock is released.

### `serve_dir_query`, phase 2: what a listing may contain

From `hook/dirquery.rs`.

Phase 2 (unlocked): build the listing. The handle only reached
`DIR_TABLE` because `tag_under_root` found `path_is_ours` true for it,
so *every* listing built here is a listing under a managed root — and
the governing invariant says the real filesystem beneath a managed root
is unreachable by any spelling. A directory listing is a spelling. So
exactly one thing may appear in one: what the director serves. When the
director's client routes the directory, its `readdir` is the whole
answer, authoritative and unmerged.

Two other sources have been removed. Until gate 4 task 8b a branch
drained the real directory behind the mount (`drain_real` over the
handle) whenever the client was absent or did not recognise the path,
so a real, unserved file under a managed root would be listed; reads,
metadata and writes were each sealed and proven by the escape matrix,
but enumeration was only ever *argued* to follow from read-open
containment. `drain_real`, `drain_real_classic` and
`parse_full_dir_info` went with it, so no code remains that can read a
real directory into a served listing. Until task C8 a second branch
listed the shim-local write overlay's own entries; that overlay is
gone, and with it every source but the director.

A tracked directory the client does not route gets an **empty** listing,
counted `contained` in the report. It is not reached: `path_is_ours`
(which decides tracking) and `FuseClient::route` (which decides the
listing) are the same question, `vpath_under_root`. It is kept because a
branch that would otherwise fail *open* is exactly the one worth having
fail closed, and those two predicates *have* drifted apart before, for
five spellings at once, when they were two different `RootMap`s.

### `serve_dir_query`: whiteout markers in the director's listing

From `hook/dirquery.rs`.

**Gate 5, Task 7 — the phantom whiteout marker,
closed.** This branch used to hand the director's
answer to the game verbatim, and the director's
answer carries the shim's own markers: it mounts the
directory the shim-local overlay wrote into as its
write layer (`overlay_layer_dir`) and spells
whiteouts `.wh.<name>`, not `<name>.__vfs_wh__`, so
the shim's came back as ordinary files. That showed
the game a phantom `<file>.__vfs_wh__` entry *and*
left the file it names listed.

**Before the wildcard filter, not after** — see
`hook/whiteout.rs::strip_whiteout_markers`, which
also records why the fix is here rather than in a
shared spelling.

The shim writes no markers since task C8 (its overlay
is gone, and a handle-based delete under a root asks
the director, like a path-based one). The filter
stays: write layers already on users' disks hold
markers older shims wrote.

### `serve_dir_query`: what the whiteout fix does not cover

From `hook/dirquery.rs`.

**Enumeration only.** A marker still does not hide its
target from an `open` through the director:
`OverlayProvider::hidden_by_whiteout` looks for its own
`.wh.` spelling, and there is no per-open hook here that
could ask without a `stat` on every read. Only markers
older shims left behind are affected.

(This section used to list a second gap: `setinfo_hook`'s
engine branch, which minted new markers under a live
director for a handle-based delete on a real under-root
handle. Task C8 removed it; that delete goes to the
director.)

### `serve_dir_query`, phase 3: fill the caller's buffer outside the lock

From `hook/dirquery.rs`.

Phase 3 (locked): store the built listing (if rebuilt) and serve a slice.

**The caller's buffer is filled after the guard is released, never under
it.** `write_dir_info` writes into a scratch buffer we own; the copy into
`info` happens below, unlocked.

That ordering is the fix for the intermittent hang traced on 2026-09-02.
`info` belongs to the caller and may lie inside one of our own
demand-paged regions, so touching it can fault into `lazy_section`, which
does file I/O, whose `NtClose` re-enters the shim and takes
`DIR_TABLE.lock()` again. `std::sync::Mutex` is not reentrant, so that
second acquisition blocked forever on a lock the same thread already
held: zero CPU, one thread, and immune to `TerminateProcess`. Measured
three times with `VFS_SHIM_BREADCRUMB` — `threads=1`, `entries - exits =
2`, `mark=TABLES`.

A scratch buffer rather than cloning the entries: the copy is bounded by
`length`, whereas a directory listing is unbounded.

## Sealed root: opens

Under a managed root the director's answer is the caller's answer. Nothing is excepted by
file name, and nothing falls through to the real file system.

### `try_fuse_create`: the DRM/identity basename exceptions

From `hook/file_open.rs`.

**Gate 5, Task 4 — the DRM/identity exceptions, closed.** Four basenames
(`steam_appid.txt`, `SkyrimSELauncher.exe`, `steam_api{,64}.dll`,
`SkyrimSE.exe`) used to be matched here, case-insensitively at any depth,
and returned `None` *before the ring was consulted* — sending the open on
to the shim-local engine (removed by task C8), which either redirected it
at a real disk path or passed it straight through to the real filesystem
under the managed root.
That was the last route by which a path under a managed root reached
something other than the director.

The reason recorded for keeping them was "serving `SkyrimSE.exe` through
FUSE produced a Steam Error, caused by an open that fails to resolve
(FUSE-relative `OBJECT_ATTRIBUTES` reaching the kernel)". That reason was
self-cancelling: the unresolvable OA only ever arose on the *excepted*
arm, which is the one that has to hand the kernel an OA whose root is a
synthetic handle (see `tramp_create_abs`). With the exception gone the
kernel is never called for these names at all — the open either gets a
synthetic handle from the director or is sealed.

Two things the deleted comment got right and are worth keeping: Steam
does **not** compare the in-memory image against the on-disk PE (measured
— the whole loaded image was once overwritten with zip PE bytes at a
relocated base and DRM still verified), and what actually needs the
on-disk exe is outside this hook: `CreateProcess` of the host image, and
Steam's own path association from a separate, un-injected process.

`OpenOutcome::FellThroughDrmException` is deliberately kept in the enum
and in the report reading **zero**: a removed counter cannot prove the
class stayed closed, and the shim/director reconciliation asserts on it.

The tracer stays wired for the live acceptance run — it is off unless
`VFS_DRM_EXE_LOG` names a file, and it now sees the opens it never could
before, since these names finally arrive here.

### `try_fuse_create`: every under-root open is the director's

From `hook/file_open.rs`.

Every under-root open — read *and* write — goes through the
director (zip / composed / writable layer), and every answer it gives,
including the failures, is this function's answer too: since gate 4's
Task 5 no *decision* below returns `None`, except behind the explicit
`allow_disk_fallthrough` opt-out.

A poisoned synth table used to be the one `None` that was not a
decision: `open_fuse_at_ex(...)?` on the success path gave up its handle,
which sent the caller to the real call after the director had already
opened the file, and leaked that `fh`. Since task C8's fix round it fails
closed instead: `STATUS_UNSUCCESSFUL`, and the director's `fh` is closed.
`try_fuse_mkdir`'s two `open_fuse` calls do the same.

It is still not a live route — but not for the reason this comment used
to give. It claimed the crate builds with `panic = "abort"`; it does
not. `rust/Cargo.toml` sets `panic = "unwind"` for both profiles,
deliberately, so "a panic cannot unwind here" is simply false and
nothing about poisoning is ruled out by the profile. What rules it out
instead is that nothing inside those critical sections can unwind.
`synth_file` holds `TABLE`/`NEXT` across `usize` arithmetic, `BTreeMap`
insert/get/get_mut/remove keyed by `usize`, and `String` clone/drop --
no `unwrap`, no slice indexing, no caller-supplied closure, no `Ord` or
`Drop` impl that can panic. Allocation failure aborts rather than
unwinding. Poisoning requires a panic to unwind *out of a held guard*,
and there is no panic here to unwind.

Note that `contain_panic` does NOT make this safe by itself: it catches
a panic at the hook boundary, but the guard's drop has already set the
poison flag by then, so later calls would see it. Re-check the argument
above if `synth_file` ever grows a fallible or reentrant operation
under those locks.

## Sealed root: statuses

A path the director does not serve is sealed, for reads and writes. The statuses that go with it
are chosen by cause, so callers' idioms (open-for-write then create, delete then create) keep working.

### `try_fuse_create`: a path the director does not have is sealed

From `hook/file_open.rs`.

Not in director: seal the path, for reads and writes alike. The
*only* way out of this arm without a status is the explicit
`VFS_ALLOW_DISK_FALLTHROUGH` opt-out, which unseals the root
wholesale (see `allow_disk_fallthrough`) and is off by default and
cleared defensively by `skyrim-live`. Behind it an under-root miss
passes through to the real file, for reads and writes; before task C8
it went to the shim-local engine instead, which denied a read and sent
a write to its own overlay.

**Gate 4, Task 5 — this is the write fall-through, closed.** A write
used to return `None` here unconditionally, which sent
`create_hook`/`open_hook` on to the shim-local engine (removed by task
C8): an overlay redirect where one was configured, and a plain
pass-through to the real filesystem *under the managed root* where one
was not.
Both spellings put content the provider graph never saw somewhere the
director cannot account for; the pass-through one physically creates a
file under a root whose whole contract is that the real filesystem
beneath it is unreachable.

### `try_fuse_create`: the two meanings of `ST_NOT_FOUND` on a write open

From `hook/file_open.rs`.

Two different failures wear `ST_NOT_FOUND` on a write open,
and NT distinguishes them, so this must too:

- The open asked to **create** (any of SUPERSEDE / CREATE /
  OPEN_IF / OVERWRITE_IF set `OPEN_CREATE`) and the director
  still said not-found: no writable provider is mounted
  anywhere over this path. The name is not what is missing —
  the caller was going to supply it — so this is
  `STATUS_OBJECT_PATH_NOT_FOUND` (`ERROR_PATH_NOT_FOUND`),
  the same answer NTFS gives for a create whose containing
  directory does not exist.
- The open did **not** ask to create (FILE_OPEN /
  FILE_OVERWRITE with write access — "open the existing
  file for writing"). Then the file itself is simply absent
  and the honest answer is the ordinary
  `STATUS_OBJECT_NAME_NOT_FOUND` (`ERROR_FILE_NOT_FOUND`) —
  the same one the read seal below returns. Answering
  PATH_NOT_FOUND here would mislead the very common
  "open-for-write, and on ERROR_FILE_NOT_FOUND create it"
  idiom into thinking the directory was gone.

### `try_fuse_create`: other director errors on a write

From `hook/file_open.rs`.

Any other director error on a write. **Gate 4, Task 5:** this used to
return `None` — "the director rejects OPEN_WRITE, so let the write
land in the shim-local overlay instead" — which is the second half of
the fall-through this task closes.

Deliberately *not* merged with the `ST_NOT_FOUND` arm above. That one
is a path no provider serves; this one is a provider that served the
path and then refused or failed the write, and the two want different
answers at the NT boundary:

- `ST_READ_ONLY` is the director's own policy status, meaning "no
  `ReadWrite` provider serves this path" (`Director::open`, which
  also records it for `vfs stats` discovery). That is a permission
  fact, not a fault, and `STATUS_ACCESS_DENIED` is what a real
  read-only filesystem answers — a status callers already have code
  for, unlike `STATUS_UNSUCCESSFUL`'s `ERROR_GEN_FAILURE`.
- `ST_IS_DIR` means the path is a directory and the caller asked to
  create or replace a file over it (`OverlayProvider::open_for_write`
  refuses rather than letting a `DiskProvider` upper create a file
  named after the directory). The two non-creating dispositions
  never get here — `dir_open_downgrades` already turned them into
  the directory open the caller meant — so what is left genuinely
  is a file create aimed at a directory, and NT has a status that
  says exactly that.
- Anything else (I/O error, bad request, a provider that broke) is a
  genuine failure: `STATUS_UNSUCCESSFUL`, matching the read-side
  `Err(_)` arm below, which likewise refuses to fall through.

Note there is no `allow_disk_fallthrough` escape here, again matching
the read side: that switch relaxes "the director does not have this",
never "the director failed".

## Sealed root: deletes and renames

Deletes and renames follow the opens: both sides under one root and routed, or no root at all and
trampolined. Anything in between is refused.

### `delete_hook_body`: the path-based delete

From `hook/file_mutate.rs`.

`NtDeleteFile` hook — the **path-based** delete (gate 5, Task 5).

**This was the one unhooked NT API that reached real disk.** The others on
this project's list all take a handle, so leaving one unhooked fails safely:
under a managed root the caller is holding a synthetic handle, the kernel
does not own it, and the call comes back `STATUS_INVALID_HANDLE`. This one
takes only an `OBJECT_ATTRIBUTES`. There is no handle to be wrong about, so
an unhooked call resolves the path itself and unlinks the real file sitting
under the root — which is the exact thing the root's contract says is
unreachable.

**The decision is made on the path, like `create_hook`/`open_hook`, and by
the same machinery.** `path_of_tracked` decodes the `OBJECT_ATTRIBUTES`
(including a handle-relative name, and including the FUSE-synthetic
`RootDirectory` a virtual directory handle produces), and the question asked
of that path is the one the open hooks ask:
`FuseClient::route` — the director's notion of the root. If it claims the
path, the director's answer is the caller's answer, both ways: `OP_DELETE`
accepted is `STATUS_SUCCESS`, `OP_DELETE` refused is a failure the caller
sees. It never continues to the kernel from here, for the same reason
`try_fuse_create` does not: a refusal that falls through is not a refusal.
`setinfo_hook` gives a handle-based delete of the same path the same answer
(`director_delete_or_rename`). Until task C8 there were two more steps,
the shim-local engine's whiteout and a `path_is_ours` backstop
(`STATUS_ACCESS_DENIED`) for the paths it declined; with the engine gone
`path_is_ours` is the client's own question, so a path the first step does
not claim is one the backstop would not claim either.

Outside every root the call is trampolined unchanged, which is most of them
— a hook with an opinion about every delete in the process would break the
rest of it.

### `close_hook_body`: `FILE_DELETE_ON_CLOSE` on a synthetic handle

From `hook/close.rs` and `hook/file_open.rs`.

**A third delete route, added by task C8.** Wine's `DeleteFileW` (kernelbase) is not a
disposition set-info: it is `NtCreateFile` with `FILE_DELETE_ON_CLOSE` followed by `NtClose`.
The director has no delete-on-close of its own, so the shim keeps it: `route_open` records the
flag on the synthetic handle it returns (`synth_file::set_delete_on_close`), and
`close_hook_body` sends `OP_DELETE` for the handle's path — the path it is named by when it
closes, so after a rename through it, the new one — after releasing the `fh`, as NT deletes at
the last close. Until then every Win32 delete of a director-served file under Proton reported
success and deleted nothing.

- **A refusal cannot be reported.** `NtClose` has no way to say the delete failed, and
  `DeleteFileW` returns TRUE either way, so a refusal (a read-only layer, a provider error, a
  director that is gone) is counted with its path (`hookstats::note_delete_on_close_refused`,
  the report's "FILE_DELETE_ON_CLOSE deletes the director refused" section, and the trace). It
  is not refused up front with `STATUS_CANNOT_DELETE` as NT would: the open carries `DELETE`
  access, which is not a write open, and nothing in a read open's answer says whether the path
  is deletable, so knowing would cost a round trip on every such open.
- **No second delete.** A set-info delete that succeeds through a handle that also has the flag
  clears it, so the close does not send `OP_DELETE` again — which could delete a file recreated
  under the same name in between.
- **Not honoured on a directory `try_fuse_mkdir` created.** That handle is synthesised without a
  path recorded on it (`open_fuse(0, 0, true)`) and the flag is never set, so a directory created
  with `FILE_DELETE_ON_CLOSE` survives its close. Not seen in practice; a known gap.

### `delete_status_for`: mapping a director refusal

From `hook/file_mutate.rs`.

The NT status for a director `OP_DELETE` refusal.

**Flattening every refusal to `STATUS_UNSUCCESSFUL` is not neutral.** That
maps to `ERROR_GEN_FAILURE`, and the delete-then-create idiom — the single
most common thing callers do with a delete — treats only
`ERROR_FILE_NOT_FOUND` as benign and gives up on anything else. So a delete
of a path the director simply does not have would stop callers that a real
filesystem lets straight through. The open path already distinguishes these
(`try_fuse_create`'s `Err` arms); this is the same mapping for the same
reason, kept as one function so the two cannot drift:

- `ST_NOT_FOUND` -> `STATUS_OBJECT_NAME_NOT_FOUND` (`ERROR_FILE_NOT_FOUND`).
  Nothing to delete is not a failure to delete.
- `ST_READ_ONLY` -> `STATUS_ACCESS_DENIED`. The director's own policy status
  for "no `ReadWrite` provider serves this path", and `ERROR_ACCESS_DENIED`
  is what a real read-only filesystem answers a `DeleteFileW`.
- `ST_IS_DIR` -> `STATUS_FILE_IS_A_DIRECTORY`, which `RtlNtStatusToDosError`
  folds to `ERROR_ACCESS_DENIED` — exactly what `DeleteFileW` returns when
  the name is a directory.
- Anything else (I/O error, a provider that broke) is a genuine failure and
  keeps `STATUS_UNSUCCESSFUL`.

### `setinfo_source_path`: finding the path of a handle

From `hook/file_mutate.rs`.

The NT path a handle-based delete/rename should act on, and whether finding
it required consulting the OS about the handle's *current* target (the same
provenance bit [`DecodedPath`] carries, for the same reason: a
`RootMap`-backed answer computed from it must not be cached under it).

**The third source is what closes an escape.** `PATH_TABLE` is populated by
`record_path`, which runs only on an open *this shim intercepted*. A handle
inherited across `CreateProcess`, duplicated in from another process, or
opened before injection is in no table of ours — and `setinfo_hook`'s
non-synthetic branch used to read that miss as "nothing to do" and hand the
call to the real `NtSetInformationFile`. For a **delete** that unlinked the
real file under a managed root, which is the same breach `delete_hook`
exists to prevent, arriving by a different door. There is no cheap
backstop for it either: a `PATH_TABLE` miss leaves no path at all, so there
is nothing to apply `path_is_ours` to until one is recovered.

So ask the OS, exactly as `parent_dir_of_handle`'s case 4 already does for
`OBJECT_ATTRIBUTES.RootDirectory` — `GetFinalPathNameByHandleW` needs no
reopen, since the caller is handing us a handle it currently holds.

**The order is correctness, not only cost.** `PATH_TABLE` holds the path the
caller *named*. While the shim-local engine existed (until task C8), a handle
that came off its `Decision::Redirect` arm was named by its virtual path but
targeted the overlay copy, and asking the OS first would have handed back a
location under no managed root, so the operation went to the kernel. No
handle is redirected any more, but the recorded name is still the one the
caller used, so it wins wherever there is one:

1. `PATH_TABLE` — an intercepted open whose path was under a managed root.
2. `HANDLE_PATHS` — every other intercepted open. A handle here but not in
   (1) is one `record_path` declined, i.e. outside every root, so this
   answers the common "delete a file that is none of our business" case
   without touching the OS.
3. `GetFinalPathNameByHandleW`. Only reached for a handle the shim never saw
   opened, and only on a delete/rename set-info, which is rare — this is not
   a per-call cost on any hot path.


### `setinfo_hook_body`: what sits on top of the handle routes

From `hook/file_mutate.rs`.

`NtSetInformationFile` hook. For a synthetic (director-served) handle it
routes truncate (`FileEndOfFileInformation`), delete and rename over the ring.
A real handle whose path is under a root (opened before injection, inherited,
duplicated in, or a fall-through open) gets the same delete and rename answer
from the same function (`director_delete_or_rename`), and the real operation
never runs, so the real file is preserved while the path reads as gone or
moved. Until task C8 that real-handle branch asked the shim-local engine
instead, which wrote a whiteout marker into the director's own upper without
the director hearing about the delete.

Two things sit on top of that, both from gate 5's Task 5, and each has its
own comment at the check itself:

- **The source is resolved even when no table knows the handle**
  (`setinfo_source_path`). A `PATH_TABLE` miss used to mean "not ours", and
  for an inherited or pre-injection handle on an under-root path that sent
  a delete to the real file.
- **A refusal keyed on the *target*, not the source.** A rename whose
  destination lands under a managed root is refused even when the source is
  outside every one of them and no source-keyed arm ever looked at it.

Between them the rule is one sentence: a rename either has both sides under
the same root, and is routed, or it touches no root at all, and passes
through. Everything else is refused, and nothing under a root reaches the
kernel.

### Hard links (`FileLinkInformation`, `FileLinkInformationEx`)

From `hook/file_mutate.rs` (`refuse_link_touching_root`; the synthetic-handle arm in
`setinfo_hook_body`). The link classes are laid out exactly as the rename classes
(`ReplaceIfExists`/`Flags` at 0, `RootDirectory` at 8, name length at 16, name at 20), so the target
is read with `parse_rename_target`. The director protocol has no link operation, so a link has no
route: it either touches no root and passes through, or it is refused.

- A synthetic handle as the source: refused, `STATUS_ACCESS_DENIED`.
- A real handle whose resolved path is under a root (`setinfo_source_path`, which asks the OS for a
  handle no table knows): refused.
- A target under a root, from a source outside every root: refused. This is the write escape: the
  kernel would otherwise create a real file under a root that seals everything the graph does not
  serve.
- Both ends outside every root, or an unparseable target against a source outside every root:
  passes to the trampoline.

Each refusal is counted (`hookstats::link_refused_count`).

### `setinfo_hook_body`: a rename to a different root

From `hook/file_mutate.rs`.

A rename whose target lands under a *different*
root is refused rather than guessed at: the
wire carries one root for both sides, and the
provider contract has no cross-root move.

It does **not** fall through — an earlier
version of this comment claimed it did, and the
shim-local engine's `rename` was written to
match that description, which is how its branch
ended up handing cross-root moves to the real
filesystem. What actually happens is
`STATUS_UNSUCCESSFUL`, the same as any other
refused delete/rename, for both kinds of
handle.

### `setinfo_hook_body`: the source is under a root

From `hook/file_mutate.rs`.

**The source is under a managed root, so the director answers.**
`tramp` would hand the operation to the kernel, which acts on the real
file, and three kinds of operation used to end that way through the
shim-local engine's branch:

 - A **delete** the engine declined (no overlay configured, or a
   path resolving with an empty remainder, the root itself).
 - A **rename out** of a managed root to a target outside every
   one of them, which *unlinks a real file under a managed root*.
   That the destination is legitimately outside does not make the
   source side any less of a breach.
 - A **rename whose target cannot be parsed at all**
   (`parse_rename_target` -> `None`, e.g. a target named against
   a directory handle we cannot resolve).

Since task C8 all three go to `director_delete_or_rename`, which
answers every operation on a routed source: the delete is the
director's, and a rename whose target is not under the same root is
refused with `STATUS_UNSUCCESSFUL`. The rule this leaves is one
sentence: a rename either has both sides under the same root, and is
routed, or it does not touch a root at all, and is trampolined.
Everything between is refused.

### `setinfo_hook_body`: a rename whose target lands under a root

From `hook/file_mutate.rs`.

**A rename whose *target* lands under a managed root** (gate 5,
Task 5). Everything above is keyed on the *source*, and for a source
outside every root none of it runs: `record_path` inserts into
`PATH_TABLE` only when `path_is_ours(path)`, so an outside handle is
never recorded, the source-keyed arm above is skipped, and `tramp` below
performed the move — physically creating a file under the
destination root, where it then read back as missing because that
root seals every path the provider graph does not serve.

The destination is what decides containment. Content crossing *into*
the VFS by a route the director never saw is the same failure as
content crossing out of it, and the source being legitimately
outside does not make the target's root any less managed.

Refused rather than routed, and there is no third option available:
`OP_RENAME` carries **one** root and two vpaths under it (see
`FuseClient::rename`), so the provider contract has no operation for
an import from outside. `STATUS_ACCESS_DENIED` rather than the
`STATUS_UNSUCCESSFUL` the cross-root arm above returns, because it is
a different answer: cross-root is "the graph cannot express this
move", this is "the destination will not accept content by this
route at all".

NOTE: `parse_rename_target` discards `parent_dir_of_handle`'s
OS-consulted provenance bit, so a target named against a directory
handle the shim never saw opened reaches `path_is_ours` here without
an `UncachedScope`. That is the known gap `parse_rename_target`
already records, not a new one — it is listed there rather than fixed
here so both callers are fixed at once.

## Panic containment

Every `extern "system"` entry point runs its body inside `contain_panic`. A panic becomes a
failure status and a log record instead of a dead game process, and the reentrancy counter is
only ever raised by a guard whose `Drop` lowers it.

### `ShimIoGuard`: the only way to raise the reentrancy counter

From `hook/entry.rs`.

RAII form of [`HOOK_REENTER`] for shim-initiated file I/O. `enter()` returns
`None` when the guard is already held on this thread — the caller's signal
that it is already running *inside* the shim's own I/O and must not start
more. While it is held, every NT file call this thread makes takes
`create_hook`/`open_hook`'s `in_hook_reenter` fast path straight to the real
ntdll — which is the point: the shim's own file I/O (the stats report, the
panic log, the diagnostic traces) must reach the real filesystem rather than
be re-decided by the hooks it runs inside. (The shim-local copy-up, which
wrote its destination file from inside the hook that asked for it, was the
first user; task C8 removed it.)

**This is the only way to raise the counter, and that is deliberate rather
than tidy.** There used to be a `hook_reenter_begin`/`hook_reenter_end`
pair as well, and three in-module callers used it raw: `install_panic_hook`,
`drm_exe_trace` and `director_open_trace`. Each of those does file I/O
between the two calls, and a panic anywhere in that span skipped the `end`.
That failure is permanent and completely silent: the counter stays at 1 for
the life of the thread, so every later hook call from it takes the
`in_hook_reenter` fast path to real ntdll, and the process quietly stops
being virtualized on that thread while every counter keeps reporting
ordinary activity. Now that `hook::contain_panic` catches panics instead of
letting them abort the process, that "later" actually exists — so the pair
is gone and the counter can only be raised by a value whose `Drop` lowers
it, unwinding included.

### `STATUS_HOOK_PANICKED`: why a generic failure

From `hook/entry.rs`.

What every hook returns when [`contain_panic`] catches a panic in its body.

**The choice that matters is that it is a failure**, with the NTSTATUS
severity bits set, so no caller can mistake it for a completed operation. A
hook that panicked half way through has written nothing to the caller's
output buffer and stored nothing in its `*mut HANDLE`; answering
`STATUS_SUCCESS` would hand the game an uninitialised handle value and a
buffer of stack garbage to parse, which is materially worse than the abort
this replaces — a crash at least stops at the fault.

It is `STATUS_UNSUCCESSFUL` and not something more specific for the opposite
reason. A panic means the shim does not know what happened, and every
*specific* status is a claim it is not entitled to make: returning
`STATUS_OBJECT_NAME_NOT_FOUND` tells the game the file does not exist, and
Skyrim will happily bake that into a load order and carry on without the
plugin; `STATUS_ACCESS_DENIED` invites a retry loop; `STATUS_NO_MORE_FILES`
from an enumeration hook silently truncates a directory listing. Generic
failure is the only answer that says "this operation did not happen" without
also asserting why.

It is uniform across every ntdll entry point because a panic is the
same event in each of them, and because a per-hook table of "best" statuses
would be one more chance per hook to pick one that a caller treats as benign.
`cpiw_hook` is the one exception and is not an exception to the principle:
`CreateProcessInternalW` returns a Win32 `BOOL`, where this constant's bit
pattern is *non-zero* and therefore reads as success. It returns `FALSE`
instead — see the `on_panic` column of the `CreateProcessInternalW` row in `detour_table!`.

Note that several hooks already return this same status when their
trampoline is missing, so the value alone does not distinguish a panic from
that. The distinguishing record is `hookstats::note_hook_panic` plus the
shim panic log, both of which a panic writes and a missing trampoline does
not.

### `contain_panic`: what the wrapper changes

From `hook/entry.rs`.

Run one hook body with its panic contained at the `extern "system"`
boundary, and report `on_panic`'s value to the caller if it faults.

**What this changes, and what it does not.** Measured on this toolchain
(2026-08-16, `rustc` with `panic = "unwind"`): a panic inside an
`extern "system"` fn runs the `Drop` impls of every Rust frame below the
boundary, in order, and *then* hits rustc's forced
`core::panicking::panic_cannot_unwind` and takes the process down with
`0xC0000409`. Adding this wrapper does not change which destructors run —
the same unwind runs the same ones — it changes only where the unwind stops
and what happens there. The unwind never reached the game's frames before
(the forced abort is at *our* boundary, not theirs) and still does not; what
the game gets now is a returned status instead of a dead process.

That the inner destructors run is a requirement here, not a tolerated cost:
[`ShimIoGuard`]'s `Drop` is what releases this thread's reentrancy counter,
and a panic that skipped it would leave every later hook call on that thread
falling through to real ntdll — a silent un-virtualization far harder to
diagnose than a crash.

# `AssertUnwindSafe`

Every hook body captures raw pointers from the NT call and touches process
statics, so none of these closures is `UnwindSafe` and the assertion is
unavoidable. It is also true here, for a reason narrower than the general
case: `UnwindSafe` guards against *observing* state that a caught unwind
left half-updated, and this function observes nothing. On the `Err` arm it
reads nothing out of the closure, touches none of the captured pointers, and
returns a constant. Every process-wide table this crate shares between hooks
(`DIR_TABLE`, `IDENTITY_TABLE`, `PATH_TABLE`, `HANDLE_PATHS`, and
`hookstats`' accumulators) is behind a `std::sync::Mutex`, which poisons on
a panic taken while held, and every lock site in this crate already treats
`Err` as "no entry". So a later call cannot read a torn value out of one; it
reads nothing, which is the same thing it does on any other lock failure.

The honest consequence of that, which the abort did not have because there
was no "later": a panic taken while one of those tables is locked poisons it
for the rest of the process, and the handle tracking it backs degrades to
permanently empty. That is a real loss of fidelity and it is why the caught
panic is counted loudly rather than swallowed.

# Visibility

`pub` because the shim's `extern "system"` surface is not confined to this
crate: `vfs-shim-dll` owns `DllMain` and `vfs_shim_sync_bootstrap`, which are
entry points Windows and the OEP stub call, and which must contain their
panics for the same reason and by the same route. One containment function for
the whole injected DLL is what lets
`no_extern_hook_bypasses_the_panic_containment_macro` check both crates
against a single marker.

### `install_panic_hook`: the log is the only record of a contained panic

From `hook/entry.rs`.

Record shim panics — which no longer take the game down, and that is the
change that matters most about this comment.

**A panic in a hook used to end the process, and does not any more.** The
history is worth keeping because two successive versions of this comment
were wrong about why. The first claimed the workspace builds with
`panic = "abort"`; it does not — `rust/Cargo.toml` sets `panic = "unwind"`
for both profiles, deliberately, so a future binding can turn a panic into
a host-language exception. The second, correct as far as it went, was that
the process died anyway because every hook is `extern "system"` and rustc
plants a forced abort wherever an unwind would cross that boundary:
measured, a panic inside such a function printed `thread caused
non-unwinding panic. aborting.` and exited **0xC0000409**
(`FAST_FAIL_FATAL_APP_EXIT`). That is no longer what happens, because
[`contain_panic`] now catches at each boundary and returns
[`STATUS_HOOK_PANICKED`] instead. The old behaviour is still one edit away
and reproducible on demand — deleting the `catch_unwind` makes
`a_panicking_hook_returns_a_failure_status_instead_of_aborting` kill the
*test process* with exactly that code.

So this hook's job changed from "attribute the crash" to "be the only
record there was a fault at all". It matters more now, not less: a
contained panic is invisible from outside the process — the game gets a
failed file operation and carries on — and this log is the only place the
message, location and thread survive. (`hookstats`' caught-panic counters
are the aggregate, but they only reach a reader if `VFS_SHIM_STATS_LOG` is
set.) The 0xC0000409 attribution still matters for the panics that *do*
abort — a panic inside this hook, or in code the containment does not
cover — where the exit is otherwise an unattributable
`STATUS_STACK_BUFFER_OVERRUN`, indistinguishable from a genuine
stack-cookie or CFG failure in the game, and localisable only by bisecting
(see the 0xC0000409 hunt behind commit 5f8f2eb).

`set_hook`'s hook runs at panic time, before any unwinding begins, so the
message is written whether the unwind is later caught or aborts. A logged
message therefore does **not** imply the process died, and now for two
reasons rather than one: a panic on the stats reporter thread kills only
that thread and never reaches an `extern` boundary at all, and a panic in a
hook is caught at that boundary and answered with a status.
Writes to `VFS_SHIM_PANIC_LOG`, else `<state dir>/shim-panic.log`, else a
fixed fallback — a panic here must never be silent for want of a path.

## Name-query consistency

`GetFinalPathNameByHandleW` builds its answer from `NtQueryObject` and two classes of
`NtQueryInformationFile`, and treats them as describing one file. The hooks answer all of them or none.

### `qif_hook_body`: class 9 is spoofed with classes 1 and 48

From `hook/file_info.rs`.

`NtQueryInformationFile` hook. Spoofs the two name classes —
`FileNameInformation` (9) and `FileNormalizedNameInformation` (48) — on a
redirected handle -> the virtual path, so `GetFinalPathNameByHandleW`
reports where the mod file appears to live. Everything else passes through.

# Why class 9 is spoofed too, having once been documented as unspoofable

This comment used to read "spoofing class 9 breaks
`GetFinalPathNameByHandleW`", and that was a true measurement of the shim as
it then stood — but the cause was consistency, not class 9 itself.
`GetFinalPathNameByHandleW` builds its answer from three sources and treats
them as describing one file:

1. `NtQueryObject(ObjectNameInformation)` — the full NT name;
2. `NtQueryInformationFile(FileNameInformation)` (class 9) — used for its
   **length only**: the device prefix is taken to be
   `ObjectName[.. ObjectName.len - class9.len]`;
3. `NtQueryInformationFile(FileNormalizedNameInformation)` (class 48) —
   appended to the drive letter that prefix maps to.

Spoof any one of those and the subtraction in (2) slices at the wrong
offset. Measured 2026-09-01 with class 1 spoofed and class 9 left truthful:
ObjectName `\Device\HarddiskVolume3\vfstmp\vfs-diag\mod.esp` (53 chars)
minus the backing file's class 9 `\vfstmp\vfs-diag-backing\backing_blob.dat`
(47 chars) gave a 6-character "device" of `\Devic`, which maps to no drive,
so the call failed with `ERROR_FILE_NOT_FOUND`.

The rule is therefore **all three or none**: classes 1, 9 and 48 must
describe the same path. They now do, and the subtraction lands on the real
device prefix again because both operands moved by the same amount.

### `qobj_hook_body`: the prefix convention, discovered

From `hook/file_info.rs`.

`NtQueryObject` hook. Answers `ObjectNameInformation` (class 1) for a handle
the shim redirected -> the VIRTUAL path, in the prefix convention this host
actually uses. Every other class, and every handle we do not track, passes
through untouched: this API answers about events, mutexes, sections and
registry keys too, and inventing a name for one of those would break
unrelated Windows APIs.

Why the convention is discovered rather than assumed: measured 2026-09-01,
Windows returns `\Device\HarddiskVolume3\...` while Wine returns `\??\C:\...`,
and `QueryDosDeviceW("C:")` reports `\Device\HarddiskVolume1` on Wine — it
disagrees with Wine's own `NtQueryObject`. So building a device path from it
would emit a form Wine never produces. Instead the trampoline runs first and
its answer's prefix is reused.

**This closes a pre-existing leak on Windows, not only on Wine.**
`GetFinalPathNameByHandleW` happens to route through
`NtQueryInformationFile(FileNormalizedNameInformation)` on Windows — hooked
by [`qif_hook_body`] — so the leak hid there; any caller reaching
`NtQueryObject` directly got the backing path, silently. Wine routes
`GetFinalPathNameByHandleW` through this entry point instead, which is how
the leak became visible at all.


## Lock semantics

`NtLockFile` on a synthetic handle is granted without being held. This is a known gap, accepted
because the alternative was unreadable INI files.

### `lock_hook_body`: a lock that is granted but not held

From `hook/file_io.rs`.

`NtLockFile` hook — grants byte-range locks on synthetic handles locally.

**Why this exists.** A synthetic handle is a tagged value in `synth_file`'s
table, not a kernel file object, so any NT call without a detour hands that
value to the real kernel and gets `STATUS_INVALID_HANDLE` back. Measured
2026-08-14: `GetPrivateProfileStringW` — how Skyrim loads `SkyrimPrefs.ini`
— issues `NtOpenFile → NtLockFile → NtQueryInformationFile → NtReadFile →
NtUnlockFile → NtClose`, and with `NtLockFile` unhooked the sequence
stopped dead at step 2. The API then returned the *caller's default* for
every key, so the game received no INI data at all — not stale data, not
real-disk data. `WritePrivateProfileStringW` failed the same way one
operation earlier. Neither showed up as a read or write at the director;
both showed up as an open and nothing else.

**The deliberate semantic gap.** This grants a lock that does not exist.
Nothing is recorded, nothing conflicts, and two callers asking for the same
exclusive byte range both get `STATUS_SUCCESS`. That is chosen, not
overlooked:

- Inside a sealed managed root the director is the only route to the bytes,
  and there is no cross-process byte-range locking anywhere in the design
  today — so there is no lock table for a real answer to consult.
- Refusing instead (`STATUS_LOCK_NOT_GRANTED`) would leave the profile APIs
  exactly as broken as an unhooked call did; it swaps a wrong status for a
  different wrong status.

**Do not read that as "there is only one writer".** There is not, by
design: `cpiw_hook` propagates injection into child processes, so a
launcher and a game — or a game and a mod manager's helper — are routinely
in one session. And the API that exposed this bug is the worst case for a
fake lock: `WritePrivateProfileString` is a read-modify-write, and the lock
it takes here is exactly what stops two of those from losing each other's
updates. Two injected writers on one INI will both be granted the same
exclusive range and one update will disappear.

That is a real hole, not a theoretical one; it is accepted because the
alternative on offer was every INI staying unreadable, not because it is
harmless. Closing it needs a byte-range table in the director — the only
component both processes share. Until then
`hookstats::note_synthetic_lock` counts every grant by path, so the
contention shows up in a report instead of only in corrupted settings.

**Which handles this answers.** Only ones [`open_synth`] resolves. The
bit-47 tag test alone would also catch `INVALID_HANDLE_VALUE` and any
closed or never-issued synthetic handle, and answering `STATUS_SUCCESS` for
those would report a lock held on a file the caller never opened.

**Completion.** Answered synchronously: `STATUS_SUCCESS`, a completed
`IO_STATUS_BLOCK`, and `SetEvent` if the caller supplied one — the same
shape `read_hook` uses, including its one limitation, that we do not run
the caller's APC. That limitation is counted rather than assumed away:
`note_read_completion` classifies every synthetic lock by the completion
its caller expected, so an APC-supplied lock — the shape that would wait
forever on a callback we never make — shows up in the report's async
section instead of passing for an ordinary grant. `FailImmediately` needs
no branch: `false` means the caller is willing to block for the lock, and
an immediate grant satisfies that strictly better than waiting.

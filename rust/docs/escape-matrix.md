# Escape matrix: Gate 2

**Provenance.** `crates/vfs-directord/tests/e2e.rs`'s
`escape_matrix_positive_and_negative_canary` runs `vfs-fixture-escape.exe`
under a real, composed session — daemon, director, injected shim, the
works, not a standalone/uninjected run — against two targets. It is part of
`cargo test --workspace` and reproduces on every run; the specific paths and
PIDs below are from one concrete run on the development machine
(2026-08-14) and will differ elsewhere, but the outcome per vector should
not.

**Two targets, mirrored by construction:**

- **Positive canary** — `escape-positive-canary.esp`, identical bytes
  written both into the `DiskProvider`'s backing directory (`content_dir`,
  a plain tempdir mounted at `/`) and physically onto the session's real,
  on-disk managed root (`session.root`). Whichever mechanism actually
  serves a given spelling — FUSE-routed (the director, reading
  `content_dir`) or real-disk passthrough (reading `session.root`
  directly) — the bytes are the same either way. `vfs-fixture-escape` reads
  every successful open back and compares it against a baseline read of
  the literal path, failing closed to `error:content-mismatch:...` on any
  difference, so `opened` below means the *same file's real bytes*, not
  merely "some handle came back".
- **Negative canary** — `escape-negative-canary.bin`, a real file
  physically on `session.root`, absent from `content_dir`. No provider
  serves it. This is not simulated; the `DiskProvider` genuinely does not
  have this filename anywhere in its backing directory.

## What this matrix does and does not establish

**Does establish:** for the positive canary, that every buildable spelling
opens the *correct* bytes under a session — the containment test cannot be
passed by breaking access. For the negative canary, that every buildable
spelling is **classified** — it lands in a counted bucket of the shim's own
`under-root open outcomes` report (`routed`, one of the `fell-through: *`
labels, or `denied`) — rather than being invisible to every counter
(`outside-root`).

**Does not establish:** that the negative canary is unreachable. It is
still reachable, for two independent, already-known reasons this gate does
not touch: (1) the passthrough fall-through this gate's own scope note
requires to stay (`NotFound`-under-root → passthrough; gate 3 owns removing
it), and (2) the shim's read-seal policy (`ST_NOT_FOUND` from the director
→ `STATUS_OBJECT_NAME_NOT_FOUND`, unless `VFS_ALLOW_DISK_FALLTHROUGH` is
set, which it is not here) blocks some spellings' *read* while still
correctly *classifying* them. A "classified" row below is not a "blocked"
row and must not be read as one. **Classification, not containment, is
what this matrix is evidence about.**

**Classification signal.** `support::classified_paths` parses the shim's
`VFS_SHIM_STATS_LOG` report and unions every path listed under any of the
seven `under-root open outcomes` buckets. For the negative canary, each
buildable vector is re-run **in isolation** (`VFS_ESCAPE_ONLY_VECTOR`, every
other vector skipped entirely, not merely omitted from the output) so that
vector's own classified-paths set can only reflect its own attempt — see
"A methodology defect found and fixed" below for why a shared, combined-run
set is not trustworthy for this.

## The matrix

`opened✓` = opened, byte-identical (positive canary only — see above).
`classified✓` = present in the isolated classified-paths set (negative
canary only). `unbuildable` rows carry the fixture's own reason and are
machine/environment-dependent, not a fixed property of the vector.

| # | Vector | Positive canary | Negative canary | Note |
|---|---|---|---|---|
| 1 | 8.3 short name (`GetShortPathNameW` + open) | opened✓ | `unbuildable: GetShortPathNameW failed: win32:2` | See "Vector 1's negative-canary unbuildable reason" below — a real quirk of attribute-query fall-through, not a canonicaliser defect. Buildable on this machine for the positive canary because the target genuinely exists there. |
| 2 | Extended-length prefix (`\\?\C:\...`) | opened✓ | classified✓ | |
| 3 | NT device path (`\\?\GLOBALROOT\Device\HarddiskVolumeN\...`) | opened✓ | classified✓ | **Found broken, then fixed — see "A real bypass found and fixed" below.** Was invisible to every counter before the fix in this same task. |
| 4 | Volume-GUID path (`\\?\Volume{guid}\...`) | opened✓ | classified✓ | |
| 5 | Handle-relative open (`OBJECT_ATTRIBUTES.RootDirectory` = a real directory handle) | opened✓ | classified✓ | |
| 5b | Handle-relative open against a handle `GetFinalPathNameByHandleW` cannot resolve (an anonymous pipe) | `error:ntstatus:0xC0000033` | not classified (by design) | **Caveat, not a failure — see "Vector 5's caveat" below.** `path_of_tracked` cannot decode a path at all for this shape, so it lands in the shim's separate "undecodable" counter, never in `under-root open outcomes`. Documented, accepted edge of Task 4's fix, not asserted as pass/fail either way. |
| 6 | CWD-relative (plain filename, cwd set to the parent dir) | opened✓ | classified✓ | |
| 7 | Junction / reparse point | opened✓ | classified✓ | **Closed — see "Vectors 7 and 9 closed: session-start alias resolution" below.** Was a verified, open gate-2 gap; fixed by resolving a junction that reparses into the managed root into a `VolumeMap` alias once per session. |
| 8 | Hardlink (new filename, same underlying bytes) | `not-found` (not `opened` — see "Vector 8's exception" below) | classified✓ | Sealed by the content-addressed provider policy, not a classification failure — `RootMap`/the canonicaliser is never even consulted for this vector when FUSE-routing claims the path first. |
| 9 | UNC / `subst` / mapped drive (administrative loopback share, `\\localhost\C$\...`) | opened✓ | classified✓ | **Closed — see "Vectors 7 and 9 closed: session-start alias resolution" below.** Was a verified, open gate-2 gap; fixed by registering the admin-share's real NT spelling as a session-start alias. |
| 10a | Case-flipped, `\\?\`-prefixed (verbatim) | opened✓ | not classified (`not-found`, unbuildable-adjacent) | NTFS resolves case regardless of the `\\?\` prefix; standalone-`opened` behaviour, unaffected by session or gate 2. Not asserted for the negative canary (an ordinary FUSE-routed/sealed case, no special canonicaliser involvement — see the fixture's own note). |
| 10b | Trailing dot, verbatim (`...\name.esp.`) | opened✓ (flip from standalone `not-found` — see "The 10/12 flip" below) | classified✓ | |
| 10c | Trailing space, verbatim (`...\name.esp `) | opened✓ (flip) | classified✓ | |
| 11 | Alternate data stream (`name.esp:probe`) | `not-found` (expected — see note) | classified✓ | Read-only `OPEN_EXISTING` against a stream this fixture never pre-creates; `not-found` means the stream doesn't exist, not that streams are unsupported. Same result standalone and under a session. |
| 12a | `.` component, verbatim | opened✓ (flip) | classified✓ | |
| 12b | `..` traversal through a non-existent intermediate name, verbatim | opened✓ (flip) | classified✓ | |
| 12c | Doubled separator, verbatim | opened✓ (flip) | classified✓ | Standalone this reports `error:win32:123` (`ERROR_INVALID_NAME`); under a session it opens. |
| 13 | Handle opened before the managed root was registered | opened (reported, not closed) | not-found (reported, not closed) | **Reported, not closed in this gate — gate 3's job.** This test cannot construct the real "handle predates root registration" timing; it only re-confirms ordinary reachability from within an already-injected process. See "Vectors 13 and 14" below. |
| 14 | Child process launched without the shim injected | opened (reported, not closed) | `error:cmd-exit:1` (reported, not closed) | **Reported, not closed in this gate — may not be a shim fix at all.** By construction, this vector's own child process has no hook to intercept anything; its classification (or lack of it) is not evidence about gate 2 either way. See "Vectors 13 and 14" below. |

All fourteen vectors (nineteen lines, counting 5b/10a-c/12a-c) were
buildable on this machine except vector 1's negative-canary line, which is
`unbuildable` for a reason specific to this run's attribute-query
fall-through (see below), not to 8.3 name generation being disabled (it is
enabled here — the positive canary's vector 1 built and opened normally).

## A real bypass found and fixed: vector 3's `GLOBALROOT` wrapper

Building this matrix's negative-canary classification check found that
vector 3 (`\\?\GLOBALROOT\Device\HarddiskVolumeN\...`) opened the real file
successfully but **never appeared anywhere in the shim's own classified-paths
set** — invisible to every counter, exactly the failure mode this gate
exists to close.

Root cause: Windows presents this Win32 spelling to `NtCreateFile` as
`\??\GLOBALROOT\Device\HarddiskVolumeN\...` — `GLOBALROOT` is a real
object-manager symlink to the namespace root (`\`), so this names *exactly*
the same object as the bare `\Device\HarddiskVolumeN\...` form. But
`RootMap::compute_under_root`'s `VolumeMap` lookup only ever matches a
registered device/volume-GUID prefix **at the very start** of the string,
so the `GLOBALROOT` token in front hid the device prefix from it entirely.
The path canonicalised as an unrecognised, non-drive-rooted string;
`path_is_ours` answered false; `tramp` still found the real file (nothing
about *reachability* changed) while `note_passthrough_outcome` never fired.

Fixed in `crates/vfs-redirect/src/canon.rs`
(`strip_globalroot_wrapper`/`resolve_device_prefix_with_globalroot`): a
`GLOBALROOT`-wrapped device or volume-GUID prefix is tried as a fallback
after the bare form, so an ordinary (unwrapped) path resolves exactly as it
always did and a wrapped one now resolves identically to its bare
equivalent. Six new unit tests in that file cover the fix directly
(case-insensitivity, both NT/DOS prefix spellings, the unmapped-device
fail-closed case, and a guard that the fallback never engages for a path
that already matched the bare form). Verified via reproduction: before the
fix, `3` was absent from the negative canary's classified set; after, it
appears as its own distinct entry.

## Vectors 7 and 9 closed: session-start alias resolution

A prior task verified these as real, open gate-2 gaps: both resolve to the
real bytes via a spelling that is **syntactically unrelated** to the managed
root — a completely different directory tree for the junction, a
`UNC\localhost\C$\...` form for the admin share — and contains no `~`, so
`RootMap::compute_under_root`'s `~`-gated OS-consult branch never fires for
either. Widening that gate to catch them was explicitly rejected as the
fix: the gate exists so an ordinary out-of-root open (the overwhelming
majority of a real game's I/O — `System32`, driver, CRT paths) never pays a
Win32 round trip, and both vectors are common enough in real installs
(mod-manager staging junctions, administrative shares) that they need a
correct answer on every session, not a rare, expensive one on every open.

**The fix extends the existing session-start pattern instead**, the same
one the device/volume-GUID table already uses: resolve each alias once,
into the same `VolumeMap`, rather than consulting the OS per open.

- **Vector 9 (UNC admin share).** `vfs-redirect::volumes::resolve_volume_map`
  now registers `\??\UNC\localhost\<drive>$` — the real NT spelling Windows
  presents to `NtCreateFile` for a `\\localhost\<drive>$\...` Win32 open,
  via the `\??\UNC` object-manager symlink to `\Device\Mup` — as an alias
  for `<drive>:`, for every drive `vfs_win::drive_mappings` already
  enumerates. **A sibling of the exact trap vector 3's `GLOBALROOT` fix
  found** (a map keyed with the `\\?\` spelling instead of the `\??\` a real
  open actually presents, matching nothing and failing *closed*) was caught
  before it could repeat here — a dedicated unit test
  (`unc_admin_share_alias_does_not_match_the_win32_spelling`) asserts the
  `\\?\`-spelled key does *not* match, alongside one asserting the `\??\`
  key does.
- **Vector 7 (junction).** `resolve_volume_map` also walks the managed
  root's own ancestor chain (root's parent, that directory's parent, and so
  on up to the drive root), doing one non-recursive directory listing per
  level. Any entry other than the chain node itself that is a directory
  reparse point, and whose resolved target's canonical form has the root's
  own components as a prefix, is registered as an alias from its own
  location to that target — so a path spelled through the junction
  canonicalises to the identical root-rooted form by ordinary string
  comparison, no OS consult needed per open. See
  `vfs-redirect/src/volumes.rs`'s `junction_aliases` doc comment for the
  full mechanism.

**Scope of the junction scan, chosen deliberately.** Scanning an entire
volume for reparse points at session start is too slow. Scanning the
managed root's own ancestor chain (not a recursive walk of the root's own,
potentially huge, content subtree) is the right general shape, but even
that chain is climbed only **two levels** (`MAX_ANCESTOR_LEVELS` in
`junction_aliases`) — not all the way to the drive root. Two levels is
exactly `vfs-directord::registry`'s own `<TEMP>/vfs-daemon-<pid>-<seq>-<id>/root`
convention: one level past the per-session base directory, one more past
the system temp directory itself, and no further into the broader user
profile tree. This was not a guess — climbing further was tried first and
measured to cost real time on an ordinary, long-used Windows profile:
`C:\Users\<name>\AppData\Local` alone carries a handful of Windows' own
built-in legacy-compatibility junctions (`Application Data`, `History`,
`Temporary Internet Files`, and on this development machine an
application-specific one besides), and `C:\Users\<name>` itself carries a
dozen more (`Cookies`, `SendTo`, `Start Menu`, ...). Reading even just their
on-disk metadata (never their targets — see the hang below) added enough
latency to the session's *first* redirect decision to occasionally miss the
shim's own stats reporter's tick window in a short-lived test process — a
real, observed classification miss during this task's own verification, not
a hypothetical. A real game session has no such tight timing window, but
paying the extra cost and the extra exposure to unrelated system junctions
buys nothing beyond the fixed, two-level convention this project's sessions
already use.

Two things this scan deliberately does **not** do, to stay on the safe side
of the "must not pull anything in" requirement:

1. **An ancestor being a reparse point itself is never aliased**, even
   though it looks tempting (e.g. `C:\Games` symlinked to a Steam library at
   `D:\Library\Games`, with root spelled `C:\Games\Skyrim`) — `RootMap`'s own
   root components are always root's *literal* spelling, never resolved
   through any junction, so aliasing one of root's own ancestors would
   rewrite **every ordinary in-root open** (which necessarily starts with
   that same ancestor's literal path) away from matching root's own
   registered components. That would be an active regression breaking
   legitimate traffic, not merely a missed vector — worse than either named
   vector left open. Every alias this scan actually registers is a genuine
   *sibling*, disjoint from root's own ancestor chain by construction, so
   this invariant holds automatically. Verified directly by
   `root_itself_being_a_reparse_point_is_never_aliased`.
2. **The managed root's own subtree is not recursively scanned** for a
   reparse point pointing *out* of the root (a Mod-Organizer-style staging
   junction, `root\Data\SomeMod` -> `D:\Mods\SomeMod`). Under the same
   "target must resolve inside root" discipline the sibling scan uses, a
   root-subtree junction only ever produces a redundant no-op (its target is
   already inside root, so both spellings already canonicalise correctly on
   their own) or would require admitting a genuinely external, unrelated
   directory into the managed root — exactly the over-eager failure class
   this project has already hit twice (the `subst`-hijack and
   `GLOBALROOT`-wrapper regressions). Left as a documented non-goal for a
   future gate to evaluate explicitly, not a side effect of closing vector 7.

Both directions were tested explicitly, not just the closing direction: a
junction whose target is a genuinely unrelated directory (outside the
managed root entirely) registers nothing
(`junction_pointing_outside_root_is_not_aliased`), and root itself being a
reparse point registers nothing keyed to root's own path
(`root_itself_being_a_reparse_point_is_never_aliased`).

**Staleness, same limit the device/volume-GUID table already carries**: a
junction created, retargeted, or removed after the alias scan runs is not
reflected until the session is rebuilt — the identical limitation already
documented for `subst`. Also documented, deliberately narrow: only the
`localhost` hostname spelling of the admin share is aliased; the machine's
own NetBIOS/DNS name and loopback address forms (`\\127.0.0.1\C$\...`,
`\\[::1]\C$\...`) resolve to the same object but are not — closing every
hostname spelling that could ever resolve to "this machine" is unbounded,
for marginal benefit over the one spelling this gate's own escape matrix
exercises.

**A real timing defect found and fixed while verifying this.** The obvious
place to resolve these aliases is `vfs-shim::Engine::build` — called once
from the shim's DLL bootstrap, which seemed like "session start". It is not
early enough: the injector's own bootstrap sequence guarantees hooks are
live *before* the target process's own `main()` runs, which means a
junction the injected process's *own later code* creates (exactly what
`vfs-fixture-escape`'s vector 7 does — `mklink /J` immediately before its
own attempted open, all inside the same already-injected process) does not
exist yet at bootstrap time. Building the alias table eagerly in `build()`
therefore saw an empty ancestor scan every time, even though the mechanism
itself was correct — verified by isolated reproduction before concluding
the fix worked, not assumed from unit tests passing. Fixed by deferring the
volume-aware `RootMap` construction from `Engine::build` to the engine's
first real decision (`Engine::map`, memoized in a `OnceLock` for the rest of
the session) — still exactly one resolution per session, at the same cost
as building it eagerly, just triggered by first use rather than by DLL
load. For a real game, bootstrap-time and first-decision-time are the same
instant for all practical purposes (both occur before the game does
anything with the filesystem, and any junction a mod manager set up already
existed long before the game process even started); the two moments only
ever diverge for this project's own synthetic fixture construction. This
does not reintroduce a per-open OS consult: it is one deferred, memoized
resolution, not a repeated one.

**A second reentrancy bug found and fixed, structurally identical to an
existing one.** Deferring resolution to the engine's first real decision
introduced a *new* re-entrancy hazard, the same shape as the
`OS_CONSULT_DEPTH` bug documented below but in a different crate:
`junction_aliases`'s directory scan makes real Win32 calls
(`vfs_win::reparse_point_target`'s `CreateFileW`, directory listings) from
inside the very first hooked call ever made in the process. Since that call
is itself intercepted by the same injected process's hooks, it re-enters
`vfs-shim::Engine::map` on the same thread, before the first call's
`OnceLock::get_or_init` closure has returned — `std::sync::Once` documents
that shape as unspecified behaviour ("a panic or a deadlock"), observed
here as an unresponsive process burning CPU rather than a clean crash.
Fixed with `vfs-shim::engine`'s own thread-local guard (`MAP_INIT_DEPTH` /
`MapInitGuard`): a re-entrant call answers `None` ("not ready yet") instead
of touching the still-initializing `RootMap` again, and every caller
already had a fail-safe `PassThrough`/`false`/`None` path for exactly this
shape of answer.

**A hang found and fixed: resolving a reparse point's target must never
follow it.** The first version of the junction scan resolved a candidate's
target with `vfs_win::final_path_for_open` (open-and-ask-Windows, the same
helper the 8.3-short-name path already used) — which opens, and therefore
*follows*, whatever the candidate names. Walking a real Windows profile's
ancestor chain encounters real, pre-existing junctions with no relation to
this project — Windows' own legacy-compatibility redirects, and on this
development machine an application-specific one pointing at another drive
— and following one whose target is offline, disconnected, or merely slow
to answer blocks on the OS's own device/network timeout, tens of seconds,
once per such junction. This hung the escape-matrix fixture outright.
Fixed by reading the reparse point's own on-disk substitute-name data
directly (`vfs_win::reparse_point_target`, `FSCTL_GET_REPARSE_POINT` on a
handle opened with `FILE_FLAG_OPEN_REPARSE_POINT` — opens the reparse point
itself, never what it names) rather than opening a handle that follows it.
Proven by a dedicated test that deletes the junction's target *after*
creating it but *before* reading it: `final_path_for_open` would now fail
outright (nothing left to open); `reparse_point_target` still succeeds,
because it never depended on the target existing at all.

**A correctness/performance bug found and fixed: a cheap pre-filter is not
optional.** A directory reparse-point check still needs *some* per-entry
work, and the first working version of the ancestor scan did that work with
`std::fs::symlink_metadata` — a separate, real, hooked query — on *every*
entry in the scanned directory, not just the reparse points among them. An
ordinary ancestor directory on a real, long-used machine (an unremarkable
`%TEMP%`) can hold thousands of entries, so this multiplied the shim's own
`NtCreateFile` call count by thousands for no reason. This was not merely
slow: an unrelated write-path e2e test's own shim/director open-count
reconciliation check (a drift assertion — every open one side records must
show up on the other) started failing, catching a real side effect this
scan's own noise caused, not a bypass in the write path itself. Fixed by
pre-filtering with `std::fs::DirEntry::file_type()` (`FileTypeExt::is_symlink_dir`)
first — data the directory enumeration itself already returned, no extra
syscall — and only calling `reparse_point_target` for entries that pass it.

**A related timing defect in the test harness, found and fixed at its
source.** `hookstats::start_reporter`'s own doc comment already states that
nothing flushes its report on process exit — a process that exits before
the reporter's first periodic tick produces no report at all. The escape
matrix's own `vfs-fixture-escape` already accounted for this with an
end-of-run wait (`interval_ms * 2`), but `VFS_SHIM_STATS_INTERVAL_MS=5`
made that a 10ms wait — under Windows' default ~15.6ms system timer
resolution, where `Sleep(10)` is not reliably "wakes at 10ms", only "wakes
no earlier than 10ms, next tick or later". An **isolated** single-vector
run (`VFS_ESCAPE_ONLY_VECTOR`) is exactly the case with no margin to spare:
the selected vector's own decision is the only real file activity in the
whole process, so the process's total lifetime is short enough that this
occasionally raced process exit, an intermittent, any-vector classification
miss unrelated to canonicalisation itself — reproduced directly, not
hypothesised. Fixed by flooring the fixture's end-of-run wait at 20ms,
comfortably clearing that granularity regardless of the configured
interval.

**A methodology fix, not a production change: vector 7's junction is now
created by the test harness, not the fixture.** `vfs-fixture-escape`'s own
`mklink /J` spawn (needed to construct its test junction) is itself real,
hooked file activity inside the already-injected fixture process — and
since `RootMap`'s volume/junction table is now resolved on the session's
*first* such activity, a fixture that spawns `mklink` before its own
intended open can trigger that first resolution before the junction it is
about to create exists, which is exactly the ordering bug the lazy-resolve
fix above exists to close, one level deeper. The fix is not to make
resolution re-run (that reintroduces a per-open cost) but to remove the
ordering question: the e2e test now creates vector 7's junction itself,
before launching the fixture process at all (`VFS_ESCAPE_VECTOR7_LINK_DIR`,
registered in `vfs-env`), exactly as it already does for the positive and
negative canary files. This is indistinguishable, from the shim's
perspective, from a junction a real mod manager already had in place
before the game process started — which is the only shape this project
ever claims to close. `vfs-fixture-escape` still falls back to constructing
its own junction when that variable is unset, for a standalone
(non-session) reproduction.

## A methodology defect found and fixed, before it could hide the above

An earlier version of this test's negative-canary check ran all nineteen
lines in one combined launch and asked "does *any* entry in the shared
classified-paths set contain this vector's marker filename?" That check
passed for vectors 7 and 9 too — riding on other vectors' (2, 5, 6, ...)
own, unrelated classified entries that happen to share the same trailing
filename. Re-running each vector **alone** (every other vector skipped
entirely via `VFS_ESCAPE_ONLY_VECTOR`, not merely left out of the output)
is what actually distinguishes "this vector's own attempt was classified"
from "some other vector's was, and this one is riding along for free" — and
is what surfaced that vectors 7 and 9 do not classify at all. This is
exactly the shape of defect the project's own history warns about: "an
earlier version of this fixture had two vectors that silently probed
nothing and would have reported both closed." Caught here before this
document was written, not after.

Two further contamination sources had to be closed before isolation was
trustworthy: `vfs-fixture-escape`'s own pre-existing target-existence check
(`std::fs::metadata`, which opens a real handle on Windows, not a
lighter attributes-only query) and this task's own byte-identity baseline
read both perform an ordinary, always-classifiable open of the bare target
path — unconditionally, regardless of which vector was selected. Both are
now skipped when `VFS_ESCAPE_ONLY_VECTOR` is set, so an isolated run's
classified-paths set reflects only the selected vector's own effect.

## A crash found and fixed: the OS-consult reentrancy bug

Before any of the above, the very first attempt to run this matrix under a
session crashed the injected process with `STATUS_STACK_OVERFLOW`
(`-1073741571`) on vector 1 (8.3 short name) against the **positive**
canary (never observed against the negative canary, which never reaches
the same code path — see below for why).

Root cause: `RootMap::compute_under_root`'s OS-consult branch calls
`expand_short_name` → `vfs_win::final_path_for_open`, which opens its own
`CreateFileW` handle on the candidate path. Inside an injected process
whose own `NtCreateFile`/`CreateFileW` are hooked, that nested call
re-enters the exact same decision path (`create_hook` → `decision_for` →
`RootMap::under_root` → `compute_under_root`) for the identical `~`-bearing
path, hits the same OS-consult branch again, and recurses without bound.
None of `vfs-redirect`'s own unit tests can see this: a plain test process
has no hook on `CreateFileW` for the recursion to loop through, so this
was only observable from inside a real, injected session — precisely what
this task is the first to build.

Fixed with a thread-local reentrancy guard scoped to the OS-consult branch
alone (`OS_CONSULT_DEPTH` / `OsConsultGuard` in
`crates/vfs-redirect/src/lib.rs`): a re-entrant call finds the guard held
and answers "not recognised here" instead of recursing, which is not a
wrong answer — the nested `CreateFileW` call's own hook invocation then
takes the fall-through path and reaches the *real*, unhooked
`NtCreateFile`, so `final_path_for_open`'s handle-open still succeeds
against the real filesystem exactly as it would without the nested detour.

## Vector 5's caveat

`5b` (handle-relative open against an anonymous pipe, standing in for a
`RootDirectory` handle `GetFinalPathNameByHandleW` cannot resolve) is a
caveat on vector 5, not a second pass of it or a failure in its own right.
`path_of_tracked` cannot decode any path at all for this shape — it lands
in the shim's "undecodable opens" counter, never in `under-root open
outcomes` — which is Task 4's own documented, accepted edge: a
handle-relative open only classifies correctly when the root handle can be
resolved to a real path. This test does not assert a pass/fail outcome for
`5b` either way; it is recorded so a reader never mistakes vector 5's own
(correctly classifying) line for covering this edge too.

## Vector 8's exception

The positive canary's vector 8 (hardlink) reports `not-found`, not
`opened` — the one buildable vector where the positive-canary assertion is
not simply "opened". A hardlink necessarily creates a **new filename**
under root that the content-addressed provider has never heard of. The
shim's own (pre-existing, gate-2-independent) `fuse_client::vpath_under_root`
matcher recognises the hardlink's path as an ordinary, unmangled spelling
and routes it to the director first; the director correctly answers "no
such name" (`ST_NOT_FOUND`); with disk-fallthrough at its secure default
(off — `VFS_ALLOW_DISK_FALLTHROUGH` unset), that answer is sealed rather
than falling through to the real, hardlinked bytes still sitting on disk.
`RootMap`/the canonicaliser this gate is about is never even consulted for
this vector, because FUSE-routing claims the path before decision_for runs
at all. Not a canonicalisation defect — an inherent property of naming the
same bytes under a name the content model has never seen, orthogonal to
this gate. (For the negative canary, this same mechanism still correctly
*classifies* the hardlink's open as `Routed`, which is all that gate 2's
own exit criterion requires there.)

## Vector 1's negative-canary `unbuildable` reason

`vector1_short_name`'s first step, `GetShortPathNameW`, itself performs a
real, hooked query against the target. For the negative canary specifically,
this query reports the target as not found (an attribute-query fall-through
quirk distinct from the open-path fall-through this gate is about — attribute
queries and opens are separate hooks with separate fall-through handling),
so `GetShortPathNameW` itself fails and the vector reports
`unbuildable:GetShortPathNameW failed: win32:2` rather than ever attempting
an open. This is a real, reproducible machine/scenario fact, not a
canonicalisation failure — no open was attempted for this line, so there is
nothing to classify. It is unrelated to whether 8.3 short-name generation is
enabled on the volume (it is, here — the positive canary's vector 1 built
and opened normally against the identical volume).

## The 10/12 flip, and what it is actually evidence of

Vectors `10b`/`10c` (trailing dot/space) and `12a`/`12b` report `not-found`
standalone (no session) and `opened` under a session — the flip the design
brief calls out as informative. It is real and reproducible, but this
matrix's own investigation found it is **not** attributable to gate 2's
`RootMap` canonicaliser specifically. All four route via FUSE (`try_fuse_create`
returns `Some(...)`, recorded as `Routed`, never reaching `decision_for`/
`RootMap` at all): the shim's pre-existing, gate-2-independent
`fuse_client::vpath_under_root` matcher does simple prefix-stripping and
happily passes the messy remainder through to the director, and the
director's own **pre-existing** (`vfs-director/src/path.rs::normalize`,
predates this branch) lexical `.`/`..` collapsing, plus ordinary
(non-verbatim) Win32 file access in the *director's own, uninjected*
process tolerating trailing dots/spaces the same way any ordinary
`CreateFileW` call always has, do the actual work. The flip is real,
reproducible, system-level evidence that these four spellings resolve
correctly end-to-end under a session — it is not, on inspection, evidence
specific to the new canonicaliser this gate added. `10a` (case fold) and
`12c` (doubled separator) do not flip via this mechanism either way: `10a`
opens standalone already (NTFS's own case-insensitivity), and `12c` is
reported as observed (`error:win32:123` standalone) rather than asserted in
advance.

## Vectors 13 and 14: reported, not closed in this gate

Per the plan's own scope note, neither vector is asserted a pass/fail
outcome here:

- **13** (a handle opened before the managed root was registered) needs a
  timing this test cannot construct — a session where a handle exists
  *before* `RootMap` is even built. This run's line only re-confirms
  ordinary reachability from inside an already-injected, already-registered
  session, which is the substrate the real scenario builds on, not a
  reproduction of the timing itself. Closing it is gate 3's job.
- **14** (a child process launched without the shim injected) reads the
  real filesystem directly by construction — there is no hook in that
  process to intercept anything, so its outcome (`opened` for the positive
  canary, `error:cmd-exit:1` for the negative one, since `cmd /C type`
  exits non-zero when the target doesn't exist) is not evidence about gate
  2 in either direction. Whether this is even a shim-layer fix at all is an
  open question for a later gate.

## Verification (vector 3 / `GLOBALROOT` closeout)

- `cargo build --all-targets`, `cargo build --manifest-path
  crates/vfs-payload/Cargo.toml --target-dir target`: clean.
- `cargo test --workspace`: 447 passed, 0 failed, 0 ignored — at or above
  the stated 438 baseline (the increase is this task's own new unit tests:
  six in `vfs-redirect::canon` for the `GLOBALROOT` fix, three in
  `vfs-directord`'s `support` module for `classified_paths`, plus the
  escape-matrix e2e test itself).
- `cargo clippy --all-targets -- -D warnings`: clean.
- No bypass removed: `Decision::Redirect`, `Decision::Serve`, the DRM
  exceptions, the passthrough, and the write fall-through are all still
  present and unmodified by this task. The one behavioural change this task
  makes (`vfs-redirect`'s `GLOBALROOT` fix) only affects **classification**
  — whether an already-reachable open is counted — never whether it is
  reachable.

## Verification (vectors 7 / 9 closeout)

- `cargo build --all-targets`: clean.
- `cargo test --workspace`: 459 passed, 0 failed, 0 ignored, run twice back
  to back for stability — at or above the stated 448 baseline (the increase
  is this task's own new unit tests: `insert_alias`/UNC-alias coverage in
  `vfs-redirect::canon`, junction/UNC/reentrancy coverage in
  `vfs-redirect::volumes`, and `reparse_point_target` coverage in
  `vfs-win::volumes`).
- `escape_matrix_positive_and_negative_canary` re-run five times back to
  back post-fix with no failures (previously flaky mid-investigation runs,
  documented above, are not this number).
- `cargo clippy --all-targets -- -D warnings`: clean.
- No bypass removed: `Decision::Redirect`, `Decision::Serve`, the DRM
  exceptions, the passthrough, and the write fall-through are all still
  present and unmodified by this task. `vfs-shim::Engine::map`'s lazy
  resolution and its reentrancy guard change *when* and *how safely* the
  volume/junction table is built, never what a decision does once it has
  one.
- Both failure directions tested explicitly for vector 7, not just the
  closing one: `junction_pointing_outside_root_is_not_aliased` (a junction
  whose target is outside the root registers nothing) and
  `root_itself_being_a_reparse_point_is_never_aliased` (root's own ancestor
  chain is never an alias key, regardless of whether it is itself a reparse
  point).

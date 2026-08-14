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
| 7 | Junction / reparse point | opened✓ | **not classified** | **Verified, open gate-2 gap — see "Two vectors verified NOT to classify" below.** Reachable (real bytes, correctly, for the positive canary) but genuinely invisible to every counter for the negative canary. |
| 8 | Hardlink (new filename, same underlying bytes) | `not-found` (not `opened` — see "Vector 8's exception" below) | classified✓ | Sealed by the content-addressed provider policy, not a classification failure — `RootMap`/the canonicaliser is never even consulted for this vector when FUSE-routing claims the path first. |
| 9 | UNC / `subst` / mapped drive (administrative loopback share, `\\localhost\C$\...`) | opened✓ | **not classified** | **Verified, open gate-2 gap — see "Two vectors verified NOT to classify" below.** Same shape of gap as vector 7. |
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

## Two vectors verified NOT to classify: 7 (junction) and 9 (UNC)

Unlike vector 3, these are **not fixed here** — they are a real, currently
open gap in gate 2's own exit criterion ("`RootMap::under_root` recognises
every buildable spelling"), verified by isolated reproduction and reported
rather than patched, because closing them correctly is a design trade-off,
not a one-line fix.

Both resolve to the real bytes via a spelling that is **syntactically
unrelated** to the managed root — a completely different directory tree for
the junction, a `UNC\localhost\C$\...` form for the admin share — and
contains no `~`. `RootMap::compute_under_root`'s only path to recognising a
syntactically unrelated spelling is its OS-consult branch
(`expand_short_name`, via `GetFinalPathNameByHandleW`/`GetLongPathNameW`),
and that branch is gated on the presence of `~` for cost: without the gate,
every ordinary out-of-root open (the overwhelming majority of a real game's
I/O — `System32`, driver, CRT paths) would pay a Win32 round trip. Neither
a junction target nor a loopback UNC share necessarily contains `~`, so
neither ever reaches that branch. `path_is_ours` answers false for both;
`tramp` still finds the real file (reachability, again, is unaffected);
`note_passthrough_outcome` never fires.

**This is squarely gate 2's own scope**, not a downstream gate's — unlike
vector 8's provider-sealing (a different layer entirely) or vectors 13/14's
own documented deferrals. Closing it needs the OS-consult trigger condition
widened past the `~` gate without reintroducing the per-open cost that gate
exists to avoid for the common case. That is a real design call — plausible
directions include triggering the OS-consult whenever the syntactic form
doesn't match *any* known shape (device/volume-GUID/8.3/plain-drive) rather
than only when it contains `~`, or restricting the widened check to reads
that already have a real handle available (Task 4's flow) rather than the
pure-string `canonicalise` path — but it is not this task's call to make
unilaterally, and is not attempted here. Verified via isolated per-vector
reruns (`VFS_ESCAPE_ONLY_VECTOR=7` / `=9`), each showing an empty classified
set for that vector specifically, not merely absent from a shared/combined
one (see the methodology note below for why that distinction matters).

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

## Verification

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

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

**Does not establish:** that the negative canary is unreachable **by a
read**. At the time this paragraph was written (gate 2), it was still
reachable for two independent reasons this gate did not touch: (1) the
passthrough fall-through this gate's own scope note required to stay
(`NotFound`-under-root → passthrough; gate 3 owned removing it), and (2) the
shim's read-seal policy (`ST_NOT_FOUND` from the director →
`STATUS_OBJECT_NAME_NOT_FOUND`, unless `VFS_ALLOW_DISK_FALLTHROUGH` is set,
which it is not here) blocks some spellings' *read* while still correctly
*classifying* them. A "classified" row below is not a "blocked" row and
must not be read as one. **Classification, not containment, is what this
matrix is evidence about.**

**Update, Gate 3, Task 5.** Reason (1) above is now closed for reads:
`RootMap::decide` no longer passes `NotFound`/`Dir` through, so every
buildable vector's negative-canary *read* is now sealed — see "Gate 3, Task
5" below. This does **not** make the negative canary unreachable outright.
A **write** open (`FILE_OPEN`/`FILE_OPEN_IF` against the negative canary,
which physically exists on `session.root`) still reached it at the time this
paragraph was written: `Engine::cow_seed`'s last-resort branch copied real
on-disk bytes into the overlay whenever neither `Redirect` nor `Serve`
applied — which then included `Deny`, exactly as it had included
`PassThrough` before that task — so the negative canary became readable
through the overlay once opened for write. That was gate 4's write path, not
touched by gate 3.

**Superseded by gate 4, Task 5** (see "Gate 4 note" and the write matrix
below): that branch is deleted, `cow_seed` now reads only through the
director, and a write open on the negative canary comes back `not-found`
with no bytes and no stray file on the real filesystem. So the read/write
split this paragraph set up no longer holds: an "unreachable"/"sealed"/
"closed" claim in this document is about **reads and writes**, and — since
Task 8b — about directory enumeration too. What it is still silent on is a
name-based attribute query, a different hook family with no path to
`decide` (see the metadata-query correction below).

**Update, Gate 3, Task 6 — the concluding sentence two paragraphs up is now
wrong for reads, and this is the correction.** "Classification, not
containment, is what this matrix is evidence about" was accurate through
Task 5, when the negative canary's read was still merely *documented* as
sealed (in the "Update, Gate 3, Task 5" paragraph above) without the test
itself asserting it. This task adds that assertion:
`escape_matrix_positive_and_negative_canary`'s `negative_expectation`
(`crates/vfs-directord/tests/e2e.rs`) now checks each buildable vector's own
reported outcome — not the classified-paths set, a separate check kept
alongside it, see below — and requires `not-found`, failing the test outright
if any spelling still opens the real bytes. **Corrected statement, precise
about what still varies by class:** this matrix now establishes
**containment**, not merely classification, for the negative canary's
**read**.

> **The paragraph above used to extend that claim to directory enumeration by
> argument. The argument was unsound, and behind it sat a real-disk drain —
> latent, not live: reachable only if a second, unrelated seal regressed
> too, and not reachable by a game as the tree stood.** Both halves of that
> sentence matter and neither should be read without the other.
>
> The argument ran: `readdir` (`NtQueryDirectoryFile(Ex)`) operates on a
> handle from an already-succeeded open, that open went through
> `create_hook`/`open_hook` → `decision_for` → `RootMap::decide`, so
> enumeration containment follows from read-open containment rather than
> being a separate mechanism. It does not follow. Enumeration has its own
> predicate (`FuseClient::vpath_under_root`, not `RootMap::decide`) and had
> its own fall-through: `serve_dir_query`'s no-director /
> client-does-not-recognise-this-directory branch drained the real directory
> behind the mount and listed whatever was physically there, for a handle
> whose *open* was entirely legitimate.
>
> What kept it off a live session was not the argument but gate 3 task 5:
> the only real under-root directory handle comes from an open the client
> does not route, and every such open is denied before a handle exists.
> Enumeration is closed by construction now and proven by test, so it no
> longer depends on that. See "Gate 4, Task 8b: enumeration" below for the
> full reachability analysis and the mutations it took to reach the branch.

**Correction: this did not extend to name-based metadata queries, and an
earlier version of this document claimed it did by the same mechanism —
that claim was false.** `qattr_hook`/`qfull_hook`/`qibn_hook`
(`crates/vfs-shim/src/hook.rs`) never call `decide` at all. Each asks
`fuse_path_attr`, which consults `fuse_client::vpath_under_root` — never
`RootMap::decide` — then the shim-local write overlay, then falls through to
the real filesystem if neither answers. While `vpath_under_root` was the
client's *own string-prefix predicate*, containment for a name-based
attribute query therefore held only for the spellings that predicate itself
recognised — the ordinary, unmangled ones — not for the five alternate
spellings (vectors 1, 3, 4, 7, 9) this document's own "second, structural
finding" (below) documents as recognised only by `RootMap`'s canonicaliser.
For those five, an attribute query on the negative canary reached the real,
physical file.

**Closed in stage 2b, task 5.** The two predicates were unified: the routing
half was deleted and `fuse_client::vpath_under_root` *is* a `RootMap` now
(`FuseClient` holds one, built over every declared root plus the staged
launch directory as an alias for root 0), so those five spellings are
recognised and routed by the same canonicalisation that classified them.
`crates/vfs-directord/tests/e2e.rs`'s
`metadata_queries_are_sealed_for_canonicaliser_only_spellings` — the same
test that used to record the gap, flipped rather than deleted — now asserts
`not-found` for vector 4's isolated `GetFileAttributesW` against the
volume-GUID spelling.

The hooks still do not route through `RootMap::decide`; that part of the
correction stands. What changed is that the predicate they *do* consult is
no longer a weaker one. Attribute queries also remain outside the
`under-root open outcomes` accounting this matrix's classification check
parses (`qattr_hook` and friends call `hookstats::note_stat`, a separate
counter, never `note_open_outcome`), so this containment is asserted by the
e2e test above rather than visible in this document's classification signal.

**The open-path consequence flipped with it.** Vectors 1, 3, 4, 7 and 9 went
`opened` → `not-found` in gate 3 task 5 (sealing a passthrough) and are back
to `opened` now — but for a different reason and against a different
mechanism: they reach the *director*, which serves the positive canary, and
they still come back `not-found` against the negative canary that no
provider serves. Reachable when a provider has it, sealed when none does,
for every spelling this fixture can build. See the matrix table below.

**Writes were the other class where classification, not containment,
applied** — gate 4's write fall-through (`Engine::cow_seed`, above) meant a
write open on the negative canary succeeded against real bytes; that class
was classified (it showed up in `FellThroughWriteFallback`) but not
contained.

**Gate 4's Task 5 closed it, so that is no longer true.** `cow_seed`'s
last-resort real-disk branch is gone, a write the director cannot serve
fails instead of reaching disk, and the **write matrix** below asserts it
directly: every buildable vector's negative-canary write comes back
`not-found`, the canary's bytes on the real filesystem are unchanged, and no
stray file is created under the root. `FellThroughWriteFallback` is now
reachable only behind the `allow_disk_fallthrough` opt-out, off by default.

What an "unreachable"/"sealed"/"closed" claim in this document still does
*not* cover: a name-based attribute query, a different hook family with no
path to `decide` at all (see the metadata-query correction above, and the
e2e assertion that covers it instead). Directory enumeration used to belong
on that list too — it is a separate mechanism with its own predicate and had
a fall-through of its own — but gate 4 task 8b closed it; see the task 8b
section below. (This sentence originally also asserted enumeration's "own
dependency on read-open containment being correct (which it is, by the
reasoning above)", which was the unsound part the task 8b correction
replaced.)

One more finding this task's own construction of the new assertion surfaced,
not predicted in advance: **vector 8 (hardlink) is no longer buildable at all
against the negative canary**, reproduced identically across five separate
runs. `std::fs::hard_link`'s underlying `CreateHardLinkW` needs a handle on
the *source* file to create the link — the negative canary itself, which
`RootMap::decide` now denies — so the hardlink can no longer even be
constructed (`unbuildable:std::fs::hard_link failed: The system cannot find
the file specified. (os error 2)`), where a session before Task 5 could
build it (the source open passed through and succeeded). This is a further
containment strengthening, not a regression: an operation that itself
requires reading the sealed file is sealed too, one level earlier than the
vector's own intended open. See the matrix row below and
`negative_expectation`'s doc comment in `e2e.rs` for how this is handled
(both the classification loop and the new unreachability loop skip
`unbuildable:` lines, exactly as the positive canary does).

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
`not-found` (positive canary) means the open reached a real decision — the
director or `RootMap::decide` — and was correctly sealed there, not that
nothing happened; see each such row's own note, and "A second, structural
finding" below for vectors 1/3/4/7/9 specifically.

**Gate 3, Task 6: every `classified✓` row below now also means unreachable
by a read.** `escape_matrix_positive_and_negative_canary`'s
`negative_expectation` now asserts `not-found` for the negative canary's own
reported outcome on every buildable vector, in addition to the pre-existing
`classified✓` check — see "Update, Gate 3, Task 6" above for the reasoning
and for why this is a strictly stronger property than classification alone.
The only two exceptions, both `unbuildable` rather than `classified✓`/
`not-found`, are vector 1 (unrelated attribute-query quirk, unchanged since
gate 2 — see its own note) and vector 8 (hardlink construction itself now
fails — a new Task 6 finding, see above and that row's own note).

**Vectors 1, 3, 4, 7 and 9's positive canary flipped twice.** Gate 3 Task 5
took them from `opened✓` to `not-found` (sealing a real-disk passthrough);
stage 2b Task 5 took them back to `opened✓`, this time *through the
director*. See "A second, structural finding: gate 2's alternate-spelling
closures were classification-only, never routing" for the full mechanism and
its resolution: all five were recognised as under-root only by `RootMap`'s
own canonicaliser and never by the shim's separate `vpath_under_root` string
matcher, so they could be classified but never routed. Stage 2b Task 5
deleted the second predicate — `vpath_under_root` *is* a `RootMap` now — so
they route like any ordinary path. Their negative-canary results are
unchanged throughout: a file no provider serves stays sealed.

**These rows are asserted against every declared root, not just the first.**
`escape_matrix_holds_against_a_second_root`
(`crates/vfs-directord/tests/e2e.rs`) runs the identical fixture, canaries
and expectation tables against a target under `RootId(1)` of a two-root
session.

| # | Vector | Positive canary | Negative canary | Note |
|---|---|---|---|---|
| 1 | 8.3 short name (`GetShortPathNameW` + open) | opened✓ (restored, stage 2b Task 5 — see above) | `unbuildable: GetShortPathNameW failed: win32:2` | See "Vector 1's negative-canary unbuildable reason" below for the negative-canary column — a real quirk of attribute-query fall-through, not a canonicaliser defect. Positive canary: was recognised under-root only by `RootMap`'s canonicaliser and so never routed; since the two predicates were unified it reaches the director like any ordinary spelling. See "A second, structural finding" below. |
| 2 | Extended-length prefix (`\\?\C:\...`) | opened✓ | classified✓ | |
| 3 | NT device path (`\\?\GLOBALROOT\Device\HarddiskVolumeN\...`) | opened✓ (restored, stage 2b Task 5) | classified✓ | **Found broken, then fixed — see "A real bypass found and fixed" below** (that fix is about the negative canary's classification, still intact). **Positive canary flipped to `not-found` in Gate 3 Task 5 and back to `opened✓` in stage 2b Task 5**, once the client predicate became the same canonicaliser; see "A second, structural finding" below. |
| 4 | Volume-GUID path (`\\?\Volume{guid}\...`) | opened✓ (restored, stage 2b Task 5) | classified✓ | Same mechanism as vector 3/1/7/9 — see "A second, structural finding" below. This is also the spelling the metadata-query gap was pinned on; it closed with the same change. |
| 5 | Handle-relative open (`OBJECT_ATTRIBUTES.RootDirectory` = a real directory handle) | opened✓ | classified✓ | |
| 5b | Handle-relative open against a handle `GetFinalPathNameByHandleW` cannot resolve (an anonymous pipe) | `error:ntstatus:0xC0000033` | not classified (by design) | **Caveat, not a failure — see "Vector 5's caveat" below.** `path_of_tracked` cannot decode a path at all for this shape, so it lands in the shim's separate "undecodable" counter, never in `under-root open outcomes`. Documented, accepted edge of Task 4's fix, not asserted as pass/fail either way. |
| 6 | CWD-relative (plain filename, cwd set to the parent dir) | opened✓ | classified✓ | |
| 7 | Junction / reparse point | opened✓ (restored, stage 2b Task 5) | classified✓ | Negative-canary classification **closed for a junction within two ancestor levels of the managed root (this project's own session layout) — not junctions in general; see "Vectors 7 and 9 closed: session-start alias resolution" below for the residual.** Was a verified, open gate-2 gap; fixed by resolving such a junction into a `VolumeMap` alias once per session. **Positive canary flipped to `not-found` in Gate 3 Task 5 and back to `opened✓` in stage 2b Task 5** — closed classification never made this spelling reachable *through the director* until the predicates were unified; see "A second, structural finding" below. |
| 8 | Hardlink (new filename, same underlying bytes) | `not-found` (not `opened` — see "Vector 8's exception" below) | `unbuildable:std::fs::hard_link failed: ... (os error 2)` (flip from `classified✓`, Gate 3 Task 6 — see above) | Positive canary: sealed by the content-addressed provider policy, not a classification failure — `RootMap`/the canonicaliser is never even consulted for this vector when FUSE-routing claims the path first. **Negative canary, Gate 3 Task 6 finding:** `CreateHardLinkW` needs a handle on the source (the negative canary itself) to create the link; `RootMap::decide` now denies that open, so the hardlink can no longer even be constructed — reproduced identically across five separate runs. Before Task 5 this was buildable (the source open passed through) and `classified✓`; the row above reflects current, post-Task-5 behaviour. |
| 9 | UNC / `subst` / mapped drive (administrative loopback share, `\\localhost\C$\...`) | opened✓ (restored, stage 2b Task 5) | classified✓ | Negative-canary classification **closed — see "Vectors 7 and 9 closed: session-start alias resolution" below.** Was a verified, open gate-2 gap; fixed by registering the admin-share's real NT spelling as a session-start alias. **Positive canary flipped to `not-found` in Gate 3 Task 5 and back to `opened✓` in stage 2b Task 5** — same reason as vector 7; see "A second, structural finding" below. |
| 10a | Case-flipped, `\\?\`-prefixed (verbatim) | opened✓ | classified✓ (`not-found`) | NTFS resolves case regardless of the `\\?\` prefix; standalone-`opened` behaviour, unaffected by session or gate 2. The e2e loop's negative-canary check skips only `unbuildable:` outcomes plus `5b`/`14` explicitly (see `classification_marker`/the skip check in `e2e.rs`) — `10a`'s outcome is neither, so it **is** asserted for the negative canary, and passes: the spelling lands in the shim's classified-paths set, correctly sealed (`not-found`) rather than left unclassified. |
| 10b | Trailing dot, verbatim (`...\name.esp.`) | opened✓ (flip from standalone `not-found` — see "The 10/12 flip" below) | classified✓ | |
| 10c | Trailing space, verbatim (`...\name.esp `) | opened✓ (flip) | classified✓ | |
| 11 | Alternate data stream (`name.esp:probe`) | `not-found` (expected — see note) | classified✓ | Read-only `OPEN_EXISTING` against a stream this fixture never pre-creates; `not-found` means the stream doesn't exist, not that streams are unsupported. Same result standalone and under a session. Stage 2b Task 5 note: `canonicalise` discards an ADS suffix (right for unifying spellings of a *file*), so the unified client predicate re-attaches it when building the vpath — without that, this row would read `opened` and be answering a named-stream request with the base file's bytes. See `FuseClient::vpath_under_root`. |
| 12a | `.` component, verbatim | opened✓ (flip) | classified✓ | |
| 12b | `..` traversal through a non-existent intermediate name, verbatim | opened✓ (flip) | classified✓ | |
| 12c | Doubled separator, verbatim | opened✓ (flip) | classified✓ | Standalone this reports `error:win32:123` (`ERROR_INVALID_NAME`); under a session it opens. |
| 13 | Handle opened before the managed root was registered | opened (reported, not closed) | not-found (reported, not closed) | **Reported, not closed in this gate — gate 3's job.** This test cannot construct the real "handle predates root registration" timing; it only re-confirms ordinary reachability from within an already-injected process. See "Vectors 13 and 14" below. |
| 14 | Child process launched without the shim injected | opened (reported, not closed) | `error:cmd-exit:1` (reported, not closed) | **Reported, not closed in this gate — may not be a shim fix at all.** By construction, this vector's own child process has no hook to intercept anything; its classification (or lack of it) is not evidence about gate 2 either way. See "Vectors 13 and 14" below. **The "by construction" premise here is false — the child *is* injected; see "Gate 4, Task 8".** |

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

**Vector 9 is closed unconditionally; vector 7 is closed only within the
scanned ancestor depth — see "Scope of the junction scan, chosen
deliberately" below for exactly what shape that is and is not.**

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

**The residual this leaves, stated plainly.** A junction whose real
location requires climbing more than two ancestor levels above the managed
root — anywhere else on the volume, not this project's own
`<TEMP>/vfs-daemon-*/root` session layout — is never scanned, never
registered as an alias, and is not classified: it stays outside-root,
indistinguishable from any other unrelated path elsewhere on disk. Gate 2
does not close that shape, and by this programme's own reasoning **no later
gate closes it either**: gate 3's job is removing *under-root* `NotFound`
passthrough, and a path the canonicaliser never recognises as under-root in
the first place never reaches that passthrough to begin with. So "vector 7
closed" throughout this document means closed for a junction within two
levels of the managed root — this project's own session convention — not
junctions in general.

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

  > **The premise of that bullet is false, and gate 4 task 8 measured it.**
  > The child *is* injected — see "Gate 4, Task 8: the write matrix"
  > below. The reading of the negative canary's `error:cmd-exit:1` given
  > here ("the target doesn't exist" for an uninjected `type`) is
  > particularly wrong: the target exists on real disk with real bytes, and
  > an uninjected `type` would have printed them. The vector stays
  > unasserted, but for a different reason than this bullet gives.

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

## Limitations recorded, not fixed, by this task

Found during the final whole-branch review of this gate. None of these are
believed to be reachable bypasses today — each is either bounded by a
separate check or purely a cost concern — but they are real properties of
the current code, worth a later gate's attention rather than silent
tolerance.

- **Unbounded alias table, two allocations per entry on every lookup.**
  `VolumeMap`'s alias table (`crates/vfs-redirect/src/canon.rs`) grows by one
  entry per mounted drive's device name, per drive's volume-GUID mount
  point, per drive's admin-share alias, and per junction the ancestor scan
  finds — no upper bound on the count. `VolumeMap::resolve` scans the whole
  table linearly on every call, and for each entry calls `fold()` twice
  (`fold(candidate) != fold(prefix)`) — an allocation per `fold` call. This
  cost is paid on every `canonicalise` cache miss, not just once per
  session; on a machine with an unusually large number of mounted drives or
  discovered junctions, this scan's cost grows without bound. Not a
  correctness issue, purely a scaling one — worth capping or memoizing the
  fold if it ever shows up in a profile.
- **Case-sensitive `Path` equality for the chain-node exclusion.**
  `junction_aliases` (`crates/vfs-redirect/src/volumes.rs`, ~line 225) skips
  aliasing a directory entry with `if candidate == child`, relying on
  `PathBuf`'s `Eq` — which compares components byte-for-byte, not
  case-insensitively, even though NTFS paths are case-insensitive. A
  directory entry spelled with different case than `child`'s own literal
  path (e.g. the OS returns a differently-cased name than the one this scan
  constructed) would fail this equality check and fall through to the
  ordinary reparse-point handling below, which is harmless today only
  because the separate "target must resolve inside root" check
  (`is_component_prefix`) already rejects root's own ancestor chain being
  aliased to itself by a different route. Should this exclusion ever become
  the only guard against re-aliasing an ancestor, it would need a
  case-folded comparison instead.
- **The admin-share alias assumes every mapped drive is local, unverified.**
  `resolve_volume_map` (`crates/vfs-redirect/src/volumes.rs`) registers a
  `\\localhost\<drive>$` alias for every drive `vfs_win::drive_mappings`
  reports, including network-mapped drives (a mapped drive backed by
  `\Device\LanmanRedirector\...` or similar passes `drive_mappings`'s
  `\Device\...`-shaped filter same as a local volume). `\\localhost\<drive>$`
  is only a meaningful alias for a drive that is actually local storage on
  this machine; for a genuinely network-backed drive letter, nothing
  verifies that the administrative loopback share even resolves to the same
  object, let alone the same bytes. No known exploit path — a real Windows
  installation's `\\localhost\<drive>$` administrative share for a
  network-redirector drive typically fails to resolve to anything at all —
  but the alias is registered unconditionally regardless, on an unverified
  assumption.
- **The Mod Organizer exposure is gate 3's, not gate 2's.** An MO2-style
  junction *inside* the managed root pointing at external staging (e.g.
  `root\Data\SomeMod` -> `D:\Mods\SomeMod`) is already classified under-root
  today, by ordinary literal-spelling matching — no special handling needed,
  and gate 2's own junction scan deliberately does not recurse into the
  root's own subtree to find these (see "Scope of the junction scan" above)
  precisely because pulling in a genuinely external directory that way would
  be the over-eager failure this gate guards against, not a fix. The
  reverse-direction junction (pointing *out* of the root) is genuinely
  outside gate 2's scope for the same reason. **The concrete consequence for
  gate 3 to handle:** gate 3's job is removing under-root `NotFound`
  passthrough — and once that passthrough is gone, an MO2-style junction's
  real content (physically outside the managed root, reached today only via
  that passthrough falling through to the real disk) will be sealed along
  with every other passthrough case. This is a predicted, concrete
  regression for a real, common mod-manager setup, not a hypothetical edge
  case — gate 3's implementer needs to design for it explicitly (e.g.
  resolving such junctions into the content model itself, not just relying
  on disk fallthrough) rather than discover it after passthrough removal
  ships.

## Gate 3, Task 5: the root becomes fully virtual — what actually happened

`vfs-redirect::RootMap::decide` no longer passes a `NotFound` or `Dir`
resolution through to the real filesystem; both deny
(`STATUS_OBJECT_NAME_NOT_FOUND`), matching the tombstone arm. `Located::Outside`
is the only remaining `PassThrough`. See that function's own doc comment
(`crates/vfs-redirect/src/lib.rs`) for why `Dir` denies rather than serving —
this pure, snapshot-only function has no ring connection and cannot itself
hand back a director-served directory handle; that handle comes from
`vfs-shim::hook::try_fuse_create`'s live path, unaffected by this change and
already correct for every directory the provider graph actually knows about.

**Scope: reads only.** This closes the *read* path — a write open
(`FILE_OPEN`/`FILE_OPEN_IF` against a file that already exists) was a separate
mechanism (`Engine::decide_open`/`cow_seed`) that at the time still seeded the
overlay from real on-disk bytes when neither `Redirect` nor `Serve` applied,
which after this task included `Deny` exactly as it had included
`PassThrough` before. That write-path residual was gate 4's to close, not
gate 3's.

**Superseded by gate 4, Task 5:** it is closed. `cow_seed`'s last-resort
real-disk branch is deleted and copy-up now reads only through the director,
so the read/write scope split this paragraph introduced no longer applies —
see "Gate 4 note" and the write matrix below.

### What the shipping config's own launch does and does not demonstrate

**This is the most consequential correction in this section, stated up
front rather than as a footnote.**
`crates/vfs-directord/src/bin/skyrim-live.rs`'s `mount_low_priority_disk_layers` (~lines 449-461) mounts
`DiskProvider::new(root)` at `/` — the managed root's own physical directory,
mounted as a provider over itself. Layered above it in `run()` (~lines
202-229): the zip-packaged game content, an optional mods directory, and the
write overlay (`overrides`) — each *also* mounted at `/`. This is the
sanctioned, correct way this project exposes a real, on-disk tree: content
still only ever reaches the game through the director (the FUSE ring, never
a raw kernel passthrough), which is the actual guarantee this design
preserves.

But it has a direct consequence for what the real launch can prove: **for
the shipping config, there is no real on-disk file under the managed root
that no provider knows about** — every physical file is either the zip's own
content or a reflection of something the launcher wrote directly onto
`root`, which the root-disk mount then serves right back as its own content.
That is exactly the condition `RootMap::decide`'s new `NotFound`/`Dir` deny
exists to seal, and the shipping config structurally never produces it. So
this task's own deny is barely reachable in a real session — the report's
own stats confirm it (zero entries under any `denied` bucket for the whole
launch, menu through world load through shutdown). **The successful Skyrim
launch is real, valuable non-regression evidence — proof this task did not
break ordinary play — but it is not evidence that the virtual-root property
holds:** the sealing behaviour this task adds, and the MO2 junction breakage
that follows from it, are both properties of what happens to a real file
that is *not* mounted as a provider, and the shipping config's own mount
stack never leaves anything in that state. The property this task actually
adds is demonstrated by the tests, not the launch:
`vfs-redirect::real_on_disk_file_under_root_with_no_snapshot_entry_is_denied`,
`vfs-shim::engine::tests::real_on_disk_file_under_root_not_in_snapshot_is_denied`,
and `vfs-shim::engine::tests::mo2_style_junction_inside_root_pointing_to_external_staging_is_sealed`
each construct, directly, the orphaned-content shape the live launch's own
layering never produces.

### The Mod Organizer consequence, confirmed by reproduction

The prediction above is correct, and verified directly rather than assumed:
`vfs-shim::engine::tests::mo2_style_junction_inside_root_pointing_to_external_staging_is_sealed`
builds a real junction (`mklink /J`) inside a real managed root pointing at a
real, external staging directory holding real bytes, confirms `std::fs::read`
through the junction genuinely returns those bytes (proving the junction is
real and transparent — exactly the mechanism the old passthrough relied on),
and then confirms `Engine::decide` now denies the identical path. Before this
task, `Decision::PassThrough` let that read reach the real, junctioned
directory; after, `Decision::Deny` seals it before any real open is ever
attempted — the junction's transparency at the kernel level no longer matters,
because the shim now refuses the open before the kernel ever sees it.

**Update (stage 2b, task 1): both gaps below are now closed.**
`vfs-director::path::strip_prefix` folds ASCII case on both sides at compare
time, and `Director::readdir` now contributes the next path component of any
mount registered below the queried directory as a synthetic directory entry.
The section below is left as originally written, as the historical record of
what gate 3 found and why its documented remedy did not work *at the time* —
see `crates/vfs-directord/tests/composition.rs`'s
`non_root_mount_matches_lowercase_open_and_is_discoverable_via_parent_readdir`
(renamed from `..._but_not_parent_readdir`) for the test that now asserts the
fixed behavior, including the original mixed-case spelling this section
documents below.

**The required configuration could not be "mount the staging directory as a
provider" with the spelling this section used to give — that prescription
did not work as written, and a reader who followed it would have lost a
debugging session finding out.** Two independent problems, found by actually
constructing this rather than reasoning about it in the abstract
(`crates/vfs-directord/tests/composition.rs`'s
`non_root_mount_matches_lowercase_open_but_not_parent_readdir`, since renamed
— see the update note above):

1. **Case.** Mount prefixes are compared case-sensitively
   (`vfs-director::path::strip_prefix`; `vfs-directord::registry::add_source`,
   ~line 184, stores the mount string exactly as given), but every vpath the
   shim ever sends over the ring is already lowercased
   (`vfs-shim::fuse_client::normalize_path_for_root`) before
   `vpath_under_root` even sees it. This section's previous example,
   `mount = "Data/SomeMod"`, can therefore never match a live open — a
   mixed-case prefix is compared against an always-lowercase path and never
   equal. The corrected, working spelling is **all-lowercase**:
   `mount = "data/somemod"` for a junction that used to sit at
   `root\Data\SomeMod`, `layer` above the base game content so it wins on any
   name collision. With that spelling, a direct, known-relative-path open
   through the mount does succeed — confirmed by the test above, not assumed.
2. **Directory enumeration does not follow.** Even with the case fixed,
   `Director::readdir` (`crates/vfs-director/src/director.rs`, ~lines
   103-140) only asks a mount for entries when that mount's own registered
   prefix is at-or-above the queried path; a mount rooted *below* the query
   (`data/somemod`, queried via `readdir("data")`) never contributes a
   synthetic entry for itself to that listing. So a consumer that discovers
   `SomeMod` by *listing* `Data` — a mod manager's own browser, a tool that
   scans `Data` for subfolders, anything that does not already know the exact
   relative path to open — will not see it, correctly cased or not. The same
   test proves this half too: the mount's own file opens correctly, but
   `readdir("data")` never lists `somemod` as a child.

**Two confirmed limitations, recorded here at the time specifically so a
later gate would inherit them rather than rediscover them — both now closed
by stage 2b task 1, per the update note above:**

1. ~~A non-root mount serves a known path only if spelled **all-lowercase**
   (mount prefixes compare case-sensitively while shim vpaths are always
   lowercased) — item 1 above.~~ Fixed: `strip_prefix` now folds case at
   compare time.
2. ~~`Director::readdir` **never** lists a mount registered below the queried
   directory, so a non-root mount cannot be discovered by listing at all —
   item 2 above.~~ Fixed: `readdir` now contributes a synthetic entry for
   the next path component of any deeper mount.

**Limitation 2 was flagged for stage 2b specifically, and this is the task
that closed it.** Gate 3 (this gate) only hit this because the MO2 remedy
needed a non-root mount to demonstrate; it was not gate 3's own concern
otherwise. **Stage 2b is "real multi-root," and mounts are exactly the
mechanism roots compose through** — an unenumerable mount would not have
been a cosmetic gap for that stage; it was a problem stage 2b would have hit
directly the first time it composed more than one root and expected a
listing of one to reflect the other. Stage 2b task 1 fixed both, first,
before any of stage 2b's multi-root composition work landed on top —
recorded here as the account of why that ordering mattered, not left as an
open item.

**This was a genuine, confirmed gap in non-root mount support, not merely an
undocumented spelling — said so plainly rather than papered over, and now
fixed rather than merely documented.**
At the time, `grep`-ing this project's entire tree turned up no non-root
mount anywhere outside this task's own new test: every existing mount, in
every test and in `skyrim-live.rs` itself, was registered at `"/"` (root,
layered by priority). This section's own previous citation of
`scenario_toml_two_disk_sources_fixture_writepath` as "the ordinary way this
project composes mods" was wrong in the same direction — that test mounts
both its sources at root, not at a non-root prefix. This project's actual,
exercised mod-composition path was root-layering; non-root mounts had not
been used by any session tooling here for anything up to that point. As of
stage 2b task 1, a non-root mount is discoverable via directory enumeration
of its parent, not just openable by an already-known relative path — the
caveat in this paragraph's earlier revision no longer applies. Any session
tooling that lets a user point at an MO2-style mod-staging layout can mount
that directory as a source in whatever case it's naturally spelled, and the
mod's files will now appear in a `Data` listing too.

### A second, structural finding: gate 2's alternate-spelling closures were classification-only, never routing

Running the full suite after the `RootMap::decide` fix surfaced five more
flips in the escape-matrix e2e's positive canary (vectors 1, 3, 4, 7 and 9 —
8.3 short name, NT device path, volume-GUID path, junction, and UNC admin
share), each going from `opened` to `not-found`. All five share one root
cause, and it is more consequential than any single vector: **all five are
recognised as under-root only by `RootMap::compute_under_root`'s
canonicalisation — never by `vfs-shim::fuse_client::vpath_under_root`, the
router that actually decides whether an open reaches the director at all.**
`vpath_under_root` does plain, case/separator-normalized string prefix
matching against the literal root and the staging-directory alias; it has no
device-prefix table, no volume-GUID table, no `GLOBALROOT` unwrap, no UNC
admin-share alias, and no junction-alias resolution — all of that lives
solely in `vfs-redirect`'s `VolumeMap`/`canon.rs`/`volumes.rs`, consulted only
from `RootMap::compute_under_root`, which `decision_for` reaches *after*
`try_fuse_create` has already given up on routing to the director.

In a real, live session the shim's own embedded `Engine` snapshot is always
the empty tree (`vfs-director::Session::serve`'s `shim.cfg` — the FUSE ring
to the director is the only real content path in a live session; see that
function's own comment). So for any of these five spellings, `RootMap` places
the path under the root correctly (that part of gate 2's fix still works
exactly as documented above), but the snapshot behind it is empty —
`SnapResolution::NotFound`, unconditionally, regardless of what the director's
real provider graph actually has. Before this task, `NotFound` passed
through, and each of these vectors "opened" by reading the byte-identical
real file physically on `session.root` — **never through the director, for
any of these five spellings, in any session that has ever existed.** Gate 2's
own exit criterion was classification (a counted outcome bucket), which these
five vectors correctly satisfy; none of them were ever evidence that the
director actually served that spelling. This task's fix is what exposes the
distinction: removing the passthrough that was quietly compensating for it
turns "correctly classified, secretly still reaching disk" into "correctly
classified, now also sealed" — for content that, had it been requested via an
*ordinary* spelling, the director would have served correctly (the positive
canary's content is real and mounted; only these five specific spellings
never reach it).

**Not fixed in gate 3 — fixed in stage 2b, task 5.** The prediction above
("closing this fully would mean teaching `fuse_client::vpath_under_root` to
recognise the same alternate spellings `RootMap` already does") is what
happened, by the stronger route: there is no longer a `vpath_under_root`
*to* teach. `FuseClient` holds a `RootMap` and `vpath_under_root` is that
map's `resolve`, so one predicate answers for both routers and the two
cannot drift apart again.

Two things came with it, both structural rather than incidental:

- **`RootMap` holds several roots**, answering `(RootId, remainder)` rather
  than a bare "inside/outside" — which is what stage 2b needed anyway, and
  what makes the answer routable. The staged-launch-directory alias that
  used to be the client's own second prefix test is now just another
  declared entry sharing root 0's id.
- **The ring wire carries the root.** Every path-carrying payload leads with
  a `root:u32` (`vfs_protocol::encode_path_req` and friends), and
  `vfs_ipc::layout::VERSION` went to 2 so a stale injected DLL speaking the
  old shape is refused at ring attach rather than having its first four path
  bytes read as a root id.

The five affected vectors' `positive_expectation` entries in
`crates/vfs-directord/tests/e2e.rs` are back in the catch-all `opened` case
they started in — and their `negative_expectation` entries are unchanged at
`not-found`, which is the pair that makes it containment rather than an
access regression in either direction.

**One deliberate carry-over.** `canonicalise` discards an alternate-data-
stream suffix, correctly: `f.esp:s` and `f.esp` are spellings of the same
*file*, which is what a canonicaliser unifying spellings should say. A vpath
built for the director must not discard it, though — a named stream that
does not exist has to answer not-found, not the base file's bytes — so
`FuseClient::vpath_under_root` re-attaches the suffix after resolving. The
string predicate kept it by accident (it never parsed the path at all);
keeping it deliberately is what holds vector 11 at `not-found`. Found by
that vector flipping to `opened` on the first unification pass, not
reasoned about in advance.

**Why this does not threaten the real Skyrim launch this task also
verifies.** None of these five spellings is one a real game process, SKSE, or
Steam constructs on its own — every one is an adversarial escape-matrix
construction (an 8.3 short name of a deep ancestor directory, a raw NT device
path, a volume-GUID path, a `GLOBALROOT`-wrapped device path, a UNC
administrative-share path, a junction one or two ancestor levels above the
managed root). Ordinary game file access — plain drive-letter paths,
CWD-relative paths, handle-relative opens — routes through
`fuse_client::vpath_under_root` exactly as before and is unaffected (vectors
2, 5, 6, 10a, 12a-c all still `opened✓`, unchanged by this task). The real
launch via `tools/gamectl.ps1` is the actual arbiter for ordinary operation,
not this synthetic matrix — see the task report for that outcome.

### Verification (Task 5)

- `cargo test --workspace`: see the task report for the exact count and any
  further named flips found while finishing this task.
- `cargo clippy --all-targets -- -D warnings`: see the task report.
- Still present and unmodified **as of that task** (see the gate 4 note below):
  `Decision::Redirect`, `Decision::Serve`, the
  legacy `zipserve` path, the DRM filename exceptions, and the write
  fall-through (`try_fuse_create` still calls `client.open_write` even with a
  director live, and the director still rejects writes, so writes still reach
  `decide_open` — gate 4's job, not touched here).

## Gate 4 note: what the section above no longer describes

The "Verification (Task 5)" list above is a record of what was true when that
task ran, not of the tree today. Gate 4 changed three of those items:

- **`Decision::Serve` is deleted.** It had been unreachable in production since
  the FUSE client became mandatory — both its arms early-returned whenever the
  client was installed, which bootstrap always does.
- **The zip-window half of `zipserve` is deleted** (`open_synth`, `ZIP_MAPS`,
  `copy_window_to_file`, and the `is_synth` handle-lifecycle checks, which were
  provably constant-false once `open_synth` went). The **synthetic-section**
  half — `register_mapped_image`, `map_view`, `close_section` and friends — is
  **retained**: it backs `fuse_create_section`'s `SEC_IMAGE` path and all of
  `lazy_section.rs`, and is live machinery.
- **The write fall-through is closed.** A write to a path under a managed root
  that the director cannot serve now fails with an NT status and creates no
  file on the real filesystem.

`Decision::Redirect` and `Decision::Deny` **survive**. Gate 4 recorded that the
four DRM/identity filename exceptions were what kept them alive, and that
removing those exceptions would collapse the enum.

**Corrected 2026-08-15 (gate 5): that was wrong, and it was measured wrong.**
Gate 5 closed all four exceptions and the variants stayed live, because
`try_fuse_create` has a **fourth** `None` exit nobody had accounted for — the
`ST_NOT_FOUND` arm under `allow_disk_fallthrough()` (`hook.rs:1307-1308`). Driven
through real detours against a real ring, an unserved under-root read produces
`denied=1` (so `Deny` is live) and `decide_open`'s write branch runs copy-up and
returns `Redirect`.

That switch is registered, documented in `architecture.md`, relied on by this
document for stray detection, and was kept deliberately by gate 4 — so the enum
does not collapse until someone decides *that* opt-out should go, which is a
separate decision from the DRM exceptions and was never gate 5's to make.

One nuance found while establishing this: the switch does more than its doc
claims. In a director session the snapshot is a *valid empty tree*
(`session.rs:493`), so `SnapshotReader::open` succeeds and an under-root **read**
lands on `Deny` either way — the same status as the seal. What it still changes
is the **write** path, **and reads of whatever those writes put in the overlay**:
`Engine::decide` consults `overlay_state` *before* the snapshot
(`engine.rs:314-320`), so once a fall-through write has materialised an overlay
copy, a later read of that path returns `Redirect` to the copy rather than the
seal. Reads do change, derivatively — an earlier version of this paragraph said
they did not.

**One counter changed meaning, not just value.** The old `Decision::Serve` arms
recorded `FellThroughServe` *before* their FUSE early-return, so that class was
incrementable in production. Traffic that would have landed there now records
as `Denied`. Measured runs showed zero either way, so no recorded figure moves —
but this is a **merge**, not a drive-to-zero, and the rows below reading
"still unexercised" should be read as "no longer reachable" from gate 4 onward.

## Gate 4, Task 8: the write matrix

Spec §8 criterion 1 asks for the canary matrix green for **write** access as
well as read, with one clause carrying the weight: *"a write to the negative
canary must be blocked, and must not create a file on the real filesystem
under the root."* Everything above this section is about reads.

`vfs-fixture-escape` now takes `VFS_ESCAPE_ACCESS=write`. Every vector builds
exactly the same spelling as in read mode — only the call made against it
changes — so the two matrices are comparable line for line. A write vector
opens with `OPEN_ALWAYS`/`FILE_OPEN_IF` (create if absent, never truncate),
writes a fixed-length payload that encodes its own vector id, and re-opens
the *same* spelling to read it back. `written` therefore means "this vector's
own bytes are durable through this name", not "the call returned success".
The disposition is chosen for both halves at once: it creates, so a spelling
that escapes leaves a file on disk for the harness to find; and it preserves,
so it is the shape that asks the director for a copy-up.

The test is `escape_matrix_write_access_positive_and_negative_canary`
(`crates/vfs-directord/tests/e2e.rs`).

### The geometry differs from the read matrix, and it has to

The read matrix mounts a writable `DiskProvider` at `/` and calls a file its
backing directory merely lacks "unserved". For reads that is exact: a read of
an absent name is a refusal. For **writes** it is not — a *create* of that
name under a writable mount is something the provider graph legitimately
accepts and stores. Contained, but not blocked, and the spec says blocked.

So the write matrix mounts its source at `/Games/Skyrim/Data` and puts the
negative canary in a sibling directory, `Games/Skyrim/Unserved`, physically
on the managed root and covered by no mount at all. Verified by mutation:
adding a second writable mount over `Games/Skyrim/Unserved` flips every
negative line from `not-found` to `written` while the real filesystem stays
clean throughout — the same distinction, stated as a measurement.

### The matrix

Positive canary: `escape-write-positive-canary.esp`, seeded identically in
the provider's backing store and physically under the root. Negative canary:
`escape-write-negative-canary.bin`, physically under the root, served by
nothing.

| # | Vector | Positive (write) | Negative (write) |
|---|---|---|---|
| 1 | 8.3 short name | `written` | `unbuildable:GetShortPathNameW failed: win32:2` |
| 2 | Extended-length prefix | `written` | not-found |
| 3 | NT device path (`GLOBALROOT`) | `written` | not-found |
| 4 | Volume GUID path | `written` | not-found |
| 5 | Handle-relative open | `written` | not-found |
| 5b | Handle-relative, unresolvable root handle | `error:ntstatus:0xC0000033` | `error:ntstatus:0xC0000033` |
| 6 | CWD-relative | `written` | not-found |
| 7 | Junction | `written` | not-found |
| 8 | Hardlink | `written` | `unbuildable:std::fs::hard_link failed: os error 2` |
| 9 | UNC admin share | `written` | not-found |
| 10a | Case fold | `written` | not-found |
| 10b | Trailing dot (verbatim) | `written` | not-found |
| 10c | Trailing space (verbatim) | `written` | not-found |
| 11 | Alternate data stream | `written` | not-found |
| 12a | `.` component (verbatim) | `written` | not-found |
| 12b | `..` traversal (verbatim) | `written` | not-found |
| 12c | Doubled separator (verbatim) | `written` | not-found |
| 13 | Handle predating root registration | `written` (reported, not closed) | not-found (reported, not closed) |
| 14 | Child process | `written` (reported, not closed) | `error:cmd-exit:1` (reported, not closed) |

Both `unbuildable` lines are honest consequences of containment, not
environment gaps: `GetShortPathNameW` and `CreateHardLinkW` each have to
resolve the target through hooked NT opens, and the negative canary is
unreachable through those, so neither construction can be built at all. They
are reported as unbuildable and excluded from the outcome assertion, never
silently skipped.

`5b`, `13` and `14` are exempt from the strict assertion for exactly the
reasons the read matrix already documents.

### The real-filesystem assertions, and where they are made

A write that is refused at the API while still leaving a zero-byte file under
the root has breached containment and reported success, so the status columns
above are only half the claim. The other half is asserted **from the
`vfs-directord` test process, which is never injected** — the equivalent of
`write_seal.rs`'s `drop(hooks)` before it inspects the root, and stronger,
because there is no detour in this process to drop. A hook-live `exists()`
answers about the provider graph, which is precisely the answer a breached
containment layer would want it to give.

After the negative run: the canary's bytes are byte-identical to what the
harness wrote; its directory contains nothing but the canary; and no named
stream was created on it (vector 11 writes with a creating disposition, and a
stream is a create no directory listing shows). After the positive run: the
provider's copy holds a payload, the physically-mirrored copy under the root
still holds its seed, and that directory has no strays either.

Both canary directories are proved physically writable first, by creating and
deleting a probe file in each from the harness. A "nothing was created"
assertion against a directory that could not have been written to anyway
establishes nothing.

### Verification (Task 8)

Every assertion was watched failing for its own distinct reason before it was
watched passing:

- **Real-disk stray detection.** With `VFS_ALLOW_DISK_FALLTHROUGH=1` in the
  fixture's environment, the negative run leaves `.vfs-escape-hardlink-<pid>`
  on real disk and the stray assertion names it. (Un-sealing also flips the
  status column, so the status loop had to be skipped to reach this one.)
- **Canary-content assertion.** Clobbering the canary from the harness before
  the check fails it on the byte comparison, and on nothing else.
- **Named-stream assertion.** Creating the stream from the harness fails it,
  and only it.
- **Negative status expectation.** Adding a writable mount over the unserved
  directory flips vector 2 to `written` — the mutation that shows the mount
  geometry above is load-bearing rather than incidental.
- **Positive status expectation.** Moving the mount to a prefix that does not
  cover the positive canary flips vector 2 to `not-found`, so the half that
  forbids "pass by breaking everything" is not vacuous either.
- **The assertions have teeth against the fixture itself.** Run standalone
  and uninjected against an ordinary directory, the identical write matrix
  creates `<name>.`, `<name> ` and the named stream — the exact artefacts the
  session run must not produce.

Classification-in-isolation (`VFS_ESCAPE_ONLY_VECTOR`) is **not** re-run for
the write matrix. That property is about whether a spelling is recognised as
under-root at all; it is established per spelling by the read matrix, and the
spellings are identical in both modes. The write matrix asserts the stronger,
more specific thing: the outcome, and the state of real disk afterwards.

### A finding: vector 14's child is injected

Vector 14 has been described throughout this document, in the fixture, and in
the e2e expectation tables as "a child process launched without the shim
injected... there is no hook in that process to intercept anything". That is
not what happens. `vfs-shim/src/hook.rs` detours
`kernelbase!CreateProcessInternalW` — the funnel under every `CreateProcess*`
— force-suspends the child, and injects the same DLL into it. Measured:

- read matrix, negative canary: `error:cmd-exit:1`. The file exists on real
  disk with real bytes; an uninjected `cmd /C type` would have printed them.
  The "Vectors 13 and 14" section above explains this line as "`type` exits
  non-zero when the target doesn't exist", which reads a containment result
  as an absence.
- write matrix, positive canary: `written`, with the bytes landing in the
  provider's store and not under the root.
- write matrix, negative canary: `error:cmd-exit:1`, and no file on disk.

So vector 14 is contained in practice for a child spawned by an
already-injected process. It is still not asserted, and should not be on this
evidence alone: `inject_child` is explicitly best-effort — force-suspend,
inject, give up on timeout — so a green assertion here would be an assertion
about scheduling. Closing it properly means deciding what happens when that
inject fails, which is not gate 4's question. What has changed is that the
reason for leaving it open is now recorded correctly.

### Two unwired fixtures deleted

`vfs-fixture-write` and `vfs-fixture-writeset` were workspace members no test
harness invoked. Neither serves this task, so both were deleted rather than
wired up, along with the `VFS_FIXTURE_DATA` and `VFS_FIXTURE_DIR` switches
that existed only for them.

`vfs-fixture-write` was strictly subsumed: `vfs-fixture-writepath`, which the
e2e scenarios do run, does its create/write/read-back round trip and a good
deal more. `vfs-fixture-writeset` was not — it covered `mkdir` (including the
`AlreadyExists` idempotency idiom) and `set_len` truncation, and **nothing
end-to-end covers those two today**. Deleting it removes a fixture that made
that gap look covered; the gap itself is recorded here rather than left
implied by a binary nobody runs.

## Gate 4, Task 8b: enumeration

Reads, metadata and writes are each sealed above and proven by the fourteen
spellings. Enumeration was never proven — it was *argued*, in the two places
this section's corrections above now point at, on the grounds that a listing
runs on a handle whose open already went through `RootMap::decide`.

**The argument is unsound, and behind it sat a real-disk drain — latent, not
live.** Take both clauses together. The dispatch for this task described the
fall-through as live and it is not, and a fix advertised as closing a live
hole when it closed a latent one is this document's own recurring disease in
mirror image. "Enumeration containment is now proven rather than argued" is
the honest claim. "A game could have read unserved files out of a directory
listing last week" is not.

Enumeration does not consult `RootMap::decide` at all. `serve_dir_query`
(`crates/vfs-shim/src/hook.rs`) asks `FuseClient::vpath_under_root`, a
different predicate from the one that admitted the handle to the shim's
directory table (`path_is_ours`, which accepts either the engine's root notion
or the client's). When the two disagreed, or when no client was installed, the
function drained the real directory behind the mount — `drain_real` over the
handle, in `FileFullDirectoryInformation` — and layered the shim-local write
overlay on top. A real, unserved file physically under the managed root would
be listed by name, off a handle whose *open* was entirely legitimate, which is
exactly why read-open containment does not imply this.

**Why it did not fire.** Four independent facts, each verified rather than
assumed. `path_is_ours` is engine-OR-client, and the client's `RootMap` is the
engine's root list plus the staging alias — so "engine accepts, client
declines", which the fallback requires, cannot arise. `RootMap::decide` maps
`NotFound`/`Dir`/`Tombstone` to `Deny` before any trampoline call, so an open
that could yield a real under-root directory handle fails first. Neither
`Decision::Redirect` arm calls `tag_under_root`, so a redirected handle never
enters the directory table at all. And a director-served directory yields a
`fuse_synth` handle, which is not a kernel handle, so `drain_real`'s first
call returned a negative status and the drain returned nothing.
`FuseClient::try_init_from_env` runs before the engine is built and before any
detour installs, closing the last window.

The seal was gate 3 task 5's, not enumeration's own — which is the point.
Enumeration was relying on another mechanism's invariant, unknowingly, with no
test on either side of the dependency, while this document asserted the
dependency was sound "by the reasoning above".

Nothing tested either branch. No fixture called `read_dir` or
`FindFirstFileW` under a session; every `read_dir` in `e2e.rs` and
`write_seal.rs` ran in the uninjected harness against physical disk. The
shim-level enumeration tests (`hook_direnum.rs`, `hook_enum_parity.rs`,
`hook_relative_paths.rs`) install real detours but attach no director, and
their fake director implements no `OP_READDIR`. The director-level `readdir`
tests are numerous and all positive-presence or de-duplication; none asserted
that an unserved real file was **absent**.

### What each branch does now

The handle reached the directory table only because the path is under a
managed root, so every listing built here is a listing under a managed root,
and only two things may appear in one: what the director serves, and the
shim-local write overlay's own entries. The real directory behind the mount
may not.

- **The client recognises the directory.** The director's `readdir` is the
  whole answer, authoritative and unmerged. Unchanged, and the only branch
  anything reaches today.
- **No client at all**, and **a client that does not recognise this
  directory.** Both answer from the shim-local write overlay alone
  (`overlay_listing` over an empty base) and neither drains real disk. They
  are counted separately because they are different failures: "no director" is
  a configuration state, while "the two under-root predicates disagree" is a
  bug with a history in this tree, and the second deserves to be a number in
  the report rather than a directory that mysteriously lists nothing.

**Neither of those two is reachable today — by a game or by a test.** Stated
plainly because an earlier draft of this section got it wrong in a way worth
recording. The no-client case was said to be exercised by this project's own
hook tests (`hook_enum_parity`, `hook_relative_paths`), on the grounds that
they install the shim with no ring. They do, but they never reach this code:
their `Data` is overlay-backed, so `Engine::decide` answers
`Decision::Redirect`, and neither `Redirect` arm calls `tag_under_root` — the
handle never enters `DIR_TABLE`, and `serve_dir_query` hands it to the OS on
the untracked branch. Measured with a temporary probe in each branch rather
than argued: **zero hits on either under-root branch across all three shim
enumeration tests**, with `hook_enum_parity`'s own listing landing on the
untracked branch against the redirected physical path `overlay\root-0\data`.
So `ContainedNoDirector`, and with it `Engine::overlay_listing`'s only
remaining call site, is dead code as the tree stands.

That makes the conclusion stronger rather than weaker: no coverage was
weakened by this change, because those tests' code path did not move at all.
And the branch is still right to keep and right to answer this way. The
alternative — keeping the drain on the grounds that only an unsupported
configuration reaches it — would make "no director" mean "the real tree is
visible", which is precisely the un-virtualised launch that retiring
standalone mode exists to prevent. A branch nothing reaches, that fails
closed, is the correct shape for one that would otherwise fail open.

`drain_real`, `drain_real_classic` and `vfs-redirect`'s
`parse_full_dir_info` are deleted along with the branch. Containment here is
now structural: no code remains that can read a real directory into a served
listing.

### The counter now says which mechanism answered

`hookstats::note_readdir` recorded a `served: bool` — "a listing we produced"
versus "one we handed to the OS". That is not the distinction containment
turns on, and **nothing anywhere asserted it**. Both under-root branches
recorded `served: true`, the draining one included, so the single instrument
that could have shown an under-root listing coming off real disk reported it
identically to a director-authored one. It is a three-way `ReadDirSource` now
— `director` / `contained` / `OS` — and the e2e test below asserts it.

### The test

`directory_enumeration_under_a_managed_root_hides_an_unserved_real_file`
(`crates/vfs-directord/tests/e2e.rs`) runs `vfs-fixture-escape`'s new,
opt-in-only `enum` vector under a real composed session against the same
two-canary geometry the read matrix uses: a served canary in both the
provider's backing store and physically under the root, and an unserved canary
physically under the root alone. The fixture lists the directory with
`std::fs::read_dir` (`FindFirstFileW` on Windows) and reports the sorted entry
names. The harness asserts, from its own never-injected process, that both
files really are in the physical directory first — an absence assertion
against a directory that never held the file establishes nothing — and then
that the listing contains the served name, does not contain the unserved one,
and that every recorded enumeration of that directory names `director` as its
source.

### Verification by mutation

The test was watched failing before it was watched passing, and against the
real defect rather than a proxy. Both mutations reconstruct states this tree
has genuinely been in: `FuseClient::vpath_under_root` declining the target
directory (the predicate drift the `path_is_ours` comment records as
possible), plus `RootMap::decide`'s `NotFound` arm returning `PassThrough` (its
behaviour before gate 3 task 5). Together they produce what the fall-through
needs and nothing else in the current tree produces: a real directory handle
under the managed root that the client does not claim.

- **Against the pre-fix code, with both mutations.** The listing came back
  `["enum-served-canary.esp", "enum-unserved-canary.bin"]` and the test failed
  on the absence assertion, naming the leaked file. The served canary was
  present in that same listing, so the failure is the leak and not a broken
  enumeration.
- **Against the fixed code, with the same two mutations.** The listing came
  back empty (`FindFirstFileW` reporting `ERROR_NO_MORE_FILES`) and the shim
  recorded `contained, 0 entries` for the directory. The test failed on the
  presence assertion, and the counter named the branch. So the fix removes the
  leak, and the source column catches the fallback even when there is no leak
  left to see.
- **Unmutated.** Passes: `director, 1 entry`, the served canary present, the
  unserved canary absent.

**That it took two mutations is itself the measurement**, and it is why this
section leads with "latent, not live" rather than burying it here. Neither
mutation alone reaches the branch: declining the directory only gets an open
that `RootMap::decide` denies, and reverting the deny only gets a handle the
client still claims and routes. The fall-through needed both, and the second
is a seal that belongs to a different gate entirely. That does not make the
branch harmless — it was one predicate change away from live, behind a
document asserting it could not happen — but no reader should take this fix
as having closed a hole a game could have fallen into.

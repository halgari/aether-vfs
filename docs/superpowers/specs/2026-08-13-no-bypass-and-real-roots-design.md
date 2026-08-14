# No Bypass and Real Roots — Design Spec

**Date:** 2026-08-13
**Status:** Approved, not implemented. Stage 2 of the five-stage plan in
[2026-08-13-pluggable-providers-design.md](2026-08-13-pluggable-providers-design.md).
**Scope:** Windows-only. This is entirely about the shim/director boundary.

## 1. Goal

Two things, in this order:

1. **No bypass.** If a path resolves under a managed root, every NT operation on
   it is answered by the director. The real filesystem under a managed root is
   unreachable by any spelling, by any process in the session.
2. **Real roots.** A session virtualizes several filesystem locations, each with
   exactly one provider, addressed as `(RootId, root-relative path)`.

Stage 1 put `RootId` in the type system with every call site passing
`RootId::DEFAULT`. This stage makes it mean something — but not before the
bypass work, for the reason in §7.

## 2. Locked decisions

| Fork | Decision |
|------|----------|
| Bypass scope | **Every byte routes through the director.** `Redirect` and `Serve` are deleted, not narrowed. |
| Real files under a root that no provider serves | **Invisible.** The root is fully virtual; mount a `disk` provider to expose a real tree. |
| Published snapshot | **Deleted as a shim input.** All metadata routes. The shim keeps one local predicate: is this path under a managed root? |
| DRM/identity exceptions | **Closed completely.** No allow-list, no fallback. |
| Writes | **Inside the invariant.** 2a absorbs the write path from what was Stage 3, because the fall-through cannot be closed without a replacement. |
| Ordering | **No-bypass (2a) before real roots (2b)**, and within 2a, the write path before closing the bypass. |

### The cost question, answered before it is asked

Routing everything is not a new cost. `try_fuse_create` already runs *before*
the `decision_for` match (`vfs-shim/src/hook.rs:1044`), so when the FUSE client
is active it takes every under-root open and `Redirect`/`Serve` are never
reached — they are the fallback for when the client failed to initialise. Every
figure in `rust/docs/benchmarks/` (the A/B/C optimisation series, the bulk
arena, spin-then-wait) measured the ring path, not the redirect path.

The work here is mostly deletion. The redirect fast path could only ever serve
the disk provider anyway: `Decision::Redirect` carries a real NT path, which a
`memory` provider or a host-language provider does not have.

## 3. The invariant

> For any path P and any process in the session, if P resolves under a managed
> root, every NT operation on P is answered by the director.

### The invariant covers writes, which forces a dependency

Writes are inside the invariant, not outside it. That creates a hard
dependency: the director cannot serve a write today. `Director::open` rejects
`OPEN_WRITE`, `ring_dispatch` implements no write opcodes, and no provider
implements `write_at`. The shim's fall-through at `hook.rs:797` — "Writes may
fall through for shim-local overlay redirect" — is load-bearing until something
else can serve them.

So **2a absorbs the write path** from what was Stage 3. Closing the
fall-through without a replacement would mean the game cannot write its INIs,
logs, or saves.

### 2a runs in two phases

The two risky areas — the write path, and closing the DRM exceptions and
escapes — are independent, and failure in either is much cheaper to diagnose
when they land separately.

- **Phase 2a-i — the write path.** `Director::open` accepts `OPEN_WRITE`;
  `ring_dispatch` gains `OP_WRITE`, `OP_SETATTR`, `OP_RENAME`, `OP_DELETE`,
  `OP_MKDIR`; `DiskProvider` implements the write half of the contract;
  `OverlayProvider` gains copy-up and is promoted to `ReadWrite`; the director
  owns append cursors. The shim's fall-through still exists but routed writes
  no longer use it.
  *Gate:* writes work end-to-end through the director; the game still launches.

- **Phase 2a-ii — close the bypass.** Delete the legacy decision paths and the
  write fall-through, make the root fully virtual, canonicalise paths and close
  the escapes, remove the DRM exceptions, and build the canary suite and the
  open-count reconciliation.
  *Gate:* the full acceptance criteria in §8.

### 2a-ii runs in five gates, and the order is the point

Phase 2a-i found **nine** distinct places where the correct path silently did
not work and traffic quietly took the bypass instead — a cache refusing writes
it advertised, four combinators doing the same, a shim never forwarding create
disposition, append handles starting at zero. Every one was invisible until
something forced it into the open. The fall-through has been absorbing that
entire class of defect.

Removing it in one step would mean a launch failure has a dozen candidate
causes. So each gate removes exactly **one** class of bypass, and the
reconciliation counter proves the previously-closed classes stayed closed.

| Gate | Removes | Failure points at |
|---|---|---|
| **1. Measure** | nothing | — |
| **2. Canonicalise** | escapes via alternate path spellings | path resolution |
| **3. Virtualise** | `NotFound`/`Dir` → passthrough, and the legacy `Redirect`/`Serve` decision paths | the read/metadata routing |
| **3.5. Real roots (2b)** | nothing — adds the second root | — |
| **4. Writes** | the shim's write fall-through | the write path |
| **5. DRM** | the four host-tree filename exceptions | Steam interaction, and nothing else |

### Why 2b moved in between gates 3 and 4

Gate 1's deep-session baseline established that **Skyrim's saves are invisible
to the counters**: they travel through an NTFS junction
(`Documents\My Games` → a real directory) that lies outside any managed root,
so the shim tags them `outside-root` and neither `Routed` nor any fall-through
class sees them. `FellThroughWriteFallback = 0` is therefore not evidence that
the write path holds for a real save — the game's saves are not virtualized at
all today.

So gate 4 would close the write fall-through against a path nothing real
exercises. It needs `Documents\My Games\Skyrim` to be a managed root first,
which is 2b.

2b sits *after* gate 3 rather than before gate 2 because gate 3 deletes the
legacy `Redirect`, `Serve`, `query_attributes`, and `merge_directory` decision
surfaces. Doing multi-root first would mean making all four multi-root aware and
then deleting them. Gates 2 and 3 need only one root, so this ordering pays for
multi-root exactly once, against the surviving code.

**Gate 1 changes no behaviour at all.** It builds the shim-vs-director
open-count reconciliation and the fall-through counter, surfaced in
`vfs stats`, then runs the game to get a baseline: how many opens take the
fall-through today, and by which paths. That converts the remaining bypass
from invisible to enumerated, and hands gates 2-5 a concrete list of what will
break — data rather than hope. Given how 2a-i went, starting from measurement
is worth a gate on its own.

**Gate 5 is deliberately last and alone.** Closing the DRM exceptions is the
highest-risk, least-predictable change in the phase — the code's own comment
concedes its "Steam Error" diagnosis is a hypothesis. Landing it by itself
means a launch failure has exactly one candidate cause.

### Fail-closed replaces fail-open

The current decision logic is explicitly fail-safe in the opposite direction.
`vfs-redirect/src/lib.rs:47`:

> Fail-safe: any path that is malformed, outside the root, or does not
> positively resolve to a virtualized file yields `PassThrough`.

Uncertainty currently resolves toward the real filesystem. This stage inverts
that: under a managed root, uncertainty resolves toward the director.

Three consequences, each a behaviour change:

- **FUSE init failure becomes a launch failure.** Today `fuse_client::global()`
  returning `None` means every path passes through — the game runs completely
  un-virtualized and nothing reports it. This is the largest bypass in the
  system and it is silent.
- **`NotFound` under a root returns not-found.** The real filesystem no longer
  answers.
- **`Dir` under a root returns a director-served handle.** No real directory
  handles are issued for paths under a root.

### One predicate

The shim currently makes five independent policy decisions: `decide()`,
`query_attributes()`, `merge_directory()`, the DRM filename match, and
`fuse_root_directory` handling. Each is a place an escape can hide, and each
would separately need to become multi-root aware in 2b.

After 2a the shim decides exactly one thing: **is this path under a managed
root?** Everything else is the director's answer. One predicate can be
enumerated against and tested exhaustively; five cannot.

## 4. What is deleted, what survives

| Component | Fate |
|---|---|
| `Decision::Redirect` / `Serve` / `Deny` | Deleted. The enum collapses to under-root or not. |
| `AttrDecision`, `query_attributes`, `merge_directory` | Deleted — metadata routes. |
| `vfs-redirect` snapshot-consuming logic | Deleted (~1,000 lines). `RootMap` survives and becomes multi-root in 2b. |
| `vfs-shim`'s `zipserve` legacy synthetic path | Deleted — the FUSE synthetic-handle path supersedes it. |
| DRM filename exceptions (`hook.rs:751-795`) | Deleted. See §6. |
| `vfs-shared` snapshot / seqlock / builder | **Retained.** Removing the shim as a consumer is in scope; retiring the crate is not — see below. |

**Why `vfs-shared` stays.** `vfs-server` and `xtask-descriptor` still consume
it. `vfs-server` is not the product path, but its own docs record that it is the
stable baseline the published benchmark numbers are measured against, and that
it should be retired only once the benchmark can express the same thing against
the director. That is not this stage's job.

The shim **keeps** its synthetic-handle table, section mapping, and
`NtReadFile`/`NtWriteFile` routing. That is the surviving path, and it is the
one the benchmarks already measure.

## 5. Escapes

### The vectors

| # | Vector | Example |
|---|---|---|
| 1 | 8.3 short name | `C:\Games\SKYRIM~1\Data\x.esp` |
| 2 | Extended-length prefix | `\\?\C:\Games\Skyrim\…` |
| 3 | NT device path | `\Device\HarddiskVolume3\Games\Skyrim\…` |
| 4 | Volume GUID path | `\\?\Volume{…}\Games\Skyrim\…` |
| 5 | Handle-relative open | `OBJECT_ATTRIBUTES.RootDirectory` outside the root, relative name into it |
| 6 | CWD-relative | `Data\x.esp` |
| 7 | Junction / reparse point | a junction into or out of the root |
| 8 | Hardlink | a link outside the root to a staged file inside it |
| 9 | UNC / subst / mapped drive | `\\localhost\C$\Games\Skyrim\…`, `Z:\` subst to the root |
| 10 | Unicode form, trailing dots or spaces | `Data.` ≡ `Data` |
| 11 | Alternate data stream | `x.esp:stream` |
| 12 | `.` / `..` components, trailing separators | `…\Data\..\Data\x.esp` |
| 13 | Handle opened before the root registered | enumeration on a stale real directory handle |
| 14 | Child process without the shim | a game-spawned helper sees the raw tree |

Vector 6 is not hypothetical: CWD-relative opens being undecodable is what
produced an empty load order previously.

### Resolution strategy

Canonicalise rather than pattern-match. Every under-root candidate resolves to
one canonical form:

- resolve the volume device prefix once at session start
  (`\Device\HarddiskVolumeN` ↔ `C:`),
- expand 8.3 names,
- strip extended-length and volume-GUID prefixes,
- fold `.` and `..` and trailing separators,
- split off any alternate-stream suffix,
- case-fold as today.

Canonicalisation is cached keyed on the raw input string. The existing
instrumentation already shows opens repeat heavily during load, so the cache
should absorb nearly all of the cost.

Handle-relative opens (5) reuse machinery the shim already has: `record_path`,
`record_identity`, and `tag_under_root` track handles the shim issued. For a
handle the shim never saw, it queries the OS for the final path rather than
guessing.

### Undecodable paths become errors

`note_undecodable` exists today as a counter. Under fail-closed: attempt OS
canonicalisation; if that fails and any evidence relates the path to a root,
deny and log loudly. A nonzero undecodable count fails the test run.

### Proving closure — the dual canary suite

Containment is a negative property, so the evidence has to be a negative test.

- **Negative canary.** A real file on disk under the managed root that no
  provider serves. A fixture attempts all fourteen spellings. **Every attempt
  must fail with not-found.** Any spelling that reaches it fails the suite and
  names the spelling.
- **Positive canary.** A file the provider *does* serve, reached via the same
  fourteen spellings. **Every attempt must succeed** with byte-identical
  content.

The positive canary exists because the cheap way to pass the negative test is to
break legitimate access. Both run in one fixture (`vfs-fixture-escape`) under a
real session and report a 14 × 2 matrix.

Vectors needing setup that is scriptable without admin — junctions via
`mklink /J`, `subst` for mapped drives, same-volume hardlinks — are set up by
the harness. Any vector that genuinely cannot be constructed in a given
environment is **reported as unbuildable, never silently skipped**: a skipped
containment test that reads as a pass is how this property rots.

### The continuous check

The shim counts opens it classified as under-root; the director counts opens it
received. The invariant is:

```
shim `routed`  ==  director `opens_ok` + director `opens_err`
```

**Not `opens_ok` alone.** The shim's `Routed` outcome means *the director
answered* — including answering with a legitimate error. A game re-opening a
file it just deleted, or issuing a second `CREATE_NEW` against an existing path,
is correctly routed and correctly refused; nothing fell through to disk.
Measured on the write-path fixture: `routed = 12`, `opens_ok = 9`,
`opens_err = 3`.

Comparing against `opens_ok` alone would fail on every run containing a
legitimate error, and the natural response — weakening the assertion — would
destroy the guarantee. Any drift in the sum is a bypass by definition.

**Directory creates are out of scope for both counters.** `OP_MKDIR` reaches
`Director::mkdir`, which never calls `record_open`; shim-side, `try_fuse_mkdir`
never calls the classifier. The omissions cancel, so the invariant holds — but
that is a property of the current wiring, not a guarantee, and a one-sided
change to either would turn a cancelled pair into phantom drift.
Surfaced in `vfs stats` and asserted in the end-to-end test, this turns the
invariant from something proven once into something monitored — which matters
because escapes are reintroduced by ordinary refactoring, not by malice.

## 6. The DRM exceptions, and the risk

Four host-tree filenames currently trampoline to the real filesystem
(`vfs-shim/src/hook.rs:751-795`): `steam_appid.txt`, `SkyrimSELauncher.exe`,
`steam_api*.dll` (under `keep_host_steam_api`), and `SkyrimSE.exe` (unless
`fuse_skyrim_exe`).

**They are closed completely. No allow-list, no fallback.** The Steam host tree
is instead mounted as a `disk`-provider root, so those files are served by the
director from real bytes.

The code's own comment records the hypothesis this rests on:

> Serving it through FUSE was observed to produce "Steam Error"; the cause is an
> open that fails to resolve (see `tramp_create_abs` and
> `STATUS_OBJECT_NAME_NOT_FOUND` on FUSE-relative OA), not an integrity check.

and warns explicitly against the competing theory:

> Steam does NOT compare the in-memory image against the on-disk PE. […] Do not
> "fix" anything here on the theory that the mapped image must match disk.

So the plan is to fix FUSE-relative `OBJECT_ATTRIBUTES` resolution and route
these paths. **That hypothesis may be wrong.**

**Contingency.** A real Skyrim launch is part of 2a's gate. If it fails with the
exceptions closed, implementation stops and reports the `drm_exe_trace` output
and the diagnosis. A bypass is not quietly reintroduced to make the gate green,
and the stage is not declared done with a game that does not launch.

## 7. Decomposition

### Stage 2a — No bypass

Single root throughout, in the two phases described in §3: first the write path
(so there is somewhere for writes to go), then closing the bypass — FUSE
routing mandatory, legacy decision paths deleted, root fully virtual, paths
canonicalised and escapes closed, DRM exceptions removed, canary suite and
open-count reconciliation built.

### Stage 2b — Real roots

`RootId` threaded through the director and shim; `RootMap` becomes multi-root;
one provider per root enforced, deleting `Director`'s layer-ordered mount merge
and the `layer` config field; the root folded into cache keys
(`vfs-cache/src/provider.rs` carries a comment marking this); named roots in
config.

### Why this order

2a collapses five decision surfaces to one predicate, so 2b's multi-root work
modifies one code path. Reversed, `Redirect`, `Serve`, `query_attributes`, and
`merge_directory` would each be made multi-root aware and then deleted — waste,
and it would triple the surface the canary suite must cover.

## 8. Stage 2a acceptance criteria

1. Canary matrix green for **read, write, metadata, and enumeration** access:
   14 spellings × 2 canaries. Unbuildable vectors reported as unbuildable,
   never silently skipped. A write to the negative canary must be **blocked**,
   and must not create a file on the real filesystem under the root.
2. Zero undecodable paths across a full game load.
3. Under-root open count at the shim equals open count at the director, for
   reads and writes alike. Any nonzero fall-through count fails the gate.
4. A write through the director is visible to a subsequent read through the
   director, and lands where the provider graph says — not on the real
   filesystem under the root.
5. FUSE init failure aborts the launch, with a test asserting it.
6. Skyrim launches under `tools/gamectl.ps1`, shows the expected load order,
   and writes its INI and save through the director.
7. No regression against the figures in `rust/docs/benchmarks/`.
8. Zero clippy warnings; full suite green.

## 8b. A prerequisite gate 4 must not start without

The director's `ops_write` and `total_write_bytes` counters exist in code but
are **printed by no report**, so there is currently no way to observe under-root
write activity short of grepping raw shim logs. Gate 4's entire job is driving
write fall-through to zero; it cannot begin without a readable number for the
writes that *did* route. Surface both counters alongside the open counts before
gate 4 starts.

## 9. Stage 2b acceptance criteria

1. A two-root Skyrim session — game directory and `Documents\My Games\Skyrim` —
   with a different provider on each.
2. The same relative path under two roots resolves independently, including
   through the block cache (the collision the Stage 1 comment records).
3. `Director` has no mount merge; `layer` is gone from config.
4. The canary suite passes against every root, not just the first.
5. No regression against recorded benchmarks; zero clippy; suite green.

## 10. Deferred

- Retiring `vfs-server` and the `vfs-shared` snapshot machinery, which needs the
  benchmark rebased onto the director first.
- **What remains of the old Stage 3.** 2a absorbs the parts the invariant
  needs: write opcodes, `DiskProvider` writes, `overlay` copy-up, append
  cursors, `ST_READ_ONLY`. What is *not* absorbed and stays deferred:
  `dry_run_writes` and the rejected-write discovery workflow, `FLAG_WRITE_BULK`
  (performance, not correctness), the read-write `memory` provider, and the
  write half of the gRPC `remote` provider. Stage 3 shrinks to those.
- Hook-boundary `catch_unwind`, still unimplemented (see the Stage 1 spec).

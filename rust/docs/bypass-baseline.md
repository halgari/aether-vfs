# Bypass baseline: Gate 1

**Provenance: this is a real game run, not a fixture.** Skyrim Special
Edition was launched under `skyrim-live` via `tools/gamectl.ps1`, driven to
the main menu, dropped into the world with `coc riverwood`, played for
several minutes, and quit cleanly through the in-game console. The numbers
below come from that run's shim report and `skyrim-live`'s own stderr, not
from `vfs-directord`'s e2e fixture tests. A second, shorter live session
(load-an-existing-save, described under "What didn't work" below) produced
an independent reconciliation that is included as a cross-check.

This document is what gates 2-5 are measured against. It records, for the
five fall-through classes and `Denied`, which count is real (occurred in this
run) and which is zero because this session never exercised that path. That
difference matters: a zero here is not evidence a bypass is closed.

## Session

| | |
|---|---|
| Date | 2026-08-13 |
| Game | Skyrim Special Edition, version 1.6.1170.0 (Steam AppID 489830) |
| Content source | `C:\tmp\skyrimse.zip`, ~15 GiB Stored zip, root prefix `Skyrim Special Edition` |
| Launch | `SkyrimSE.exe` directly (`VFS_SKYRIM_LAUNCH` default, no SKSE, no `VFS_SKYRIM_MODS` overlay) |
| Managed root | `C:\tmp\skyrim-runtime` (wiped and re-staged before launch) |
| Overrides / saves / profiles | `C:\tmp\skyrim-data\{overrides,saves,profiles}` (persist across runs) |
| Binaries | `cargo build --release` for `vfs-shim-dll`, `vfs-payload` (separate workspace), and `vfs-directord --bin skyrim-live`; DLL freshness verified by grepping the built `vfs_shim_dll.dll` for `under-root open outcomes` before launch |
| Steam client | already running, settled (~128,000s uptime), offline mode active — `skyrim-live` skips the online CM-Connected wait in that mode and talks to the client via local IPC only; this is `skyrim-live`'s existing, documented behaviour, not something introduced for this measurement |
| Stats config | `VFS_SHIM_STATS_LOG=C:\tmp\skyrim-data\perf\bypass-baseline-shim-stats.log`, `VFS_SHIM_STATS_INTERVAL_MS` unset (default 250ms — session ran ~245s, well past it) |
| Wall time | ~245s from launch to the process exiting after `qqq` (includes zip-staging overhead before the ring starts counting; `io_mark_launch()` marks t=0 for the traffic below) |

## What happened

1. `tools/gamectl.ps1 -Action launch` started `skyrim-live.exe` fully
   detached (`Start-Process`, not a job-object child of the driving shell;
   see "Code changes" below for why that distinction matters).
2. Reached the main menu (screenshot confirmed), opened the console
   (`` ` ``), and ran `coc riverwood` directly from there. That's the
   known-good path (see the `skyrim-empty-load-order` project note): a `New
   Game` would add a long scripted intro, and `coc` mid-intro does not
   reliably take.
3. Loaded into Riverwood. A first-run "Enable Survival Mode?" dialog
   appeared and did not respond to keyboard (`Enter`) or to a mouse click
   dispatched through `gamectl.ps1`'s new `click` action (see "Harness
   limitation found" below). Bypassed by opening the console and running
   `qqq`, which reliably reaches the game regardless of what modal is on
   screen.
4. `skyrim-live` detected the game process exit, wrote a final I/O dump, and
   exited itself.

Total session: main menu → console → `coc riverwood` → ~245s in Riverwood
(world rendered, NPCs present, no combat/inventory/save exercised) → `qqq`.

### What didn't work (recorded, not papered over)

An earlier attempt loaded an existing save via `CONTINUE` instead of `coc`.
That save references ~50 Creations Club plugins not present in this zip
image. After confirming the "content is not present, continue loading?"
dialog, the game's CPU usage climbed continuously (593 → 664+ CPU-seconds
over the observation window, `Responding=True`) while file I/O stayed
completely flat at `routed=383` for well over 90 seconds. That pattern fits
the engine validating a very long missing-plugin list, not a VFS hang:
`hookstats`'s async-I/O counters showed 0 async opens and 0 in-flight
section fills, ruling out the known synthetic-handle completion hang noted
in an earlier gate. Abandoned in favour of a fresh session rather than
waited out indefinitely, given the time budget for this task. That aborted
attempt's numbers are still real and still reconcile cleanly (see
"Cross-check" below): the game got far enough to validate a full 6-master,
~50-plugin load order before getting stuck on an unrelated first-party
dialog.

### Harness limitation found (and fixed, additively)

Two native dialogs ("content is not present… continue loading?" and "Enable
Survival Mode?") did not respond to `SendInput` keyboard events, unlike every
other menu in this session (main menu navigation, the "Continue from your
last saved game?" prompt, and the in-game console all worked correctly with
the existing `key`/`type` actions). Investigation: `gamectl.ps1` had no mouse
support at all. A first attempt to add one (`SetCursorPos` + legacy
`mouse_event`) silently failed the same way `SendKeys` fails for keyboard:
the cursor sprite rendered by the game never moved between screenshots
regardless of where `SetCursorPos` pointed, because Skyrim's Scaleform UI
tracks its own cursor position from real input events, and `SetCursorPos`
alone generates none. Fixed by sending the click through `SendInput` with
`MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE` (normalized to the virtual
screen), matching the same pattern the file's own header comment already
prescribes for keyboard. That fix is in `tools/gamectl.ps1` now, but even
the corrected version did not move the in-game cursor for these two specific
dialogs; they may be a native `MessageBoxMenu` variant that reads
gamepad/controller input rather than mouse or the standard menu-accept key.
Not chased further; the console hotkey bypass is reliable and was already
needed for `qqq`.

## Results

### Under-root open outcomes

Parsed from the shim's `VFS_SHIM_STATS_LOG` report at exit
(`under-root open outcomes:` section).

| Outcome | Count | Gate that closes it | What's expected to break when it does |
|---|---:|---|---|
| **Routed** | 392 | — (this is the non-bypass path) | — |
| FellThroughRedirect | 0 | gate 3 | not exercised this run — see "Zero is not closed" below |
| FellThroughServe | 0 | gate 3 | not exercised this run |
| FellThroughPassthrough | 0 | gates 2-3 | not exercised this run |
| **FellThroughDrmException** | **16** | **gate 5** | see below |
| FellThroughWriteFallback | 0 | gate 4 | not exercised this run (no save/write attempted) |
| Denied | 0 | n/a (not a fall-through class) | not exercised this run |

Total under-root opens: 408 (392 routed plus 16 fell-through; every other
class is zero in this run).

#### `FellThroughDrmException`: the one non-zero fall-through class

16 opens, all at process/DLL-load time, before the shim's hooks are the only
path in:

```
      15x  \??\c:\tmp\skyrim-data\stage\vfs-stage-25800\skyrimse.exe
       1x  \??\c:\tmp\skyrim-runtime\steam_appid.txt
```

This is the documented DRM filename exception in `hook.rs`
(`steam_appid.txt`, `SkyrimSELauncher.exe`, `steam_api*.dll`,
`SkyrimSE.exe`). The staged host EXE has to be a real file on disk before
`CreateProcess` runs and the shim is even injected, and `steam_appid.txt` is
Valve's own documented override that `SteamAPI_Init` reads directly.

Gate 5 closes this class, and both of these opens need a different answer
when it does: the staged EXE either needs to be served through the ring
before the loader's own `CreateProcess` (a real ordering constraint, not
just a flag flip), or the exception must stay narrower than "any of these
four filenames, anywhere under root." `steam_appid.txt` still needs to
resolve to a real file for Steam's DRM handshake to succeed. Expect gate 5
to either narrow this list's scope (path-qualify it instead of matching on
filename alone) or prove that routing these specific opens through the ring
doesn't break the ordering `CreateProcess`/`SteamAPI_Init` depend on. If
gate 5 flips this to `Routed` without addressing the ordering constraint,
the game will fail to launch at all, blocked on its own DRM check, rather
than fall back. That failure mode is the canary that the ordering issue was
missed.

#### Zero is not closed

The previous phase (2a-i) found nine real fall-through occurrences across
`Redirect`/`Serve`/`Passthrough`/`WriteFallback`. None of those four classes
shows up in this session, and none showed up in the `vfs-directord` e2e
write-path fixture either (Task 4's report: fall-through map empty in both
write tests). Two independent zero-readings read as "this scenario doesn't
trigger them," not "they're gone." This session never saved, never opened
the inventory or a container, never fast-travelled, and ran for under four
minutes; the nine occurrences from 2a-i almost certainly need a save/write
exercised and a longer, more varied session (menus beyond the pause menu,
quest scripts, a dungeon load) to reproduce.

Gates 2-4 should not treat these zeros as a clean bill of health. They are
gaps in this baseline's coverage. Closing them will need a session that
specifically exercises a save (for `WriteFallback`) and more varied
navigation (for `Redirect`/`Serve`/`Passthrough`), not just a longer idle
dwell in Riverwood.

### Director reconciliation

`skyrim-live` embeds the director directly (`vfs_director::Session`) rather
than running it behind the `vfs-directord` gRPC daemon, so there is no `vfs
stats` endpoint to query for a live game run. `skyrim-live` was extended,
additively (see "Code changes" below), to print the same
`io_stats::open_totals()` / `rejected_writes()` numbers the gRPC `stats` RPC
exposes for the daemon case, directly to its own stderr:

```
vfs-io opens: ok=77 err=315 (reconciliation target ok+err=392) rejected_writes=0 distinct path(s), 0 total
```

| | |
|---|---:|
| Director `opens_ok` | 77 |
| Director `opens_err` | 315 |
| Reconciliation target (`opens_ok + opens_err`) | **392** |
| Shim `routed` | **392** |
| **Drift** | **0** |
| Rejected writes | 0 distinct paths, 0 total |

Per Task 4's finding, the invariant is `routed == opens_ok + opens_err`, not
`routed == opens_ok` alone: `opens_err` includes legitimate negative answers
the director gave (a missing Creations-Club master is a real `STATUS_...`
error, correctly `Routed`, correctly counted as `opens_err`, not a bypass).
That is mostly what the 315 here is: this session's Riverwood load order
references ~50 Creations Club plugins this zip image doesn't have, and each
miss is a real round trip to the director that came back negative.

### Cross-check: the aborted continue-save session

The earlier, abandoned attempt (see "What didn't work") also reconciled
cleanly before it stalled on the unrelated missing-content dialog:

| | |
|---|---:|
| Shim `routed` | 383 |
| Director `opens_ok` | 71 |
| Director `opens_err` | 312 |
| Reconciliation target | 383 |
| Drift | 0 |
| Fall-through outcomes | none nonzero (same pattern as the main run) |

Two independently-launched live sessions plus the existing `vfs-directord`
e2e fixture tests (routed=12, ok=9, err=3, drift=0) now agree on the
invariant. Three-for-three, no exceptions found in this gate.

### Supporting numbers (context, not part of the invariant)

From the shim's general hook-call counters, same run:

- 596,627 total intercepted NT calls (`NtCreateFile`, `NtReadFile`, etc.)
  across the whole process lifetime; only 392 (0.1%) were `rooted` (served
  from the VFS). This matches the earlier debug-vs-release benchmark's
  finding that the shim detours far more traffic than ever reaches the
  director, most of it legitimate passthrough to `System32`/CRT/driver paths
  outside every managed root, which the classifier correctly never counts
  (see "A path outside every managed root... must record nothing" in
  `hookstats.rs`).
- Final `io_stats` snapshot at exit (t+245.0s): `getattr=4040 readdir=5
  open=392 read=26430 close=60 err=315 bytes=1400.27 MiB paths=613`. The two
  biggest single files: `data\skyrim - textures5.bsa` (813.6 MB, 1065 reads,
  330.4 MiB delivered) and `data\skyrim.esm` (249.8 MB, 240 reads, 238.2 MiB
  delivered).

## A blind spot these counters cannot see

`vfs-payload`'s `create_hook`/`open_hook` (`rust/crates/vfs-payload/src/lib.rs`,
roughly lines 245-308) consult an early redirect table
(`Config::redirects`, populated from `RunConfig::preinit_redirects`) before
anything else runs. When `match_redirect` finds an entry (lines 261-271 for
`NtCreateFile`, 295-301 for `NtOpenFile`), the open is redirected straight
through the original ntdll trampoline (`orig(...)`) and the hook returns
immediately, before `secondary_create`/`secondary_open` (the full-shim
dispatch that `install_late` wires up) ever runs.

That matters because both halves of this baseline's instrumentation sit
downstream of that dispatch: the shim's under-root open outcomes classifier
(the `Routed` / `FellThrough*` / `Denied` counts above) and the director's
`record_open` (the `opens_ok`/`opens_err` reconciliation) each only see
opens that reach `secondary_create`/`secondary_open`. An open matched by the
preinit redirect table reaches neither. It doesn't increment `routed`,
doesn't land in any `FellThrough*` class, and never reaches the director,
so it doesn't show up as `Denied` either. Drift stays 0 and every
`OpenOutcome` stays 0 regardless of what that table does, because neither
counter can observe it.

Today the table is empty: `rust/crates/vfs-director/src/session.rs` (around
line 270) builds skyrim-live's `RunConfig` with `preinit_redirects: vec![]`,
so `redirect_count` is 0, `match_redirect` never returns `Some`, and this
path contributes nothing to any number in this document. The zeros and the
`drift == 0` recorded above are earned; they are not being laundered
through this table.

**Gate 5 warning:** this table is exactly the mechanism someone would reach
for to relocate the `FellThroughDrmException` handling (the
`steam_appid.txt` / `SkyrimSE.exe` / `SkyrimSELauncher.exe` /
`steam_api*.dll` exceptions in `hook.rs`), since it runs earlier than that
check and would look like it solves the ordering constraint described
above under "`FellThroughDrmException`". Doing that would not close the
bypass. It would relocate it to a place neither counter can see: the DRM
opens would stop appearing as `FellThroughDrmException`, the fall-through
table would read all-zero, drift would stay 0, and the same
real-filesystem access this whole exercise exists to eliminate would still
be happening, just silently. Gate 5 must not move the DRM filename
exceptions into the preinit redirect table. Whatever gate 5 does with
those four filenames has to stay visible to at least one of the two
counters this document relies on.

## What's portable to gates 2-5, and what isn't

The headline numbers above (`routed=392`, `opens_ok=77`, `opens_err=315`)
are specific to this session: this content image (`C:\tmp\skyrimse.zip`,
unmodified), and this navigation (main menu → `coc riverwood` → ~245s in
Riverwood, no save, no inventory/container access, no fast travel), against
a load order that references ~50 Creations Club plugins this zip image
doesn't ship. Most of the 315 in `opens_err` is exactly those ~50 missing
masters, each a real round trip to the director that came back negative.
Point gate 2-5's measurement at a different image, in particular a
complete image that actually has those Creations Club plugins, and
`opens_err` should collapse toward zero, because the director starts
saying yes instead of no. That is not a regression; it's the same
invariant holding against a different, more complete input.

**Portable comparison targets:** carry forward and expect gates 2-5 to
match these:
- `drift == 0` (`routed == opens_ok + opens_err`). This is the
  reconciliation invariant; it doesn't depend on which opens happened,
  only on every routed open being accounted for by the director.
- The per-class zero/non-zero *pattern* in the outcome table:
  `FellThroughDrmException` non-zero (until gate 5 closes it), the other
  four fall-through classes and `Denied` at zero for this scope of
  navigation (see "Zero is not closed" above: those zeros are a coverage
  gap, not a proof of closure).

**Not portable:** don't treat these as a target to reproduce:
- The absolute counts (`392`, `77`, `315`, `16`, and the 408 total). They
  are a deterministic function of the content set (which files and
  plugins exist) and the navigation performed (how long, which areas,
  whether a save happened). A different image or a different play session
  will produce different counts even with the bypass fully closed.

To compare absolute counts at all, a gate 2-5 run would have to hold the
content image, the load order, and the navigation script (same start
point, same duration, same actions) fixed against this session; otherwise
a count difference says nothing about whether the gate changed anything.

What establishes that these counts are a property of the image and
navigation, and not run-to-run noise, is the cross-check above: the
second, independently-launched live session against the *same* image
reproduced `opens_err=312` (`routed=383`, `opens_ok=71`) before it stalled
on an unrelated dialog. That's in the same neighborhood as the main run's
315, consistent with a shorter, earlier-terminated navigation over the
same missing-plugin load order. Two runs against the same image landing in
the same neighborhood is what makes "image-determined" a finding rather
than an assumption.

## Code changes (additive only; no routing behaviour touched)

- **`rust/crates/vfs-directord/src/bin/skyrim-live.rs`**: added
  `print_open_totals()`, called once at the final I/O dump and once per
  10-second heartbeat tick, printing `io_stats::open_totals()` and
  `io_stats::rejected_writes()` to `skyrim-live`'s own stderr. This is the
  only way to see the director-side reconciliation numbers for a live game
  run, since `skyrim-live` has no gRPC daemon to query. Pure stderr output;
  no decision path, redirect, DRM exception, or fallback was read or
  changed.
- **`tools/gamectl.ps1`**: added `launch` (starts `skyrim-live.exe` fully
  detached via `Start-Process`, with `VFS_SHIM_STATS_LOG` set so the child
  inherits it through `CreateProcessW`'s null-environment inheritance),
  `stats` (dumps the shim's outcome section and, if given `skyrim-live`'s
  stderr log, the director's `vfs-io opens: ok=.../err=...` line side by
  side), and `click` (mouse input via `SendInput`, needed once the two
  stuck dialogs above ruled out keyboard-only driving). No existing action
  was changed.

Neither change alters what the shim or director decide for any open. Both
are additive observability, consistent with the rest of this gate.

## Verification

- `cargo test --workspace`: 407 passed, 0 failed, 1 ignored (unchanged from
  the Task 4 baseline; this task added no new Rust tests, only stderr
  output and a driving harness).
- `cargo clippy --all-targets -- -D warnings`: clean.
- `cargo clippy --manifest-path crates/vfs-payload/Cargo.toml --target-dir target -- -D warnings`: clean.
- DLL freshness verified before the run: `vfs_shim_dll.dll` grepped for
  `under-root open outcomes`, `fell-through: passthrough`, and
  `VFS_SHIM_STATS_INTERVAL_MS` (all present) after a clean `cargo build
  --release -p vfs-shim-dll`, per the project's known DLL-staleness traps.

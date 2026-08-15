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

## Deep session: save, load, broader navigation (2026-08-13)

This section extends the gate-1 baseline above with the session it explicitly
called out as missing: a real in-game save, a reload of that save, and an
interior/exterior transition. It keeps the original run above intact for
comparison and adds a second data point rather than replacing the first.

**Headline finding, stated up front:** the save and load both worked, and
`FellThroughWriteFallback` reads 0 for this run — but that reading is **not**
evidence that a save write routes through the director. The save's I/O
never enters the region either counter observes at all. See "The save is
invisible to this baseline's instrumentation" below before treating this as a
clean bill of health for gate 4.

### What the session did

1. Confirmed prerequisites: Steam running and settled (~135,000s uptime,
   offline mode), session unlocked (foreground window was a normal
   application, `LockApp` present but suspended, not the lock screen).
2. Verified the release binaries against the working tree: `vfs_shim_dll.dll`
   and `skyrim-live.exe` were already newer than their last source commit;
   `vfs_payload.dll` predated its last commit by 8 minutes, so it was
   rebuilt — the rebuild was a no-op (`Finished in 0.01s`), because that
   commit only added a comment to `Cargo.toml`, confirmed by inspecting the
   commit's diff before trusting cargo's fingerprint.
3. **Removed the Survival Mode Creation Club plugin from the load order.**
   `Plugins.txt` (`C:\tmp\skyrim-data\profiles\LocalAppData\Plugins.txt`)
   turned out to hold no plugin lines at all — it's just the standard
   two-line header. The file that actually drives which Creation Club
   content this content set loads is `Skyrim.ccc`, at the root of the staged
   image (confirmed present in `C:\tmp\skyrimse.zip` at
   `Skyrim Special Edition/Skyrim.ccc`, 75 lines). Wrote an override at
   `C:\tmp\skyrim-data\overrides\Skyrim.ccc` — identical to the zip's copy
   with exactly one line removed, `ccQDRSSE001-SurvivalMode.esl` (line 17 of
   75) — which the director's mount composition (`zip` under `overrides`)
   serves in place of the zip's copy without modifying the zip itself. This
   is the file whose absence raised the "Enable Survival Mode?" dialog in the
   gate-1 baseline; every other line is unchanged, so the load order for
   this run differs from the shallow run's by exactly that one plugin.
4. Launched via `tools/gamectl.ps1 launch`, reached the main menu (confirmed
   by screenshot), opened the console, and ran `coc riverwood` — the same
   known-good entry point the gate-1 baseline used.
5. **No Survival Mode dialog appeared this time** (screenshot after the cell
   loaded: clear Riverwood exterior, HUD compass visible, no modal). This is
   the first empirical result of the plugin removal. One caveat, below.
6. **Saved in-game via the console**: `save deepsession1`. Chosen over the
   pause-menu Save option because console commands are the harness's proven
   reliable input path (menu navigation risks new, undiscovered blocking
   dialogs); this is still an in-game save through the engine's own save
   system, not a quit. Confirmed by screenshot (command echoed, save icon
   flash) and by the file landing on disk:
   `C:\tmp\skyrim-data\saves\deepsession1.ess`, 2,482,241 bytes, timestamped
   at the moment of the command.
7. **Transitioned to an interior cell**: `coc qasmoke` (a stock vanilla test
   cell, chosen because its editor ID is guaranteed valid in every Skyrim SE
   install regardless of load order, unlike a specific building interior
   that might depend on a mod). Screenshot confirmed a distinct interior
   (stone cave room, chests and barrels) — a real exterior→interior
   transition, pulling different loose files/BSAs than the Riverwood
   exterior.
8. **Loaded the save back**: `load deepsession1`. Screenshot confirmed the
   game returned to the exact Riverwood exterior view the save was taken
   from — direct visual proof the save round-tripped correctly, exercising a
   read of the file the previous step's write had produced.
9. **A second, different exterior** for broader navigation: `coc whiterun`.
   Screenshot confirmed a visually distinct area (mountain-pass terrain, a
   different HUD state), pulling yet another set of region assets.
10. **Clean exit**: console `qqq`. `skyrim-live` detected the process exit,
    wrote its final I/O dump, and exited itself — confirmed by
    `game process not found — final I/O dump:` in its stderr and by no
    `SkyrimSE`/`skyrim-live` process remaining afterward.

Total session: main menu → console → `coc riverwood` → save → `coc qasmoke`
(interior) → load → `coc whiterun` (exterior) → `qqq`, spanning ~496s from
launch mark to exit (`vfs-io t+495.9s` at the final dump), versus the
baseline's ~245s.

### What went wrong along the way (recorded, not papered over)

The console's known toggle-with-no-readable-state hazard recurred once,
exactly as the project's own notes warn. After the first `coc riverwood`,
a `GRAVE` intended to close the console was followed by a second `GRAVE`
before the save attempt; the second one landed on a console that had
already re-closed from an unrelated timing gap, so the subsequent
`type "save deepsession1"` went to the game as raw keystrokes instead of
console input. It opened the Favorites/Magic menu (visible in the
screenshot: `ALL`/`DESTRUCTION`/`RESTORATION`/`POWERS`/`ACTIVE EFFECTS`
tabs). Caught immediately because every command in this session was
screenshotted before submitting, not after — recovered with `ESC`, then
every subsequent console toggle was verified by screenshot (looking for the
blinking input caret) before typing anything further. No corrupted input
reached the game; the retried `save deepsession1` is the one that produced
the file on disk.

### Results

Parsed from `VFS_SHIM_STATS_LOG`
(`C:\tmp\skyrim-data\perf\deep-session2-shim-stats.log`) and `skyrim-live`'s
own stderr (`deep-session2-shim-stats.live-err.log`), via
`tools/gamectl.ps1 stats`:

```
=== shim: under-root open outcomes ===
under-root open outcomes:
  routed                                391
  fell-through: drm-exception            16

=== director: open totals ===
  vfs-io opens: ok=75 err=316 (reconciliation target ok+err=391) rejected_writes=0 distinct path(s), 0 total
```

| Outcome | Count | Same pattern as gate-1 baseline? |
|---|---:|---|
| Routed | 391 | yes (392 there; not portable, see below) |
| FellThroughRedirect | 0 | yes — still unexercised |
| FellThroughServe | 0 | yes — still unexercised |
| FellThroughPassthrough | 0 | yes — still unexercised |
| FellThroughDrmException | 16 | yes, identical breakdown (15× staged EXE, 1× `steam_appid.txt`) |
| **FellThroughWriteFallback** | **0** | yes — **but this run exercised a save, and it still reads 0; see below for why that is not the closure signal it looks like** |
| Denied | 0 | yes — still unexercised |

Reconciliation: shim `routed` = 391, director `opens_ok + opens_err` =
75 + 316 = 391. **Drift = 0.** The invariant holds again, on a session more
than 3x the wall-clock length of the baseline and with a save/load/multi-cell
navigation the baseline never attempted.

`rejected_writes=0 distinct path(s), 0 total`: the director never refused a
write this session either.

### The save is invisible to this baseline's instrumentation

This is the deliverable's most important finding, and it complicates the
headline question rather than answering it cleanly.

`skyrim-live` remaps Skyrim's save/profile location with real NTFS
junctions, not virtual mounts: `setup_my_games_junctions()`
(`rust/crates/vfs-directord/src/bin/skyrim-live.rs`, around line 985) links
`Documents\My Games\Skyrim Special Edition` (and its `Saves` subdirectory)
straight to `C:\tmp\skyrim-data\profiles` / `saves` at the filesystem level.
That is a genuinely different mechanism from `session.mount()` (used for the
zip and the `overrides` write layer): a junction is resolved by the OS's own
reparse-point handling, and it points at a path that is never inside the
managed root (`C:\tmp\skyrim-runtime`) the shim's classifier checks against
(`path_is_ours` / `is_under_root`, `rust/crates/vfs-shim/src/hook.rs` around
line 737). The shim's own log confirms this directly — both the save and its
temp file were tagged by the shim's classifier as outside the root it
tracks:

```
       3x  outside-root \??\c:\users\tbaldrid\documents\my games\skyrim special edition\saves\deepsession1.ess
       1x  outside-root \??\c:\users\tbaldrid\documents\my games\skyrim special edition\saves\deepsession1.ess.tmp
```

Consequently: the save write (and the load's read) never reaches the six-way
`OpenOutcome` classifier (`Routed`/`FellThrough*`/`Denied`) at all, in either
direction. It isn't `Routed` and it isn't `FellThroughWriteFallback` — it's
structurally outside the domain either counter observes, the same shape of
gap the gate-1 baseline already documented for the preinit-redirect table
("A blind spot these counters cannot see"). `FellThroughWriteFallback`
reading 0 here is therefore **not evidence that 2a-i's director-served-write
path held for a real save** — it's evidence that the real save never
attempted to go through that path in the first place, by a deliberate
architectural choice (saves/profiles are real host directories reached by a
real junction, precisely so tools like Windows Explorer and the game's own
save browser keep working without VFS involvement).

One caveat on causation for the plugin removal itself: `ccQDRSSE001-
SurvivalMode.esl` still shows up **routed 3 times** in this run's outcome
listing, meaning the engine still opened the plugin's file — removing it
from `Skyrim.ccc` did not stop the file from loading. What changed
observably is that its first-run "Enable Survival Mode?" prompt did not
fire. Both facts are drawn from a single run each way (gate-1 baseline had
the dialog with the entry present; this run didn't, with the entry absent),
so this establishes correlation, not proof — a plausible mechanism is that
`Skyrim.ccc` gates content recognition/first-run scripting rather than raw
plugin loading, but that's inference, not something measured directly here.

### Comparison: this run vs. the gate-1 baseline

| | Gate-1 baseline | This run | Changed? |
|---|---:|---:|---|
| Session | main menu → `coc riverwood` → ~245s idle → `qqq` | main menu → `coc riverwood` → save → `coc qasmoke` → load → `coc whiterun` → `qqq` | richer navigation |
| Load order | full `Skyrim.ccc` (75 entries) | `Skyrim.ccc` minus `ccQDRSSE001-SurvivalMode.esl` (74 entries) | **yes — content change, not a VFS change** |
| Wall time (launch mark → exit) | ~245.0s | ~495.9s | longer |
| Routed | 392 | 391 | not portable (different navigation/load order; see baseline's own caveat) |
| FellThroughRedirect | 0 | 0 | unchanged — still unexercised |
| FellThroughServe | 0 | 0 | unchanged — still unexercised |
| FellThroughPassthrough | 0 | 0 | unchanged — still unexercised |
| FellThroughDrmException | 16 | 16 | unchanged, identical breakdown |
| FellThroughWriteFallback | 0 | 0 | unchanged in count, but now known **structurally unreachable for saves** (new information, not closure) |
| Denied | 0 | 0 | unchanged — still unexercised |
| Director opens_ok | 77 | 75 | within noise for different navigation |
| Director opens_err | 315 | 316 | within noise |
| Reconciliation drift | 0 | 0 | invariant holds again |
| `getattr` / `read` / `bytes` / distinct paths | 4040 / 26430 / 1400.27 MiB / 613 | 8567 / 29693 / 1977.14 MiB / 671 | more content touched, consistent with richer navigation |
| Survival Mode dialog | appeared, forced `qqq` escape | did not appear | plugin removed (see caveat above) |
| In-game save performed | no | yes — `deepsession1.ess`, 2,482,241 bytes | new this run |
| Save reloaded | no | yes — confirmed by screenshot match | new this run |
| Interior/exterior transition | no | yes — Riverwood (ext) → QASmoke (int) → Riverwood (ext) → Whiterun (ext) | new this run |

### What gates 2-5 can conclude now, and what's still open

**Now established, that wasn't before:**
- The reconciliation invariant (`routed == opens_ok + opens_err`, drift 0)
  holds across a session with a real save, a real load, and multiple cell
  transitions — not just an idle dwell. Three-for-three becomes four data
  points against real live sessions, plus the e2e fixture.
- `FellThroughRedirect`, `FellThroughServe`, `FellThroughPassthrough`, and
  `Denied` remain at zero across *two* independently-scoped live sessions
  now (idle-dwell and save/load/multi-cell), which is somewhat stronger
  evidence they're genuinely rare in ordinary play than the single baseline
  run gave — but see below for what kind of navigation still hasn't been
  tried.
- The engine's own save/load cycle works correctly against this content
  image and this director (files write, and the game reads its own write
  back correctly) — that much is a real, positive result, independent of
  which counter does or doesn't see it.
- The Creations-content and Survival Mode dialogs are both avoidable by
  content-scenario changes (an unmissing-content save/`coc` origin for the
  first, a `Skyrim.ccc` edit for the second) rather than input automation,
  confirming the project's existing guidance on this point.

**Still open, and now more precisely characterized than "coverage gap":**
- **Gate 4's actual target (writes that fall through to real disk from
  *under the managed root*) was not exercised by anything in either session
  to date.** The one write this session performed — the save — is
  structurally outside gate 4's observation domain by design (the My Games
  junction), not merely unexercised. If gate 4 wants a data point for
  in-game writes under the managed root, it needs a different write to
  target: something that lands inside `C:\tmp\skyrim-runtime` proper (a
  shader cache write is the most game-driven candidate; the `overrides`
  write layer that `steam_appid.txt` uses is another, though that one is
  written by `skyrim-live` itself rather than the game). Whether *any*
  write went through the director's write path this session (successful or
  rejected) cannot be answered from the printed reports at all —
  `rejected_writes` counts only refused writes, and the director's
  `ops_write`/`total_write_bytes` fields (`rust/crates/vfs-director/src/io_stats.rs`,
  lines 39/43) exist but are never surfaced by `snapshot_report` or
  `print_open_totals`. That is itself a reporting gap worth closing before
  gate 4 starts, or gate 4 will have no way to see its own progress on
  under-root writes short of grepping raw shim logs for outside-root tags.
- `FellThroughRedirect`/`FellThroughServe`/`FellThroughPassthrough` are still
  zero after a session that added an interior cell and two additional
  exterior regions — a real increase in navigation breadth — so the case
  that they're rare is a little stronger, but combat, inventory/container
  access, fast travel, and a longer dwell in a busy location (a city
  interior with NPCs and vendor containers) are all still untried. The
  gate-1 baseline's caution stands: these are gaps in observed coverage,
  not proof of closure.
- Whether the Survival Mode dialog's removal really was caused by dropping
  the `Skyrim.ccc` line (as opposed to some other run-to-run variation) is
  inferred, not proven — one run each way is a correlation, not a
  controlled experiment.

### Code / harness changes this task made

- **`rust/docs/bypass-baseline.md`** (this file): this section only. No
  other part of the gate-1 baseline was edited.
- **No Rust source was changed.** `vfs-payload` was rebuilt (a no-op) purely
  to verify freshness per the project's DLL-staleness convention; no code
  was edited. `cargo test --workspace` and clippy were not re-run because
  nothing Rust changed — the 407-passing baseline from gate 1 stands.
- **`tools/gamectl.ps1`**: the working tree already carried an uncommitted,
  additive change before this task started — `TapShifted` (holds shift
  across a scancode tap, for typing an underscore) and a `type` action
  update to use it for `_`. That capability was **not used** for this
  session (the plugin-removal approach made it unnecessary — no console
  command typed here needed an underscore), and it does not touch dialog
  handling. Left in place as-is and included in this task's commit since it
  was already sitting in the working tree; it is a generic harness input
  capability, not a dialog-bypass mechanism.
- **Content-scenario file (outside the repo, not committed):**
  `C:\tmp\skyrim-data\overrides\Skyrim.ccc` — a copy of the zip's
  `Skyrim.ccc` with `ccQDRSSE001-SurvivalMode.esl` removed. This is picked
  up by `skyrim-live`'s existing `overrides` mount layer with no code
  change; it persists across launches (the `overrides` directory is never
  wiped) and should be considered part of this deep-session scenario's
  fixed configuration for any future run that wants comparable numbers.

### Verification

- No Rust files changed; `cargo test --workspace` / clippy were not
  re-run, per the note above (nothing to invalidate the gate-1 result).
- `tools/gamectl.ps1`'s only functional use in this task was existing
  actions (`launch`, `key`, `type`, `shot`, `stats`); no new PowerShell
  logic was added.
- Every console command in this session was screenshotted and read before
  being submitted with `ENTER`, including the one recorded miss above,
  which was caught by that same discipline rather than assumed to have
  worked.
- Save file existence and size verified directly on disk
  (`C:\tmp\skyrim-data\saves\deepsession1.ess`), not inferred from the
  console echo alone.
- Clean exit verified by both the absence of `SkyrimSE`/`skyrim-live`
  processes afterward and the `game process not found — final I/O dump:`
  line in `skyrim-live`'s own stderr.

## Gate 3, Task 6: re-measurement after the root became fully virtual

**Provenance: this is a real game run, not a fixture**, using the same
`tools/gamectl.ps1` driver and the same content image
(`C:\tmp\skyrimse.zip`) and managed root (`C:\tmp\skyrim-runtime`) as the
gate-1 baseline above. This section's own job is narrow: confirm
`FellThroughRedirect`/`FellThroughServe`/`FellThroughPassthrough` read zero
now that `RootMap::decide` denies `NotFound`/`Dir` instead of passing them
through (Gate 3, Task 5), and record what `FellThroughDrmException` and
`FellThroughWriteFallback` still look like so gates 5 and 4 have a number to
measure against.

### Session

| | |
|---|---|
| Date | 2026-08-14 |
| Game | Skyrim Special Edition, same install/content image as the gate-1 baseline |
| Launch | `tools/gamectl.ps1 -Action launch`, `skyrim-live.exe` detached, `VFS_SHIM_STATS_LOG=C:\tmp\skyrim-data\perf\task6-shim-stats.log` |
| Binaries | `vfs_shim_dll.dll` / `skyrim-live.exe` at `rust/target/release/`, both dated 2026-08-14 17:14-17:15 — built during this gate's own Task 5 work and unchanged since (this task edits only `crates/vfs-directord/tests/e2e.rs` and two docs, neither of which affects the shim/director/skyrim-live binaries); DLL freshness re-confirmed before this run by `Select-String` for `under-root open outcomes`/`fell-through: passthrough`/`VFS_SHIM_STATS_INTERVAL_MS` against the built DLL, all present |
| Navigation | main menu (screenshot) → console (screenshot, caret visible) → typed `coc riverwood` (screenshot, echoed) → `ENTER` → Riverwood loaded (screenshot: trees, mill, mountains, HUD) → ~142s dwell (a second mid-dwell screenshot confirms the world still rendering) → console (screenshot, caret visible) → typed `qqq` (screenshot, echoed) → `ENTER` |
| Wall time | ~232.0s from `io_mark_launch()`'s t=0 to the final dump (`game process not found — final I/O dump:` in `skyrim-live`'s stderr) |
| Clean exit | confirmed by the same two signals as the gate-1 baseline: no `SkyrimSE`/`skyrim-live` process remaining, and the "final I/O dump" line |

Every console command was screenshotted before and after typing, per the
project's own console-toggle-with-no-readable-state caution; no misfire
occurred this run (contrast the gate-1 deep-session's one recorded miss).

### Results

Parsed from `VFS_SHIM_STATS_LOG` (`task6-shim-stats.log`)'s single
`under-root open outcomes:` section, written once at exit, and from
`skyrim-live`'s own stderr (`task6-shim-stats.live-err.log`)'s final
`vfs-io opens:` line:

```
under-root open outcomes:
  routed                                392
  fell-through: drm-exception            16
           15x  \??\c:\tmp\skyrim-data\stage\vfs-stage-26956\skyrimse.exe
            1x  \??\c:\tmp\skyrim-runtime\steam_appid.txt

vfs-io opens: ok=77 err=315 (reconciliation target ok+err=392) rejected_writes=0 distinct path(s), 0 total
```

No other outcome label (`fell-through: redirect`, `fell-through: serve`,
`fell-through: passthrough`, `fell-through: write-fallback`, `denied`)
appears anywhere in the report — `render_outcomes` only prints a bucket that
has at least one entry, so their absence here is the same "zero" signal the
gate-1 baseline's own render used, not a parsing gap (confirmed directly:
`grep -c denied` on the whole log returns 0).

| Outcome | Gate-1 baseline | Task 5's own launch | **This run (Task 6)** | Gate that closes it |
|---|---:|---:|---:|---|
| Routed | 392 | 392 | **392** | — |
| FellThroughRedirect | 0 | 0 | **0** | gate 3 — reads zero in this configuration, not because the code path is gone; see the correction below for why and what gate 4 changes |
| FellThroughServe | 0 | 0 | **0** | gate 3 — same correction as `FellThroughRedirect` above |
| FellThroughPassthrough | 0 | 0 | **0** | gates 2-3 — the one of these three with a real claim to structural closure; see below for its own residual producers |
| FellThroughDrmException | 16 (15× staged exe, 1× `steam_appid.txt`) | 16, identical breakdown | **16, identical breakdown** | gate 5 — the DRM filename exceptions in `hook.rs`, untouched by this task |
| FellThroughWriteFallback | 0 (not exercised) | 0 (not exercised) | **0 (not exercised — no save attempted this run)** | gate 4 — `Engine::cow_seed`'s last-resort branch, untouched by this task |
| Denied | 0 | 0 | **0** | n/a — not a fall-through class; see below for what this zero does and does not mean |
| Director `opens_ok` | 77 | 77 | **77** | — |
| Director `opens_err` | 315 | 315 | **315** | — |
| Reconciliation target (`ok+err`) | 392 | 392 | **392** | — |
| Drift | 0 | 0 | **0** | — |
| Rejected writes | 0 | 0 | **0** | — |

**Absolute counts reproduced identically across three independent live
sessions now** (gate-1 baseline, Task 5's own launch, and this run) —
`routed=392`, `opens_ok=77`, `opens_err=315`, drift 0, identical
`FellThroughDrmException` breakdown. Per the gate-1 baseline's own caution,
absolute counts are a property of the content image and the navigation
script, not something gates 2-5 are expected to reproduce on a different
scenario — but the fact that three separate launches of the *same* image and
navigation land on the exact same numbers is what makes this reproducible
evidence rather than a one-off. The **pattern** (which classes are zero,
which are non-zero) is the portable comparison target, exactly as the
gate-1 baseline specified, and it now shows three of the five fall-through
classes flipped from "not exercised, still an open question" (gate-1
baseline's own framing) to "genuinely zero, by construction" — see below.

### Correction: why `FellThroughRedirect`/`FellThroughServe` actually read zero here

An earlier version of this section claimed these two were "closed this
task" because "`RootMap::decide` no longer passes `NotFound` through." That
explanation is wrong twice over, and the final whole-branch review of gate 3
caught it before it could propagate into gate 4's own baseline.

First, `RootMap::decide` still produces `Decision::Redirect` and
`Decision::Serve` today, unmodified by Task 5 — they come from the `File`
resolution arm (`crates/vfs-redirect/src/lib.rs:369-381`), not from the
`NotFound`/`Dir` arm that arm's neighbour, which Task 5 changed. `Redirect`
and `Serve` were never something the `NotFound` arm produced, so a change to
that arm cannot be why either bucket reads zero.

Second, and more consequential: `RootMap::decide` is not even the only
producer either bucket has. `vfs-shim::Engine` — the shim-local wrapper
`hook.rs`'s `decision_for` actually calls — produces `Decision::Redirect`
itself, above and independent of `RootMap::decide`, for two cases: every
overlay hit (`Engine::decide`, `crates/vfs-shim/src/engine.rs:193`) and
every under-root write that reaches the overlay
(`Engine::decide_open`, `engine.rs:234`). Both are live, ordinary code paths
today, not something this or any other task removed.

The real reasons these two read zero in every live session measured so far
are configuration and ordering facts, not a missing code path:

1. The shim's embedded snapshot in a live session is always the empty tree
   (`vfs-director::Session::serve`, `crates/vfs-director/src/session.rs:184-186`)
   — the FUSE ring to the director is the only real content path — so
   `RootMap::decide`'s `File` arm, which is the one source of `Redirect`/
   `Serve` that actually depends on `NotFound`'s sibling arms at all, never
   has anything to resolve to a `File` and never fires.
2. `hook::try_fuse_create` already serves or seals every under-root read
   that `fuse_client::vpath_under_root` recognises, before
   `decision_for`/`Engine::decide_open` ever runs — a recognised read either
   comes back `Routed` (director served it) or is sealed with
   `STATUS_OBJECT_NAME_NOT_FOUND` right there. **Since stage 2b task 5 that
   predicate is a `RootMap`**, the same canonicaliser
   `RootMap::decide` uses, so the five alternate spellings
   `rust/docs/escape-matrix.md`'s "second, structural finding" describes are
   now recognised here too and no longer reach `decision_for`. What still
   does: an open `try_fuse_create` explicitly falls through (the DRM
   exceptions, the write fallback), and a path under no declared root at
   all.
3. Of the opens that do reach `decision_for`, `outcome_recorded` (the
   out-param `try_fuse_create` sets before returning `None` — see its own
   doc comment) diverts the ones that would otherwise classify as
   `FellThroughRedirect` back into the bucket `try_fuse_create` already
   recorded: a DRM-exception open where the exception's own filename is
   also present in the write overlay (`steam_appid.txt`, written there by
   `skyrim-live` itself — see "Record a changed effect" below) computes
   `Redirect` in `Engine::decide_open` but is counted as
   `FellThroughDrmException`, not `FellThroughRedirect`, because
   `note_decision_outcome` suppresses the second count when `already` is
   set. The write-fallback case is the same shape: a write that falls
   through to the overlay is counted as `FellThroughWriteFallback` even
   though `Engine::decide_open`'s own answer for it is `Redirect`.

These are facts about this run's configuration and about the order two
different classifiers run in, not about `Redirect`/`Serve` having no
producer left. The distinction matters concretely for gate 4: gate 4's own
job is the write path, and the write path is one of `Redirect`'s two
producers (item 2 above, via `Engine::decide_open`, line 234). A future
baseline that still reads these two buckets as zero after gate 4 changes
that producer needs this section's reasoning to know that the zero is still
provisional, not to inherit "closed this task" as if the mechanism gate 3
built were the reason.

### `FellThroughPassthrough`: the one with a real claim to structural closure, and its own residuals

`FellThroughPassthrough` is different from the other two, and does get the
stronger claim: after Task 5, `RootMap::decide` has no code path left that
produces the generic `Passthrough` fall-through for an under-root
`NotFound`/`Dir` resolution — `Deny` is the only arm left besides
`Located::Outside`'s pass-through (see `rust/docs/escape-matrix.md`'s "Gate
3, Task 5" section). This run's zero for that class is close to a
**structural** zero — reinforced, not merely repeated, by the fact that the
matching unit/e2e tests (`real_on_disk_file_under_root_not_in_snapshot_is_denied`
and friends, plus this task's own `negative_expectation` assertion) prove the
same thing by direct construction rather than by absence of contrary
evidence.

"Close to" rather than "clean," though: `hook.rs`'s own accounting still has
two residual producers of an under-root `FellThroughPassthrough`, neither
touched by Task 5. `Engine::decide_open`'s write path answers
`Decision::PassThrough` directly, bypassing `RootMap::decide` entirely, when
there is no overlay configured at all (`engine.rs:215`) or when the
computed remainder is empty or absent (`engine.rs:217`, `220`) — a
configuration this project's own shipping launcher never hits (`skyrim-live`
always configures an overlay), but not something the type system or gate 3
rules out for a differently-configured session. And `Engine::map` itself
answers `None` — which every caller, `decide_open` included, treats as
"not under any managed root", i.e. `PassThrough` — while a re-entrant,
same-thread call lands during the very first decision this engine ever
makes, before that first call's own `RootMap` construction has finished
(`engine.rs:155-165`, the `MapInitGuard` reentrancy guard). Both are real,
narrow, documented edges, not evidence against this run's zero, but reason
enough to say "close to structural" rather than "airtight."

`FellThroughDrmException` and `FellThroughWriteFallback` do **not** get this
upgrade — both remain genuine coverage questions, not structural
guarantees, for the reasons the gate-1 baseline already gave and that still
hold: **gate 4** (`FellThroughWriteFallback`) needs a save/write under the
managed root proper to exercise at all (this run, like every live session to
date, never attempted one — see the gate-1 deep-session's own finding that
even a real in-game save is invisible to this instrumentation, because
`skyrim-live`'s save/profile junctions land outside the managed root
entirely); **gate 5** (`FellThroughDrmException`) is exercised every launch
by construction (the staged EXE and `steam_appid.txt` are required for the
DRM handshake to complete at all) and needs no further coverage work, but
remains open until gate 5 addresses the ordering constraint the gate-1
baseline already documented in detail (narrowing the exception's scope
versus routing these opens through the ring without breaking
`CreateProcess`/`SteamAPI_Init` ordering).

### An unchanged code path with a changed effect, recorded here for gate 5

The DRM-exception block itself (`hook.rs:1092-1130`) is untouched by this
gate — same four filenames, same `return None` after recording
`FellThroughDrmException`. What changed underneath it, silently, is what
happens *next*: after `try_fuse_create` returns `None` for one of these
opens, `create_hook`/`open_hook` still call `decision_for`
(`Engine::decide_open`) unconditionally to decide how to actually service
the open, and that answer is what gate 3 changed for an under-root path.

Concretely, for a **read** open under the managed root with nothing in the
overlay, `Engine::decide` now resolves `RootMap::decide` against the
(live-session, always-empty) snapshot, gets `NotFound`, and returns `Deny`
— where before this gate it returned `PassThrough` and the open reached the
real file on disk underneath. `steam_appid.txt` is exactly this shape: it
lives under the managed root, so its DRM-exception open is subject to this
change. The one recorded open of it in this run's own outcome table still
succeeds only because `skyrim-live` writes a copy into the write overlay
(`crates/vfs-directord/src/bin/skyrim-live.rs:149`, `490-500`) — `Engine::
decide`'s overlay check runs before `RootMap::decide` and finds it there,
answering `Redirect` into the overlay copy instead of ever reaching the now-
`Deny`ing snapshot path. The other 15 opens in this run (the staged EXE)
are unaffected by this change for an unrelated reason: the staging directory
is physically outside every managed root, so `RootMap::decide` never enters
its `NotFound`/`Dir` arms for it at all — `Located::Outside`'s `PassThrough`
still applies exactly as before.

This is not a bypass and not something this task fixes — the constraint was
already satisfied (`steam_appid.txt` was already written into the overlay
before this gate started) and the DRM exceptions themselves are explicitly
out of this gate's scope. It is recorded here because **gate 5 owns
removing these exceptions**, and gate 5's implementer needs to know what
they currently depend on to keep working: without the overlay copy, this
gate's own `Deny` change would have silently broken `steam_appid.txt`'s DRM
exception the moment it shipped, for a reason having nothing to do with the
DRM handling itself. Any future change to where or whether `skyrim-live`
seeds that overlay copy needs to account for this dependency, and gate 5's
own redesign of these exceptions inherits it directly.

**`Denied` reading zero here is exactly what `escape-matrix.md`'s "What the
shipping config's own launch does and does not demonstrate" section
predicts, not a surprise.** `skyrim-live.rs` mounts `DiskProvider::new(root)`
at `/` alongside the zip/mods/overrides layers, so the shipping config
structurally never has a real on-disk file under root that no provider
knows about — the one condition `RootMap::decide`'s new deny exists to seal.
This run's own zero `denied` count is live confirmation of that prediction,
not new evidence that the deny does anything in a real launch: the property
the deny adds is demonstrated by the targeted tests (see that section), not
by this or any other real-launch run, and no future gate-2-5 re-measurement
should expect a nonzero `denied` count from an ordinary `skyrim-live` launch
either, for the same structural reason.

### Verification

- `cargo test --workspace`: see `task-6-report.md` for the exact count from
  this task's own workspace-wide run; this section adds no new Rust tests
  itself (fixture-derived counter re-measurement is not needed here, since a
  real launch was available).
- No Rust source was changed to produce this section — `vfs-shim-dll` and
  `skyrim-live.exe` were the exact binaries Task 5's own work already built
  and verified fresh; this task neither rebuilt nor modified either.
- DLL freshness re-confirmed immediately before this launch (see the Session
  table above), per the project's own DLL-staleness convention.
- No stray `vfs.exe`/`SkyrimSE.exe`/`skyrim-live.exe` process was running
  before this launch or after it exited (`tasklist` checked both times).

## Stage 2b, Task 6: the two-root session (2026-08-14)

The question this stage exists to answer, from gate 1's deep session: Skyrim's
saves and profile files travel through the NTFS junction
`Documents\My Games\Skyrim Special Edition` → `C:\tmp\skyrim-data\profiles`.
That junction sat outside any managed root, so the shim tagged everything under
it `outside-root` and neither `Routed` nor any fall-through class ever saw it.
Stage 2b makes that location a second managed root.

### Session

`tools\gamectl.ps1 -Action launch`, `skyrim-live` pid 25108, game pid 28180.
Two roots: `C:\tmp\skyrim-runtime` (root 0, zip + overrides) and
`Documents\My Games\Skyrim Special Edition` (root 1, `DiskProvider` over the
junction's resolved target).

**Root 1 is declared with the literal, unresolved junction path.** This is the
opposite of what the task brief instructed, and the brief was wrong. The shim
hooks `NtCreateFile` *before* the kernel resolves the junction, so the spelling
it sees is the literal one — which gate 1's own captured log shows directly
(line 504 of this file records the save open as
`\??\c:\users\...\my games\skyrim special edition\saves\deepsession1.ess`, never
a resolved target). `RootMap`'s OS-consult fallback is `~`-gated and never fires
for an ordinary junction path. Declaring the resolved target would have produced
a root the shim could never match: root 1's counters would have read zero, and
that zero would have been indistinguishable from "saves still bypass". The
provider mounted at root 1 uses the *resolved* target; only the matching path is
literal. The harness prints and cross-checks the resolution before launching, so
a misconfigured root announces itself as a WARNING rather than as a false
negative.

### Results

```
under-root open outcomes:
  routed                               4393
        1858x  ...\my games\skyrim special edition\skyrim.ini
        1708x  ...\my games\skyrim special edition\skyrimcustom.ini
         450x  ...\my games\skyrim special edition\skyrimprefs.ini
           5x  ...\my games\skyrim special edition\saves\
           (remainder under c:\tmp\skyrim-runtime — root 0)
  fell-through: drm-exception            16
          15x  ...\vfs-stage-25108\skyrimse.exe
           1x  ...\skyrim-runtime\steam_appid.txt

vfs-io opens: ok=2375 err=2018 (reconciliation target ok+err=4393) rejected_writes=0
root 0 (implied = combined − root 1): open ok=62  err=310
root 1: getattr ok=301 notfound=0 err=0
        open read ok=2013 err=1708  write ok=300 err=0
        read_ops=0 read_bytes=0  write_ops=0 write_bytes=0
```

**Reconciliation is exact:** 2375 + 2018 = 4393 = the shim's `routed`. The
invariant is `routed == opens_ok + opens_err`, not `opens_ok` alone.

**The headline answer: the My Games root now routes.** There is no
`outside-root` class in this run at all — every path the shim saw under the
junction was routed. The only fall-through is the four DRM filename exceptions,
which gate 5 owns and which are expected to be non-zero until then.

Stated precisely, because the imprecise version is tempting: this run does
**not** show gate 1's specific `outside-root` entries moving to `routed`. Those
entries were `saves\deepsession1.ess` and `saves\deepsession1.ess.tmp`, and
neither occurred here — no save was written (see below). What this run shows is
that the *category* is gone: the profile files and the `saves\` directory
itself, all under the same junction that produced gate 1's `outside-root`
tags, now route. The save file specifically remains unobserved.

**Writes to root 1 route at open time:** 300 successful write-opens, 0 errors,
0 rejections. `rejected_writes=0` is honest rather than a masked instrument,
but note it is a *combined* count across both roots, not root 1's alone. Root
1's provider is unconditionally `ReadWrite`, so root 1 can never contribute a
director-level rejection — a root-1 write failure would appear as `open write
err`, which is 0. The 0 therefore says nothing was rejected anywhere; it does
not isolate root 1.

### What this run did NOT establish

**No `.ess` save was written.** The session never reached gameplay: the
Anniversary Edition "Thanks for buying / DOWNLOAD" modal held the main menu and
could not resolve, almost certainly because Steam is in offline mode. Input was
confirmed reaching the game (the cursor moves) — the dialog simply swallows it.
Per the standing convention in this project, a blocking dialog is a content
problem, not an input problem, so this was not scripted around.

The consequence for gate 4 is specific and worth stating precisely:
`FellThroughWriteFallback` is **0 in this run, and that zero is not evidence of
anything**. It is 0 because the 300 write-opens that did occur all routed
successfully, and because the one write class gate 4 most wants to see — the
save file itself — never happened. Gate 4 must not read this as "no write
fall-through work to do."

`write_ops=0 / write_bytes=0` alongside `open write ok=300` says the INI files
were opened for write at menu time but no ring-level write op followed.

### Follow-up this run identified

The AE upsell modal needs a **content-level** fix in `skyrim-live`'s profile
seeding, not input automation — the profiles directory is fresh each run, so
the game re-shows the one-time prompt every session. Until that lands, this
harness cannot reach gameplay on this machine, and therefore cannot produce a
save.

### Verification

Both release artifacts were confirmed fresh before launching, and both were
stale on first inspection: `target\release\skyrim-live.exe` and
`vfs_shim_dll.dll` dated 17:14/17:15, predating stage 2b's wire change and this
task's harness. `gamectl.ps1 launch` runs the **release** binary, so verifying
the debug build proves nothing. Rebuilt, sizes changed (861696→905216,
1006592→1026048), and markers (`VFS_VIRTUAL_ROOTS`, `root 1 resolves`,
`EmptyRoot`) confirmed present in both.

Had the stale DLL been launched, it would have spoken wire VERSION 1 to a
VERSION 2 director. Stage 2b's version bump (`vfs-ipc/layout.rs`, rejected at
`ring.rs` open) would have failed the attach loudly rather than misparsing a
root-prefixed payload as a path — the guard behaving as designed.

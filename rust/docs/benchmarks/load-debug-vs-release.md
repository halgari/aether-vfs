# Load benchmark — debug vs release

Wall clock from `skyrim-live` start to **the game's window on screen**, measured by
`vfs_director::bench` (`VFS_BENCH=1`). Unlike `vfs-fuse-bench`, which times ring
round-trips in isolation, this covers the whole path a player waits on: zip index,
PE staging, injection, hollow, and content streaming.

**Host:** WIN11-RUST · **Game:** Skyrim AE 1.6.1170 via SKSE 2.2.6 · **Content:**
15 GiB Stored zip, nothing installed on disk · **Window:** 1280x720 windowed

## Run

```powershell
cargo build --release -p vfs-shim-dll
cargo build --release -p vfs-payload
cargo build --release -p vfs-directord --bin skyrim-live

$env:VFS_BENCH=1; $env:VFS_BENCH_LABEL='release'
$env:VFS_SKYRIM_MODS='C:\tmp\skse\overlay'; $env:VFS_SKYRIM_LAUNCH='skse64_loader.exe'
.\target\release\skyrim-live.exe
```

Build the shim DLL as its own command: `--bin` filters the target set, so
`-p vfs-shim-dll ... --bin skyrim-live` silently skips the DLL and the run
measures a stale one.

## Results

| run | zip idx | staged | serving | launched | **window** | MiB | KiB/read | reads/MiB |
|-----|--------:|-------:|--------:|---------:|-----------:|----:|---------:|----------:|
| debug | 0.37 | 0.41 | 0.37 | 0.77 | 12.23 | 69.5 | 164.4 | 6 |
| release | 0.37 | 0.41 | 0.37 | 0.74 | 9.79 | 69.5 | 164.4 | 6 |

Three runs each, time-to-window in seconds:

| build | 1 | 2 | 3 | mean |
|-------|--:|--:|--:|-----:|
| debug | 12.23 | 12.41 | 11.51 | **12.05** |
| release | 9.79 | 10.61 | 10.61 | **10.34** |

**Release is ~14% (1.7 s) faster.** Byte and op counts are identical between
builds, as expected — only CPU cost differs, not the work done.

## Where the time goes — the control run

Everything before the game starts is **0.74 s total** (zip index, staging,
serve, inject, hollow). The obvious reading is that the remaining ~9 s is the
game's own startup. **That reading is wrong**, and the only way to know was to
measure a launch with no VFS at all.

The zip was extracted to disk and the same SKSE launch timed natively — no
shim, no director, same INIs (My Games is junctioned, so the 720p settings
carry), same window detection:

| configuration | time to window |
|---------------|---------------:|
| **Native** (no shim, no VFS) | **1.0 s** (0.92 / 1.03 / 0.92) |
| VFS, zip backend | 10.34 s |
| VFS, disk backend | 10.38 s |

**The VFS adds ~9.3 s — roughly a 10× slowdown**, and it is *not* the game
being slow.

Swapping the content source changes nothing (10.34 vs 10.38 s), so the cost is
**not** in `ZipBackend`: it is in the layer common to both, the shim hooks and
the IPC round trip.

## Why the op counters do not explain it

At the window mark the director has served 433 reads / 69.5 MiB plus ~370
getattr/open — about **800 operations**. Spreading 9.3 s over those would need
**~11 ms per op**, which is implausible for a shared-memory ring measured at
20–209 µs per RPC (`c-throughput-delta.md`).

So the cost is in work the director never sees. The shim detours *every* file
operation the process makes, including the thousands that pass straight through
to disk (System32 DLLs, probes, directory queries) and never become a ring
request. `io_stats` counts only what reaches the director, so that traffic is
invisible here.

**Next measurement:** count and time hook invocations in the shim — total calls,
under-root vs passthrough, and time spent inside the detour. That is the only
number that can attribute the 9.3 s. Optimising the read path (read-ahead,
larger blocks) targets ~800 ops and cannot recover it.

## Where read amplification actually lives

The small-read problem is real but sits **after** this measurement point, and
given the control run above it is **not** where the 9.3 s lives. A full session
to the main menu shows:

| path | ops | MiB | avg |
|------|----:|----:|----:|
| `skyrim - shaders.bsa` | 12,432 | 64.19 | 5.3 KiB/op |
| `skyrim - misc.bsa` | 5,813 | 23.38 | 4.1 KiB/op |
| `skyrim - animations.bsa` | 63 | 62.14 | 1010 KiB/op |

Two archives account for ~97% of all read RPCs. At the window mark only 433
reads have happened, so that traffic is menu/content load, not startup. A
client-side read-ahead cache (the shim has none; the server already has one —
see B5/C2) targets *that* phase and should be measured with an end condition
past the window — but it is a second-order win next to the per-call hook cost.

## Reproducing the native control

```powershell
& 'C:\Program Files\7-Zip\7z.exe' x 'C:\tmp\skyrimse.zip' -o'C:\tmp\skyrim-native' -y
$root = 'C:\tmp\skyrim-native\Skyrim Special Edition'
# Beside the exe: steam_appid.txt (DRM reads it from the cwd), the DX redist
# DLLs, and SKSE - so the native run matches the VFS run file for file.
Start-Process "$root\skse64_loader.exe" -WorkingDirectory $root
```

Then poll for a visible `SkyrimSE.exe` window with a non-zero client rect, the
same end condition `vfs_director::bench` uses.

## Hook-level attribution

`VFS_SHIM_STATS_LOG=<file>` turns on `vfs_shim::hookstats`: per-hook call counts
and time inside the detour, snapshotted every 250 ms from the game process.
Release build, SKSE launch, read at the window mark (9.78 s):

```
  NtCreateFile                       370 calls     1.953s   5279.6 us/call  rooted 168 (45.4%)
  NtOpenFile                        3789 calls     0.105s     27.8 us/call
  NtQueryAttributesFile               41 calls     0.110s   2675.6 us/call
  NtQueryFullAttributesFile          229 calls     2.147s   9375.5 us/call
  NtReadFile                        2482 calls     3.899s   1570.9 us/call
  NtWriteFile                        217 calls     0.004s     19.7 us/call
  NtClose                           5815 calls     0.143s     24.7 us/call
  NtQueryDirectoryFileEx               8 calls     0.018s   2213.5 us/call
  NtQueryInformationFile            2081 calls     0.003s      1.4 us/call
  NtCreateSection                     31 calls     0.001s     31.5 us/call
  NtMapViewOfSection                 105 calls     0.001s      8.3 us/call
  TOTAL                            15168 calls     8.385s    552.8 us/call
```

**8.385 s of a 9.78 s launch is spent inside the detours — ~86% of wall clock**,
which accounts for essentially all of the 9.3 s gap against native.

It is not call volume. Hooks that only pass through cost **1–28 µs**
(`NtQueryInformationFile` 1.4 µs across 2081 calls; `NtClose` 25 µs across
5815). The hooks that issue a ring RPC cost **1.6–9.4 ms** — two orders of
magnitude more than `vfs-fuse-bench` measures for the same RPCs (20–209 µs).

## Root cause: the notifier, not the ring

`vfs-fuse-bench` runs `SpinNotifier`. The director runs `EventNotifier`, whose
waits are (`vfs-win/src/event_notifier.rs`):

```rust
WaitForSingleObject(self.server_ev, 1); // 1ms slice then re-check atomics
```

A 1 ms timeout does not wake in 1 ms. Windows' default timer resolution is
**15.6 ms**, so the wait rounds up to the next tick and any RPC that misses an
immediate signal pays a scheduler quantum. That is precisely the observed
1.6–9.4 ms per call, and the totals reconcile:

```
2482 × 1.57ms + 229 × 9.4ms + 370 × 5.3ms ≈ 8.0 s
```

**The published ring numbers are measured on a notifier the product does not
use.** Fixes, in order of expected effect:

1. **Spin-then-wait.** Spin for a few tens of µs — the real work is 20–209 µs —
   before falling back to the event. This is what makes `SpinNotifier` ~100×
   faster and should remove most of the 8 s.
2. **`timeBeginPeriod(1)`** in the director/shim: a cheap mitigation that turns
   15.6 ms granularity into ~1 ms, but still sleeps.
3. Only then read-path work (read-ahead, larger blocks). At 2482 reads it is
   worth having, but it is second-order next to per-call wake latency.

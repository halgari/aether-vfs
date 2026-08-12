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

## What this says about where the time goes

Everything before the game starts is **0.74 s total** (zip index, staging,
serve, inject, hollow). The remaining **~9 s is the game's own startup** —
D3D device creation, master/BSA header parsing, engine init.

The VFS moves **69.5 MiB in 433 reads (164 KiB/read)** before the window
appears, plus ~370 getattr/open ops. Against the ring's measured throughput
(`c-throughput-delta.md`: ~1937 MiB/s sequential bulk, 20–209 µs per RPC by
size) that is well under a second of the ~10.

**So the VFS is not the bottleneck for time-to-window.** Optimising the read
path will not move this number much.

## Where read amplification actually lives

The small-read problem is real but sits **after** this measurement point. A full
session to the main menu shows:

| path | ops | MiB | avg |
|------|----:|----:|----:|
| `skyrim - shaders.bsa` | 12,432 | 64.19 | 5.3 KiB/op |
| `skyrim - misc.bsa` | 5,813 | 23.38 | 4.1 KiB/op |
| `skyrim - animations.bsa` | 63 | 62.14 | 1010 KiB/op |

Two archives account for ~97% of all read RPCs. At the window mark only 433
reads have happened, so that traffic is menu/content load, not startup. A
client-side read-ahead cache (the shim has none; the server already has one —
see B5/C2) targets *that* phase, and should be measured with an end condition
past the window.

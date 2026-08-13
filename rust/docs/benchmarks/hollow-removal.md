# Removing the hollow: launch cost before and after

**Date:** 2026-08-13
**Build:** release
**Harness:** `skyrim-live` with `VFS_BENCH=1` (stops at the first rendered
frame), three runs per configuration, game killed between runs.

## Question

Once the launch closure is staged to disk, `CreateProcess` has a real image and
the Windows loader maps, relocates and binds it. The hollow then re-read the same
PE from the VFS and wrote a hand-built copy back over that mapping. Was it still
buying anything?

## Numbers

Columns as emitted by `bench::markdown_row`: phase marks, then time to window,
then VFS bytes read and read shape at that point.

| label | zip idx | staged | serving | launched | **window** | MiB | KiB/read | reads/MiB |
|-------|--------:|-------:|--------:|---------:|-----------:|----:|---------:|----------:|
| hollow-1 | 1.66 | 1.70 | 1.66 | 5.06 | **5.07** | 532.5 | 25.6 | 40 |
| hollow-2 | 1.73 | 1.78 | 1.74 | 5.12 | **5.13** | 531.0 | 26.1 | 39 |
| hollow-3 | 1.72 | 1.76 | 1.72 | 5.11 | **5.12** | 533.7 | 25.3 | 40 |
| nohollow-1 | 1.72 | 1.76 | 1.72 | 2.01 | **2.82** | 69.5 | 164.4 | 6 |
| nohollow-2 | 1.71 | 1.75 | 1.71 | 2.00 | **2.81** | 69.5 | 164.4 | 6 |
| nohollow-3 | 1.71 | 1.74 | 1.71 | 1.99 | **2.81** | 69.5 | 164.4 | 6 |

**5.11 s → 2.81 s**, and **532 MiB → 69.5 MiB** read from the VFS by the time a
frame is on screen.

Note where the time sits: `launched` is the mark after `Session::launch`
returns, and on the hollow path it is 5.06 s against 2.01 s. The hollow ran
*inside* `launch`, so the cost is the hollow itself, not the game starting more
slowly afterwards.

## Reading this honestly

The no-hollow profile — 69.5 MiB, 164.4 KiB/read, 6 reads/MiB — is an exact
match for the figures in [`load-debug-vs-release.md`](./load-debug-vs-release.md),
recorded when the hollow path measured 2.74 s. So the right conclusion is **not**
"removing the hollow made the launch 1.8× faster". It is:

- the hollow path **regressed** at some point between 2026-08-12 and 2026-08-13,
  to 5.1 s and 532 MiB;
- removing it **restored** the known-good profile exactly.

The regression was not isolated. The most likely interaction is the relative-name
resolution added on 2026-08-13, which broadened what resolves through the VFS and
would plausibly pull the hollow's own PE and import reads onto the ring. That is
a hypothesis, not a measurement — but since the path it would have affected no
longer exists, it is not worth chasing further.

## Correctness, not just speed

Both configurations were driven to a loaded world before the numbers were taken
seriously:

- direct `SkyrimSE.exe`: masters load, `Data` enumerates 34 entries,
  `coc riverwood` from the main menu loads Riverwood;
- via `skse64_loader.exe`: the hook-stats file is written by the **child** pid,
  `getskseversion` reports `2.2.6, release idx 72, runtime 01064920` in-game, and
  `coc riverwood` loads the world.

The second matters more than the first: it is the process switch-over, and it
works because staging places the game EXE beside the loader and the shim follows
the child across `CreateProcess`. See
[`../architecture.md`](../architecture.md) §4.2 and §4.3.

## Method notes

- `coc` is issued **from the main menu**, not after starting a new game: a new
  game begins with the scripted Helgen sequence and `coc` during it does not
  reliably take.
- Runs are ended with `qqq` at the console so the game shuts down on its own path
  rather than being killed.

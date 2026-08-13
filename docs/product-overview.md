# aether-vfs — Product Overview

**For:** project and product managers, and anyone who needs to understand what
this is without reading code.
**Technical companion:** [`rust/docs/architecture.md`](../rust/docs/architecture.md)

---

## In one sentence

aether-vfs lets a Windows game run against a modded set of files that exists
only in memory — the game plays normally, and nothing on disk is changed.

## The problem it solves

Game modding today works by **copying files over the game install**. A mod
manager takes a mod, copies its files into the game folder, and overwrites what
was there. This is how the whole ecosystem has worked for twenty years, and it
creates four durable problems:

- **The install becomes unknowable.** After a few dozen mods, no one — including
  the tooling — can say with confidence what is actually installed. Diagnosing a
  broken setup means diagnosing a pile of overwrites.
- **Everything is slow and duplicated.** Installing a mod means copying
  gigabytes. Trying a different combination means copying them again. A user
  with several setups stores several full copies.
- **Changes are hard to undo.** "Remove this mod" is not a clean operation once
  files have been overwritten.
- **Verification breaks.** The game's own files no longer match what the store
  shipped.

The alternative the industry uses for this class of problem is a **kernel
driver**. That works, but it requires code signing, it is invasive to install,
and a defect can take down the whole machine rather than one application.

## What aether-vfs does instead

It presents the game with a **virtual filesystem**: the base game plus the
selected mods, combined into a single view, assembled on the fly. The game reads
what it expects to find. Every other program on the machine — including the game
store's own verification — sees the original, untouched installation.

Concretely:

- The base game can stay in a **single compressed archive**. It is never
  unpacked.
- Mods are **layered on top** as data, not copied over anything.
- Switching mod sets is **changing a list**, not moving files.
- Nothing is written to the game folder, so **nothing needs undoing**.
- A failure affects **one process**, not the operating system.

## Current status

**Working, and demonstrated end to end.** Skyrim Special Edition — a large,
commercially released game with anti-tamper protection — boots, loads its world,
and plays from a 15 GB archive with no extraction. Official add-on content loads
alongside it. The system has been driven through a full session: main menu,
starting a game, and warping into the world.

This is the meaningful milestone: not "files can be served", but "a real,
unmodified, protected commercial game cannot tell the difference."

## Performance

Startup is the honest measure, since that is when a game touches the most files.

| Setup | Time to game window |
|---|---|
| Normal installation, no aether-vfs | ~1.0 s |
| aether-vfs, earlier build | ~10.3 s |
| **aether-vfs, current** | **~2.7 s** |

The remaining difference is roughly one and a half seconds, and about half of
that is one-time setup work a normal launch never has to do. Ongoing play is not
affected in the way startup is.

The large early gap turned out not to be the cost of reading from an archive —
it was time spent waiting rather than working, and fixing that accounted for
almost the whole improvement. Reading from a compressed archive costs
essentially the same as reading from an ordinary folder.

## What it is good for

- **Mod managers.** A user's setup becomes a list that can be shared, versioned,
  and reproduced exactly, rather than a folder no one can audit.
- **Disk economy.** Many different configurations over one copy of the game
  instead of one full copy each.
- **Instant switching.** Changing configuration is not a copy operation.
- **Safety.** The installation is never modified, so "restore my game" is not a
  procedure.
- **Support and diagnosis.** What a session was actually running is recorded
  data rather than an archaeological question.

## What it is not

- **Not a cheat tool.** It is a single-player modding tool. The techniques it
  uses to load a game are the same ones anti-cheat systems exist to detect, so
  it is unsuitable for competitive multiplayer, deliberately.
- **Not cross-platform yet.** Windows 64-bit only.
- **Not a general-purpose filesystem.** It is built for the read-heavy,
  many-small-files behaviour of games. Full support for programs that write
  extensively into the virtual view is still in progress.
- **Not a compression system.** Archives must be stored uncompressed inside the
  container, which is what makes reading any part of a 15 GB file fast. The
  saving comes from not duplicating installs, not from shrinking them.

## Risks and constraints

| Risk | Standing |
|---|---|
| Deep OS integration is inherently fragile across Windows updates | Real. Mitigated by broad automated tests that run the actual interception logic in-process; a Windows 11 behaviour change was caught this way. |
| Techniques resemble those used by malware and cheats | Accepted and scoped: single-player modding only. May affect antivirus interaction. |
| Games can behave in undocumented ways | The main source of past defects. Addressed by instrumentation that makes "the game silently gave up" visible, which is the characteristic failure. |
| Write-heavy workloads | Known gap; read paths and overlay reads are complete, full copy-on-write is not. |

## The engineering story worth knowing

Most defects in this system do **not** produce an error. They cause a step to be
quietly skipped, and the game carries on and behaves oddly much later.

The clearest example: the game reached its main menu looking completely healthy,
but could not start a game and appeared to hang forever. It was not hung — it
had silently failed to find any of its content files, so there was simply no
world to load. Nothing errored. Every performance counter looked idle, which
read as "stuck" and was actually "finished asking".

The cause was that Windows lets a program name a file in more than one way, and
one of those ways was not being understood. The tell came from a detail a person
noticed rather than a metric: the normal game plays a logo video at startup, and
this one skipped it. A missing video is skipped without complaint — which made
it a much simpler instance of the same failure, and led straight to the cause.

Two things came out of that, and both are now permanent:

1. **Instrumentation that can show an absence**, not just a count. When a metric
   shows nothing, the first suspect is the measurement.
2. **Tests covering every way the operating system can express the same
   request**, not just the common one. That gap is what let the defect ship, and
   closing it immediately surfaced three more instances of the same mistake
   elsewhere in the code.

## Where it goes next

- Completing the write path so tools that modify files in the virtual view are
  fully supported.
- Closing the remaining startup gap against a normal launch.
- Broadening coverage to further games beyond the current proof point.

---

*Last updated 2026-08-12.*

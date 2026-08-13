# aether-vfs — Architectural Overview

**Audience:** engineers joining or reviewing the system.
**Scope:** how the whole thing fits together, and how the genuinely hard parts
are solved. Implementation detail lives in module docs; this is the map.

Companion documents: [`../../docs/product-overview.md`](../../docs/product-overview.md)
(non-technical), [`vfs-summary.md`](./vfs-summary.md) (earlier long-form
narrative), [`benchmarks/`](./benchmarks/) (measurements).

---

## 1. What the system does

aether-vfs makes a Windows game see a filesystem that does not exist on disk.

A game is installed as a 15 GB zip archive plus a set of mod folders. Instead of
extracting and merging those onto disk, aether-vfs composes them into a single
virtual tree and serves that tree to one process, live, by intercepting the
Windows NT file API inside it. Every other process on the machine sees the
original, untouched directory.

The proof point is Skyrim Special Edition: it boots, loads its world, and plays
from a Stored zip with **no durable extract** of game content.

### Why this is worth doing

The established approach to game modding is to copy mod files over the game
install, or to use a kernel filter driver. Copying is destructive, slow, and
makes "what am I actually running?" unanswerable. A driver is invasive, needs
signing, and a bug takes the machine down rather than the game.

A userspace VFS keeps the install pristine, makes a mod list a piece of data
rather than a mutation, and confines failure to one process.

---

## 2. Topology

```text
┌─ Host process (CLI, daemon, or embedding app) ────────────────────────────┐
│                                                                           │
│  Session          mounts, paths, launch                                   │
│  Director         userspace FUSE kernel: resolve, overlay, handle table   │
│  Backends         zip · disk · cache · compose · gRPC plugin              │
│                                                                           │
└───────────────────────────────┬───────────────────────────────────────────┘
                                │  shared memory:
                                │  control ring (slots) + bulk arena (banks)
┌───────────────────────────────▼───────────────────────────────────────────┐
│ Game process                                                              │
│                                                                           │
│  ntdll detours   NtCreateFile / NtOpenFile / NtRead / stat×3 / enum×2 / …  │
│         │                                                                 │
│         ├─ path under the managed root ──► FuseClient ──► ring ──► director│
│         └─ anything else ────────────────► real ntdll, untouched          │
│                                                                           │
│  synthetic handles · demand-paged sections · staged PE closure            │
└───────────────────────────────────────────────────────────────────────────┘
```

**Direction of authority.** The director owns content; the shim owns
interception and owns nothing else. The shim never opens a layer archive. That
single rule is what keeps the trust boundary describable: archive bytes enter
the game only through the ring.

---

## 3. The layers

### 3.1 Content model — `vfs-core`, `vfs-shared`

Pure, OS-free, no I/O. Given enumerated layers, it produces a merged tree and
answers `resolve(vpath)`. It knows about:

- **Layer precedence** — later layers win.
- **Tombstones** — a first-class entry kind meaning "hide what is beneath".
  Deleting in an overlay must not reveal the file it was covering.
- **Case folding** — Windows is case-insensitive; the virtual tree must be too,
  without being case-*destructive* (the original spelling is preserved for
  enumeration).
- **Wildcards** — enumeration filters (`*.esm`) are matched here, not in the hook.

`vfs-shared` is the bitness-neutral shared-memory layout for publishing a
snapshot of that tree, with a seqlock so a reader never observes a torn update.

Keeping this layer pure is what makes the merge semantics testable without a
game, a driver, or even a filesystem.

### 3.2 Sources and composition — `vfs-source`, `vfs-zip`, `vfs-compose`, `vfs-cache`

Everything that can supply bytes implements one `Backend` trait
(`vfs-protocol`): `getattr`, `readdir`, `open`, `read`, `write`, `rename`,
`delete`, `mkdir`, `close`.

- **`vfs-zip`** parses the ZIP64 central directory and serves **Stored**
  (uncompressed) entries as byte windows into the container. Stored-only is a
  deliberate constraint: a Stored entry is a contiguous range, so a read at an
  arbitrary offset is a seek, not a decompress-from-the-start. That is what
  makes random access into a 15 GB archive viable.
- **`vfs-compose`** stacks backends bottom-to-top with overlay semantics.
- **`vfs-cache`** is a block cache (RAM LRU, optional disk tier) keyed by
  `(source, file, block)`.
- **`vfs-source`** turns a declarative spec into a live backend, including
  out-of-process gRPC plugins — so a source can be written in any language.

### 3.3 The director — `vfs-director`

The userspace FUSE kernel. Holds the mount table, resolves a virtual path
through it, owns the global file-handle table, and serves ring requests. It is
the only component that touches archive containers.

`Session` is the host-facing API: configure mounts and paths, `serve()` to
stand up the ring, `launch()` to start the target.

### 3.4 IPC — `vfs-ipc`, `vfs-win`

A shared memory segment holding a **control ring** of fixed slots plus a **bulk
arena** of per-slot banks. Small requests and replies travel inline in the slot;
large reads land in the arena so the ring never has to carry megabytes.

Slot ownership moves by `compare_exchange`, which makes it multi-producer safe
without a lock — necessary because a game issues file I/O from many threads at
once. `vfs-ipc` imports no OS API at all; the mapping and the event objects live
in `vfs-win`. All `unsafe` is confined to the segment accessor.

### 3.5 The shim — `vfs-shim`, `vfs-redirect`

Detours on ntdll, installed inside the game. For each intercepted call it
decides: is this path ours? If yes, serve it (from the director, or from a
synthetic handle); if no, call the original function so the rest of the system
is untouched.

`vfs-redirect` holds the pure decision logic — path in, decision out — so the
policy is unit-testable away from the hooks.

### 3.6 Process creation — `vfs-payload`, `vfs-inject`, `vfs-director::stage`

Getting the shim into the process before the process needs the VFS. This is the
subtlest part of the system and gets its own section below.

### 3.7 Control plane — `vfs-control`, `vfs-directord`, `vfs-launch`

A gRPC contract plus a declarative config schema, a daemon that can hold many
sessions, and CLIs. The control plane is language-agnostic; the data plane is
the ring.

---

## 4. The hard parts, and how they are solved

This is the section worth reading. Each of these took real effort to get right,
and several were only understood after a failure that produced **no error at
all**.

### 4.1 Bootstrapping: the process must be hooked before it can run

You cannot hook a process that does not exist, and by the time it exists the
Windows loader has already resolved the executable's static imports. A game EXE
sitting alone in an otherwise-virtual directory dies at `STATUS_DLL_NOT_FOUND`
before a single line of our code runs.

Three mechanisms, in order:

1. **Staging the PE closure.** Before launch, the director writes the target EXE
   and its non-system static imports — transitively, and nothing else — into a
   scratch directory. For Skyrim that is a 37 MB EXE and three DLLs against
   ~15 GB that stays virtual. The directory is deleted when the process exits.
2. **Dual-layer injection.** A `no_std`, zero-import payload is reflectively
   mapped and runs *before* `LdrpInitializeProcess`, where only ntdll exists. It
   hooks the four path/attribute stubs needed to survive early init. Once the
   loader has finished, the full shim installs and takes over with the complete
   hook set and a live ring client.
3. **Spin-gate handoff** between the two so neither races the other.

The payload imports nothing because it cannot: at that point in process life
kernel32 and the CRT are not mapped. Every address it needs is passed in a
config struct by the injector.

### 4.2 `CreateProcess` needs a real file on disk

Windows will not create a process from a buffer, and for a long time the answer
here was **process hollowing**: create the process from some unrelated on-disk
host image, then overwrite that image in memory with the PE we actually wanted.
It preserved a strict "no game PE ever touches disk" invariant.

Making a hollowed MSVC CRT executable actually *run* cost a great deal —
security cookie initialisation, remote TLS plus the TEB slot,
`RtlAddFunctionTable` for x64 unwind data, LDR `SizeOfImage`/`EntryPoint`
fixups, an entry trampoline so exception registration ran on the primary
thread — and every one of those was a hand-written re-implementation of
something the Windows loader already does correctly.

**That invariant no longer holds, so the mechanism is gone.** Staging writes the
real EXE and its import closure to a scratch directory, which means there *is* a
real file to `CreateProcess`, and the loader maps, relocates and binds it
properly. The launch path is now simply: stage → `CreateProcess` suspended →
inject → resume.

The measurements that settled it (three runs each, 2026-08-13) are worth keeping,
because the redundancy was not free:

| | time to window | VFS bytes read |
|---|---:|---:|
| with hollow | 5.07 / 5.13 / 5.12 s | 532 MiB |
| without | **2.82 / 2.81 / 2.81 s** | **69.5 MiB** |

The hollow had become a no-op that still did all the work: `VFS_HOLLOW_HOST`
pointed at the staged EXE, so the code re-read that PE from the VFS, re-applied
the same relocations, and wrote it back over the loader's own correct mapping at
the same base. `host_is_target` was true, which already caused the TLS setup to
be skipped — an explicit admission that the loader had done it right.

Removing it deleted ~3,800 lines (`ghostly.rs`), the purpose-built neutral host
crate, three diagnostic binaries, a `hollow_pe` flag threaded through the gRPC
contract and every launch API, and a hardcoded `contains("skyrimse")` special
case. One consequence had to be carried across deliberately rather than deleted:
the hollow path also grew the primary thread's stack to 16 MiB, because the
shim's extra frames overflow the stock 1 MiB stack (`0xC00000FD`). That is not
hollow-specific and now happens on the single launch path.

**The one capability genuinely lost** is launching a child EXE that exists *only*
inside an archive, with no path to `CreateProcess` from. Staging the whole launch
closure — including a child that a loader will spawn — covers the real cases (see
§4.3), and keeping a second launch mechanism alive for a hypothetical one was
judged not worth its weight.

### 4.3 Following the game across process creation

A mod loader does not *become* the game; it launches it. `skse64_loader.exe`
starts, does its work, spawns `SkyrimSE.exe`, and exits. The virtualised view has
to survive that handover, or the process that actually plays the game sees the
real, nearly-empty directory. This is a known hard part of the problem for any
VFS in this space.

Two halves solve it:

**Stage the whole launch closure, not just the entry point.** When the launch
executable is a loader, staging also places the game EXE beside it, because the
loader will `CreateProcess` it and that needs a real image for exactly the same
reason the top-level launch did. It also stages `skse64_<runtime>.dll`
explicitly: SKSE injects that at runtime rather than importing it, so a PE import
walk cannot discover it. For an SKSE launch the staged set is six files.

**Follow the shim across `CreateProcess`.** The shim detours
`kernelbase!CreateProcessInternalW` — the single funnel beneath every
`CreateProcess*` variant — forces the child to start suspended, dual-layer
injects it, waits for its hooks to report ready, then resumes. A failed inject or
a timeout still resumes the child, so the failure mode is an unvirtualised game
rather than a hung one. The child's image identity is scoped so the parent's does
not leak into it.

Verified 2026-08-13: launched via SKSE, the hook-stats file is written by the
*child* pid, `getskseversion` reports `2.2.6` in-game, and `coc riverwood` loads
the world — with no hollow anywhere in the path.

### 4.4 Multi-gigabyte memory-mapped archives

Bethesda archives are opened with `CreateFileMapping` and read through slid
views. A naive implementation would have to materialise a whole BSA to back the
section — several GB, per archive.

Instead, `NtCreateSection` **reserves** address space without committing it, and
a vectored exception handler commits and streams 256 KiB chunks from the
director on first touch. Large archives are demand-paged; small ones take an
eager path.

The lifetime rule matters as much as the paging. The reservation belongs to the
*section*, never to a view, because that is NT's model and the game depends on
it: a BSA reader slides views across one archive, so unmapping one window must
leave every other window — and any later remap of the still-open section —
valid. The VA is released only once the section handle is closed **and** the last
view is gone. Getting this wrong produced crashes far away from the cause.

### 4.5 A file has more than one name

This is the defect class that cost the most, and it is worth stating plainly:
**NT lets a caller name a file as (directory handle + relative name)**, not only
as an absolute path. `CreateFileW("Data\\X")` reaches ntdll as the process's
current-directory *handle* plus the relative string.

A hook that only understands absolute names does not fail on these. It decodes
nothing, declines to act, and the call proceeds to whatever is really on disk
behind the mount. No error, no log line, no counter — the file simply appears
not to exist. Skyrim reached its main menu with an empty load order for exactly
this reason: every plugin lookup took the relative form.

The resolution is one shared `parent_dir_of_handle` consulted by every hook that
decodes a name, covering three kinds of parent:

1. our own synthetic directory handles,
2. real directory handles the process opened (we record a path for every
   successful open),
3. **the current-directory handle**, which the OS creates and publishes only in
   `RTL_USER_PROCESS_PARAMETERS.CurrentDirectory` — there is no API that returns
   it, so it is read from the PEB.

The same defect appeared independently in three hooks. It is now structurally
impossible for one to know about a parent the others cannot.

### 4.6 The same question has several APIs

Windows offers multiple ways to ask "does this file exist, and how big is it",
and callers choose between them for reasons of their own — the same program uses
different ones in different code paths.

- **Enumeration**: `NtQueryDirectoryFile` *and* `NtQueryDirectoryFileEx`.
- **Stat**: `NtQueryAttributesFile`, `NtQueryFullAttributesFile`, and
  `NtQueryInformationByName` — which Windows 11 prefers for existence checks.

Any hook that answers differently from its siblings produces a program that
believes a file both exists and does not. Both failure directions are real: a
false negative makes content silently invisible (this is what suppressed
Skyrim's intro video), and a false positive leaks a file the snapshot
deliberately hides.

Every one of these entry points is hooked, they share one implementation body
where the shapes allow, and cross-API agreement is a test rather than a
convention (§6).

### 4.7 Performance was never where it looked

Early measurement showed the VFS adding ~9.3 s to a game launch, roughly a 10×
slowdown. The natural assumption — too many reads, or reads that are too small —
was wrong. Swapping the content source (zip vs plain disk) changed nothing, and
only ~800 director operations were involved.

The cost was **wake latency**, not work. Per-hook instrumentation with max and
`>1 ms` stall counts made this visible: `NtQueryFullAttributesFile` averaged
819 µs/call, but 215 of 231 calls were fast and sixteen took up to 15.2 ms —
the Windows timer quantum. The average described no call that ever happened.

Two fixes, both about who is awake:

- **Server-side spin-then-wait**: the director spins while the ring is hot
  instead of sleeping between bursts. 10.34 s → 2.74 s.
- **The client wakes the server**: the shim signals an event on submit and then
  spins for the reply, which arrives in 20–209 µs — far less than the cost of
  sleeping for it. Time inside hooks fell 0.536 s → 0.180 s, and every
  quantum-scale stall outside `NtReadFile` disappeared.

The lesson encoded in the tooling: report max and stall counts, never a bare
mean, because the two shapes of "slow" want opposite fixes.

### 4.8 Merged directory listings

An enumeration must show the union of real and virtual entries, hide
tombstones, apply the caller's wildcard, honour case-insensitivity, and remain
stable across the restart-scan and single-entry-at-a-time protocols NT allows.
It is also stateful: a directory handle carries a cursor, so the merged listing
is built once per scan and served in slices.

### 4.9 Isolation

The managed root is **sealed**: under-root paths resolve through the director
only, never falling through to whatever happens to be on disk there. This is
what makes "the game cannot read anything we did not give it" a property rather
than a hope, and it is why the runtime directory is nearly empty at rest.

### 4.10 Handle identity

Once a file is served from a synthetic handle, everything asked *about* that
handle must answer consistently — name, size, position, volume information —
or a caller that stats its own open file gets a contradiction. Handles carry
their virtual identity so queries return the virtual answer, not the backing
file's.

---

## 5. Performance

Time-to-window for Skyrim SE, three clean runs each
([`benchmarks/load-debug-vs-release.md`](./benchmarks/load-debug-vs-release.md),
[`benchmarks/hollow-removal.md`](./benchmarks/hollow-removal.md)):

| configuration | mean | measured |
|---|---:|---|
| native, no VFS | 1.0 s | 2026-08-12 |
| VFS, before wake fixes | 10.34 s | 2026-08-12 |
| VFS, after wake fixes (hollow still in) | 2.74 s | 2026-08-12 |
| **VFS, staged launch (current)** | **2.81 s** | 2026-08-13 |

The current figure is roughly 1.8 s over native. About 0.72 s of that is work
native never does at all — staging, injection — and the rest is hook time.

Two cautions about reading this table. The rows are not a clean progression:
each was measured on the build of its day, and the 2026-08-12 rows predate the
relative-name fixes (§4.5), which broadened what resolves through the VFS. On
2026-08-13 the *same* benchmark put the hollow path at 5.1 s, so that path had
regressed since its 2.74 s was recorded; removing it restored the profile
(69.5 MiB read at the window, 164 KiB/read — an exact match for the historical
table) rather than beating it.

Content source is not a factor: zip and disk backends measure the same
(10.34 vs 10.38 s pre-fix), which is what identified wake latency as the cost.

---

## 6. Testing strategy

The system's characteristic failure is **silence** — work that is skipped rather
than work that errors. Tests are shaped around that.

- **Pure layers are unit-tested** (`vfs-core`, `vfs-redirect`, `vfs-shared`,
  `vfs-ipc`): merge order, tombstones, case folding, wildcards, ring state
  transitions.
- **Hook behaviour is tested in-process.** Integration binaries install the real
  detours into the test process and then use ordinary `std::fs` and raw NT calls
  against a live engine. One install per process, so one test binary per
  scenario.
- **Every naming form is covered, not just the convenient one.** The
  relative-name battery exercises each decoding hook through a real directory
  handle, because Win32 decides on its own whether a relative path becomes an
  absolute name or a handle pair — going through `std::fs` alone cannot
  guarantee the second form was hit.
- **Cross-API agreement is asserted directly**: both enumeration entry points
  must return one view; every stat API must agree about existence and size, in
  both directions.
- **Byte-exactness** is checked against ground truth, including against a native
  extract of the same archive when the corpus is present.

Two habits worth keeping:

*Test the wiring, not only the unit.* The staging-alias predicate was unit-tested
and correct; what drifted was which callers consulted it. A test of a predicate
cannot catch a caller that does not call it — only a single shared implementation
can.

*Verify a regression test fails.* The enumeration-parity test was confirmed to
fail with the classic detour disabled, which is the exact defect that once
shipped.

---

## 7. Diagnostics

Because failures are silent, the shim carries instrumentation that can be turned
on with `VFS_SHIM_STATS_LOG` and answers questions counters normally cannot:

- per-hook calls, total and **max** time, and a `>1 ms` stall count;
- **open paths by frequency** — a retry loop reopens one path thousands of
  times, and a deduplicated list hides exactly the path that matters;
- **every directory enumeration** with its filter, entry count, and whether we
  served it or the OS did — "listed `Data`, got nothing" and "never listed
  `Data`" are different bugs with identical symptoms;
- **every attribute query with its outcome**, since a stat that wrongly says no
  never becomes an open;
- an **ordered trace** of under-root operations, because counts cannot show
  where a sequence stopped;
- **undecodable opens**, which is the only way an unnameable path becomes
  visible at all.

That last one is the general lesson: when a counter shows *nothing*, suspect the
observer before concluding the process is idle.

---

## 8. Crate map

| crate | role |
|---|---|
| `vfs-core` | pure merged-tree resolver: layers, tombstones, case folding, wildcards |
| `vfs-shared` | bitness-neutral shared snapshot layout + seqlock |
| `vfs-protocol` | wire codecs, opcodes, the `Backend` trait |
| `vfs-ipc` | control ring + bulk arena, OS-free |
| `vfs-win` | Windows shared memory and events |
| `vfs-zip` | ZIP64 central directory, Stored windows |
| `vfs-compose` | layered/overlay backends |
| `vfs-cache` | block cache, RAM LRU + optional disk tier |
| `vfs-source` | declarative spec → backend, incl. gRPC plugins |
| `vfs-director` | FUSE kernel, session, staging, launch |
| `vfs-directord` | daemon + CLI; `skyrim-live` harness |
| `vfs-control` | gRPC contract + config schema |
| `vfs-redirect` | pure redirect-decision core |
| `vfs-shim` / `vfs-shim-dll` | NT detours, FUSE client, synthetic handles, sections |
| `vfs-payload` | `no_std` pre-init hook payload |
| `vfs-inject` | injection, PE parsing, process creation |
| `vfs-launch` | end-user launcher |
| `vfs-fixture-*`, `vfs-ring-harness` | test fixtures |

Dependency direction is enforced by the split: pure crates never learn about the
OS, and the zip backend never learns about the host.

---

## 9. Known limitations

- **Windows x64 only.** The design is portable in principle; the implementation
  is not.
- **Stored zip entries only.** Deflate would defeat random access. Archives are
  expected to be repacked Stored.
- **Copy-on-write is partial.** Read-side whiteouts and upper-wins are
  implemented; full create/write-through is outstanding.
- **Anti-cheat.** The techniques here are indistinguishable from those an
  anti-cheat system exists to detect. This is a single-player modding tool.
- **Per-child staging recursion** for sub-processes is designed but not
  implemented.

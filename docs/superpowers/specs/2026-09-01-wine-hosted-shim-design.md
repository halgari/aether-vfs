# Linux delivery via the Wine-hosted shim — design

**Status:** approved 2026-09-01. Supersedes the `vfs-fuse` half of
`2026-08-31-linux-fuse-proton-portability-design.md` (increments 2–4 of that
document). Increment 1 of that spec — making the Director OS-agnostic — landed
and stands.

## 1. The decision

**Linux does not get a FUSE filesystem. It runs the existing Windows shim under
Proton's Wine.**

The earlier spec assumed the Windows shim was inherently Windows-only, so Linux
needed a second delivery mechanism (`vfs-fuse`) translating FUSE ops onto the
same `Director`. That assumption was never tested. It is now false.

Measured 2026-09-01 against **GE-Proton11-6 (Wine 11.0 Staging)** in an Arch WSL
box, running already-compiled Windows binaries under `wine` with no
recompilation:

| Mechanism | Result | How it was measured |
|---|---|---|
| `retour` inline detours on Wine's **PE** ntdll | works | `hook_identity` installed all hooks |
| `NtCreateFile`/`NtOpenFile` redirection | works | `mod.esp`, absent from disk, opened |
| `NtReadFile` redirection | works | `assert_eq!(content, b"the-real-bytes")` passed |
| `CreateProcessW` child spawn | works | child ran to completion |
| Remote DLL injection | works | `loaddll` trace, below |
| Ring transport across the Wine/Linux boundary | works | bidirectional coherence, below |
| Handle → virtual path identity | **fails** | see §6 |

Injection, from `WINEDEBUG=+loaddll`:

```
00e8:loaddll:build_module Loaded L"C:\probe\hollow_hello.exe" at 6FFFFF020000
00ec:loaddll:build_module Loaded L"C:\probe\vfs_shim_dll.dll" at 6FFFFED80000
```

The **differing thread ids are the proof**: `00ec` is the injected remote
thread, so `CreateProcessW` → suspended child → write DLL → remote `LoadLibrary`
all work, `CreateRemoteThread` included. An `inject error: Timeout` printed
alongside is *not* an injection failure — the injector was waiting on the shim's
ready marker, which cannot appear without a valid config file, and the probe
deliberately passed a nonexistent one.

Transport coherence, from purpose-built two-sided probes:

```
LINUX: SAW WINE MAGIC = 'WINE-WROTE-THIS'   LINUX: counter read = 0x1000
LINUX: wrote reply + bumped counter
WIN:   SAW LINUX REPLY = "LINUX-REPLIED-TO-YOU"
WIN:   counter now = 0x3222                  <- 0x1000 + 0x2222, Linux's arithmetic
```

A Wine process and a native Linux process shared memory through a **file-backed**
mapping, each seeing the other's writes. The counter arithmetic rules out a
stale-cache coincidence.

### What it cost

One line. `install_all_detours` resolves 20 ntdll functions; Wine's ntdll exports
**18** of them. The two absentees are `NtQueryDirectoryFileEx` (Win8) and
`NtQueryInformationByName` (Win10 RS2). The second was *already* optional; the
first was a hard `?` and was the entire cause of `install: ProcMissing`.

Skipping an absent export is sound rather than a fudge. `make_detour` fails
because `GetProcAddress` found nothing, and a symbol absent from ntdll's export
table is equally unreachable for the game: it cannot be resolved dynamically, and
a static import against it would fail module load. On such a host the only
reachable enumeration entry point is `NtQueryDirectoryFile`, hooked
unconditionally.

This is **not** licence to let enumeration go unhooked where the export exists.
Per the `NtLockFile` history, an unhooked handle-taking NT API does not error — it
quietly serves the real, near-empty directory, which reads exactly like a mod
list that is simply empty. `hook::skipped_detours()` therefore reports what was
passed over, so a host that requires total interception asserts it instead of
discovering the hole from a silently empty load order.

## 2. Why not FUSE

FUSE was proven *feasible* before this decision — unprivileged mounting works
(`fusermount3` is setuid; mounted as a non-root user), the WSL2 kernel exposes
`/dev/fuse`, mounts propagate into a bubblewrap container, and the `Director`'s
API maps almost 1:1 onto FUSE ops with platform-neutral `ST_*` errors. It is a
workable design. It is simply the worse one, for three reasons:

1. **Semantic divergence.** Windows behaviour would emerge from the shim and
   Linux behaviour from separate FUSE code. Every case-folding rule, every
   whiteout, every rename edge case would exist twice and drift. The case-fold
   contract increment took an entire increment to get right *once*.
2. **mmap cost.** Games memory-map large archives. FUSE adds per-page
   round-trips to a userspace daemon that an in-process hook never pays.
3. **Volume.** `vfs-fuse` is a new filesystem — inode table, lifetime rules,
   handle caching, readdir cookies. The Wine path needs a mapping constructor and
   a launcher.

FUSE stays documented as the fallback if Wine hosting hits something
insurmountable. Nothing in this design forecloses it: the `Director` remains
OS-agnostic, which is what increment 1 bought and what both designs consume.

## 3. Architecture

The shim, ring protocol, and Director are **unchanged and shared**. Only the
transport's OS handles and the launch path are new.

```
  Linux host process (embeds vfs-embed)
    +-- Director  (native Linux, unchanged)
    |     ^
    |     |  ring over a FILE-BACKED shared mapping  (new: both ends)
    |     v
    +-- GE-Proton wine
          +-- game.exe  (Windows PE)
                +-- vfs_shim_dll.dll  (injected, unchanged)
                      +-- retour detours on Wine's PE ntdll
```

Today's transport cannot bridge that boundary:

- `vfs-director/src/ipc.rs` calls `SharedMapping::create(&section_name, ..)`,
  which passes `INVALID_HANDLE_VALUE` to `CreateFileMappingW` — a **page-file
  backed named section**, with no identity a Linux process can open.
- It pairs that with `EventNotifier`, a pair of **named Windows events**, equally
  unreachable from Linux.
- `vfs-shim/src/fuse_client.rs` calls `SharedMapping::open(section, ..)`, by name.
  (That file is named for the FUSE-style RPC design, not for `/dev/fuse`. It
  mounts nothing. The name is vestigial and out of scope to change here.)

Both problems have the same answer, and both halves already exist:

- **Memory:** a file-backed mapping. Wine's `CreateFileMappingW(hFile, ..)` maps
  the real underlying Linux file, coherently with a native `mmap` — measured in
  §1. The shim inside Wine keeps using Win32; the Director `mmap`s the same path.
- **Wakeups:** `SpinNotifier`, which already exists in `vfs-ipc` and needs no OS
  object at all. `vfs-ipc` contains **zero** `cfg(windows)` and already compiles
  and tests on Linux, so the ring's logic needs no work.

`SpinNotifier`'s cost is real and is accepted deliberately for this increment: it
burns CPU where `EventNotifier` blocks. A shared-memory futex would be the proper
answer and is explicitly deferred — the Wine side cannot call `futex` from
Windows code without breaking the one-codebase property, so it needs its own
design. Spin-with-backoff is the honest v1.

## 4. Crate layout

- **`vfs-win`** gains file-backed constructors alongside the named-section ones.
  Still `cfg(windows)`; still only compiled for the shim's Windows target.
- **`vfs-unix`** — new, `cfg(unix)`, holding the Director side's file-backed
  `SharedSeg` producer. It mirrors `vfs-win` deliberately: one crate per OS,
  each owning that OS's handles, with the portable ring above both.

  **`vfs-ipc` must not gain this.** It has exactly one dependency
  (`vfs-protocol`), no external crates at all, zero `cfg(windows)`, and is
  consumed by the Linux CI job as the portable protocol core. Rust's std has no
  `mmap`, so the Linux side needs `libc` — and **`libc` appears nowhere in this
  workspace today**, which makes it a genuine addition rather than a formality.
  Confining it to a new `cfg(unix)` crate keeps it out of every existing
  crate's graph and off the Windows build entirely. Prefer `libc::mmap` behind a
  small RAII type over `memmap2`: one dependency instead of a tree, and it
  matches how `vfs-win` wraps its own primitives.
- **`vfs-proton`** — new, `cfg(target_os = "linux")`. Runtime acquisition, prefix
  construction, launch. §5.
- **`vfs-embed`** dispatches `launch()` by target: inject-and-ring on Windows,
  Proton-and-ring on Linux. Its Windows-only dependencies (`vfs-inject`,
  `vfs-shim`) move under `[target.'cfg(windows)'.dependencies]`, which is what
  currently makes `vfs-embed` fail to even *check* for Linux: `vfs-shim` →
  `retour` → `libudis86-sys`, whose build script needs a C cross-compiler.
  Gating in `Cargo.toml` is required — source-level `cfg` cannot help, because
  the build script runs first.

## 5. The Proton launch path (`vfs-proton`)

### Runtime acquisition

GE-Proton11-6 ships `GE-Proton11-6-x86_64.tar.gz` (533,700,853 bytes) beside a
published `GE-Proton11-6-x86_64.sha512sum`. Download, **verify the digest, and
refuse to proceed on mismatch**; extract once.

Storage is entirely aether-owned. Nothing is written to system or Steam
locations:

```
$XDG_DATA_HOME/aether-vfs/          (override via an explicit API argument)
  runtimes/GE-Proton11-6-x86_64/    extracted once, shared, read-only (~1.5 GB)
  sessions/<id>/prefix/             per-session Wine prefix (~627 MB), disposable
  sessions/<id>/ring.bin            the file-backed ring segment
```

### GE, never stock — enforced, not merely configured

`PROTONPATH` **defaults to UMU-Proton, which is Valve's stock Proton**. An unset
or mistyped value therefore silently downgrades to exactly what is unacceptable.
Two defences, both required:

1. Set `PROTONPATH` to our absolute extracted path — the only form that is both
   GE and download-free.
2. **Verify after resolution** by reading the runtime's `version` file, which for
   this release contains `1787951532 GE-Proton11-6`. Assert it names
   `GE-Proton`. A resolved runtime that is not GE is a **hard error**, not a
   warning. Silent fallback is the failure mode most likely to waste a day.

### umu is deferred, and why

The probes ran `GE-Proton11-6-x86_64/files/bin/wine` **directly**, with no umu
and no pressure-vessel container, and everything in §1 worked. umu's value is the
Steam runtime container supplying host-independent libraries — worth having for
distro breadth, and it ships a self-contained
`umu-launcher-1.4.4-zipapp.tar` that can be bundled without touching system
packages.

It is deferred because it is not on the critical path to a working game, it
interposes a container between injector and target that complicates both
injection and path visibility, and direct-wine is the configuration actually
measured. Revisit when targeting distros where GE-Proton's bundled libraries are
insufficient. Recorded here so the earlier "bundle umu" decision is visibly
deferred rather than quietly dropped.

### Prefix and path rerouting

A prefix created by `wineboot -u` contains `dosdevices/c: -> ../drive_c` and
`dosdevices/z: -> /`. Both matter:

- Game trees are served through the shim, so they need no real directories —
  this is where the "no clutter from multiple game installs" requirement is
  satisfied. Content lives once; each session composes a view.
- **`dosdevices/z:` is removed.** It maps the entire host filesystem into the
  game's namespace. Deleting it gives containment Windows does not have. Any
  path the game legitimately needs gets an explicit drive letter.

`WINEARCH=win64` does **not** avoid the 32-bit dependency: the `wine` launcher
probes the 32-bit loader regardless, and `wineboot` fails with
`/lib/ld-linux.so.2: could not open`. A 32-bit runtime (`lib32-glibc`,
`lib32-gcc-libs` on Arch) is a hard prerequisite and must be detected with a
clear diagnostic rather than surfaced as Wine's error. FreeType warnings are
cosmetic for console targets.

## 6. The identity gap

`GetFinalPathNameByHandleW` on a redirected handle returns the **backing** path,
where Windows returns the virtual one. The shim spoofs identity through
`NtQueryInformationFile(FileNameInformation)`; Wine's kernelbase evidently
resolves final paths by another route.

This is a Wine divergence in a Win32 wrapper, not a hole in the model, and it is
**not guessed at in this spec**. The work is to trace which call Wine actually
makes — `WINEDEBUG=+relay` on `GetFinalPathNameByHandleW`, or reading
`kernelbase`'s implementation — and cover it. Candidates are `NtQueryObject`
with `ObjectNameInformation` and Wine's internal unix-to-DOS path conversion, but
the trace decides, not this list.

Games do check file identity, so this is required for real workloads. It is
sequenced after the end-to-end run because that run does not depend on it.

## 7. Verification

Three of the four gates are new, because Windows CI structurally cannot run any
of this.

| gate | proves | where |
|---|---|---|
| `cargo test --no-fail-fast` on Windows | no Windows regression — the binding constraint | local + CI |
| `cargo clippy --all-targets -- -D warnings` | lint parity | local + CI |
| `cargo tree -p vfs-embed --target x86_64-unknown-linux-gnu` names no Windows crate | the Cargo gating actually holds | local |
| shim + ring + Director end-to-end under GE-Proton | **the whole design** | Arch box |

`--target` on the tree query is load-bearing: `cargo tree` resolves for the
**host** by default, so on Windows both `cfg(windows)` dependencies and
dev-dependencies resolve and the query reports Windows crates even when the
property holds perfectly.

`cargo check --target x86_64-unknown-linux-gnu` is necessary but **not
sufficient** and must not be reported as proof of portability: it cannot detect a
`windows-sys` dependency at all, because `windows-sys` emits extern declarations
that type-check on any target and fail only at link. `cargo check --target
x86_64-unknown-linux-gnu -p vfs-win` *succeeds* today.

The end-to-end run is the definition of done for increment 1, not a follow-up.
Every probe so far isolated a single mechanism; the composed stack — valid
config, ready marker, ring traffic, Director serving a real read — has never run
under Wine, and that is where integration surprises live.

## 8. What must not regress

- **No behaviour change on Windows.** The `hook.rs` change is additive: an
  already-conditional hook becomes conditional, and `skipped_detours()` is new
  surface. Verified 2026-09-01: `cargo test -p vfs-shim --no-fail-fast` →
  25 binaries, 129 passed, 0 failed, 2 ignored.
- **No interface change for embedders.** `vfs-embed::Session` keeps its shape;
  `launch()` acquires a second implementation, not a second signature.
- **The protocol descriptor must not drift.** `bin/regen-protocol` and the
  `git diff --exit-code resources/` gate stay green. Nothing here touches wire
  format — the ring's *bytes* are unchanged; only the memory's backing changes.
- **CI stays green on all three jobs**, which it is as of `37a782d`.

## 9. Scope

**In scope:** file-backed mapping on both ends (new `vfs-unix` crate for the
Linux half), `SpinNotifier` wiring,
`vfs-proton` with verified GE-Proton acquisition and prefix construction,
`vfs-embed` dependency gating and target-dispatched `launch()`, and the
end-to-end run under Wine.

**Out of scope, deliberately:**

- The identity gap (§6) — sequenced next, needs a trace first.
- A futex-based notifier. `SpinNotifier` ships first; its CPU cost is accepted.
- umu / pressure-vessel (§5).
- A real game. `hollow_hello.exe` is single-threaded and a few hundred KB;
  Skyrim is not. Increment 1 proves the stack, not the workload.
- A Linux build of the Node addon.
- Renaming `fuse_client.rs`, whose name predates and misdescribes this work.

## 10. Risks

- **Wine divergence beyond ntdll exports.** The identity gap is one instance;
  there will be more, and each is found by running things, not by reading. This
  is the main reason increment 1's done-ness is an end-to-end run.
- **`SpinNotifier` under a real game's load.** Spinning on both ends may be
  unacceptable at Skyrim's I/O rate. Mitigation is the deferred futex work;
  the trigger is a measurement, not a hunch.
- **GE-Proton upgrades.** `version` parsing and the 18-of-20 export set are
  observations about GE-Proton11-6, not guarantees about 11-7. The export
  handling degrades safely by construction (absent means unhookable means
  unreachable); a *new* export appearing is the dangerous direction, which is
  exactly what `skipped_detours()` makes visible.
- **Injection under a container.** If umu is revisited, pressure-vessel sits
  between injector and target and this needs re-proving.

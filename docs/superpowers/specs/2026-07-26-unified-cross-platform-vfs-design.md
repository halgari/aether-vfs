# Unified cross-platform VFS: aether-vfs + vfs merge

**Date:** 2026-07-26
**Status:** Approved (design); plan pending

## What

Merge two independently-built virtual-filesystem projects into one library
with a single Clojure-facing interface that works on both Linux and Windows:

- **aether-vfs** — Clojure/JVM. A `Provider`-based VFS (the software definition
  of files) with a Linux **FUSE** mount adapter (jnr-fuse + an FFM zero-copy
  read path) and a Proton launch runtime.
- **vfs** (`halgari/vfs`) — Rust. A usermode Windows VFS that injects a shim
  into a target process, hooks `NtCreateFile` et al., and serves file ops over
  a shared-memory control ring + bulk data arena to a userspace "director"
  (a userspace FUSE kernel) hosting backends.

The two have independently converged on nearly the same abstraction:

| Concept | aether-vfs (Clojure) | vfs (Rust) |
|---|---|---|
| definition of files | `Provider`: `lookup`/`readdir`/`open-file`/`read-at`/`write-at`/`release-handle` | `Backend`: `getattr`/`readdir`/`open`/`read`/`release` |
| routing | `router` (glob → provider) | `Director.mount(prefix, backend)` |
| composition | `compose` (layered / overlay / inline) | Director overlay resolve |
| delivery | jnr-fuse mount | `NtCreateFile` hooks + injection |

The merged product keeps **one** interface — aether's `Provider` / `ReadInto`
/ `Writable` protocols — and gives it **two** OS-specific delivery adapters
that both dispatch into it: FUSE on Linux (exists), and a JVM-hosted
shared-memory ring **server** on Windows (new). The consumer always writes
Clojure and imports one library.

## Decisions (as approved)

1. **Consumer is always JVM/Clojure.** A unified FUSE-like interface in
   Clojure; everything else is cross-platform wiring and implementation.
2. **Approach ① — the JVM is the ring server.** On Windows the Clojure process
   maps the shared section and runs the `vfs-ipc` ring loop itself via FFM
   (`java.lang.foreign` / Panama), dispatching opcodes into the `Provider`
   stack and writing bulk read data straight into the shared arena
   `MemorySegment`. This is the *same* `ReadInto` zero-copy path aether uses to
   write into the kernel's FUSE buffer on Linux — on Windows the destination is
   the IPC arena. Rejected alternatives: a Rust daemon calling back to the JVM
   (two hops on the read path, always two processes); serializing providers
   down to Rust (impossible — providers are arbitrary Clojure).
3. **JVM owns the Windows lifecycle via FFM.** The JVM creates the named
   section + events (`CreateFileMapping`/`CreateEvent`), maps the view,
   initializes the ring header, and runs the server loop. A small **Rust launch
   helper** only spawns the target suspended, injects the shim, and resumes.
4. **Read + write from the start.** The first Windows milestone targets the
   full `Writable`/overlay path (create/unlink/rename/truncate + `.wh.*`
   whiteouts), not read-only. Because the JVM is the daemon, write *dispatch*
   is Clojure `Writable`+`overlay` (already implemented and tested in aether);
   the new native work is the write-side **hooks** in the shim.
5. **One repo, OS split explicit.** Both codebases move into a single
   Clojure-rooted repository. The library name stays **aether-vfs**
   (`aether.vfs.*`). OS-specific Clojure lives under `os/{linux,windows}/`; the
   native engine lives under `rust/`. The core namespaces never mention an OS.
6. **Rust owns the protocol; the JVM mirrors it — mechanically, CI-enforced.**
   See "Protocol single source of truth & anti-drift" below. This scaffolding
   lands in M1, before any Windows code, so every later commit is checked.

## Architecture

```
                 ┌────────────── Clojure library (OS-agnostic core) ──────────────┐
   user code ───▶│ provider · types · error · router · compose · providers/{…}     │
   (mount root)  │ read_pool · inode        Provider / ReadInto / Writable  ◀─ the │
                 │                            ONE interface                          │
                 └──────────▲───────────────────────────────────▲───────────────────┘
                            │ dispatch                            │ dispatch
              ┌─────────────┴──────────┐          ┌───────────────┴────────────────┐
              │ os/linux/fuse.clj       │          │ os/windows/ring.clj  (NEW)      │
              │ jnr-fuse adapter        │          │ FFM ring SERVER: maps section,  │
              │ ReadInto → kernel buf   │          │ runs opcode loop, ReadInto →    │
              │ (exists)                │          │ arena MemorySegment             │
              └──────────▲──────────────┘          └───────────────▲─────────────────┘
                         │ /dev/fuse                                │ vfs-ipc ring+arena
                  ┌──────┴──────┐                          ┌────────┴──────────┐
                  │ Linux FUSE   │                          │ Rust shim (hooks   │
                  │ kernel       │                          │ NtCreateFile…),    │
                  └─────────────┘                          │ injected in target │
                                                           └────────────────────┘
```

## Repository layout

```
<repo root>/   (aether-vfs — the Clojure library)
├── deps.edn
├── src/aether/vfs/
│   ├── provider.clj  types.clj  error.clj  router.clj  compose.clj   ← OS-agnostic
│   ├── read_pool.clj  inode.clj  providers/{inline,layered,overlay,   ← OS-agnostic
│   │                                        passthrough,fsutil}.clj
│   ├── mount.clj            NEW  (mount root target opts) → dispatch by OS
│   ├── protocol.clj         NEW  loads the generated protocol descriptor (§ anti-drift)
│   └── os/
│       ├── linux/  fuse.clj  proton.clj                              ← existing, relocated
│       └── windows/                                                   ← all NEW
│           ├── section.clj   FFM: CreateFileMapping/CreateEvent, map view, init header
│           ├── ring.clj      FFM: ring server loop (producer/consumer, event wait/signal)
│           ├── arena.clj     FFM: bulk-arena writes (ReadInto destination)
│           ├── wire.clj      opcode/status codec, driven by the descriptor
│           ├── inject.clj    invoke the Rust launch helper; lifecycle/teardown
│           └── launch.clj    Windows launch entry (spawn target against the mount)
├── resources/
│   └── protocol-descriptor.edn   generated, committed (§ anti-drift)
│   └── native/                    packaged shim.dll / payload / launch helper
├── test/aether/vfs/…             existing provider/router/overlay tests + new conformance
└── rust/                          the Windows delivery engine (vendored)
    ├── Cargo.toml (workspace)
    └── crates/
        ├── vfs-payload   no_std in-game hook payload (the "FUSE driver")
        ├── vfs-shim / vfs-shim-dll   injected DLL, ring CLIENT
        ├── vfs-inject    DLL injection
        ├── vfs-launch    spawn-suspended / resume / child-process propagation
        ├── vfs-ipc       ring + arena memory layout (SOURCE OF TRUTH)
        ├── vfs-protocol  wire codec, opcodes, status (SOURCE OF TRUTH)
        ├── vfs-win       Win32 helpers still needed by the shim/injector
        ├── xtask-descriptor   emits protocol-descriptor.edn + golden vectors
        ├── fixtures (vfs-fixture-vproxy, vfs-fixture-staticimp)
        └── vfs-core · vfs-server · vfs-director   REFERENCE daemon only (§ reference)
```

The physical merge (M1) hosts the unified tree in the aether-vfs repo and moves
the Rust crates under `rust/`, preserving both git histories via subtree merge.

## Division of responsibility (Windows)

| Concern | Owner |
|---|---|
| Section + events creation, view mapping, ring header init | **JVM (FFM)** — `os/windows/section.clj` |
| Ring server loop, event wait/signal, arena writes | **JVM (FFM)** — `os/windows/{ring,arena}.clj` |
| Opcode dispatch → `Provider`/`Writable`, overlay resolve, handle table | **JVM (Clojure)** — reuses `router`/`compose`/`overlay` |
| In-game read + write hooks (`NtCreateFile`, write-disposition, `NtSetInformationFile` class 64 delete / class 10 rename), no_std payload | **Rust** — `vfs-payload`, `vfs-shim` (ring CLIENT) |
| Spawn-suspended, inject DLL, resume, child-process propagation | **Rust launch helper** — `vfs-inject`/`vfs-launch`, invoked by the JVM |
| Wire format + ring/arena memory layout | **Rust source-of-truth; JVM mirrors via generated descriptor** |

## Data flow (Windows: open + read + write)

1. JVM: `CreateFileMapping`(named) + two `CreateEvent`s via FFM → map view →
   write the `vfs-ipc` ring header/geometry + protocol version → start server
   thread(s) waiting on the client event.
2. JVM invokes the Rust launch helper: spawn target **suspended**, pass section
   + event names + geometry via env, inject shim, resume.
3. Shim opens the section by name, verifies the protocol version in the header
   (§ handshake), installs hooks. Game's `NtCreateFile("Data/x.esp")` → shim
   emits `OP_OPEN` on the ring.
4. JVM server wakes, decodes, calls `provider/open-file` → `{:fh …}`; `OP_READ`
   → `read-into!` writes bytes into the **arena**, returns arena offset (bulk)
   or inline payload; shim returns bytes to the game.
5. Write: game write-disposition open / `NtSetInformationFile` → `OP_WRITE` /
   `OP_DELETE` / `OP_RENAME` → JVM `Writable` dispatch → `overlay` copy-up into
   the writable overrides dir + `.wh.*` whiteouts. The base is never mutated.

## Protocol single source of truth & anti-drift

The one real technical risk is the JVM's FFM ring server matching the Rust
`vfs-ipc`/`vfs-protocol` byte layout exactly, and the two staying matched while
work happens on only one side. The guard is a layered, mechanically-enforced
system, and it is built in **M1 — before any Windows code exists** — so every
subsequent commit is checked against it.

**Rust is the single source of truth.** The `vfs-protocol` (wire codec,
opcodes, status, flags) and `vfs-ipc::layout` (ring/arena header offsets, slot
stride formula, arena defaults, `BULK_THRESHOLD`, `READ_RESP_BULK_BIT`, header
magic) crates own the protocol. The Clojure side never hand-writes a magic
number.

The four enforced mechanisms:

1. **Generated descriptor — Clojure never hardcodes.** A Rust binary
   (`xtask-descriptor`) emits `resources/protocol-descriptor.edn`: every
   opcode/status/flag value, each wire message's field offsets/sizes/order, the
   ring + arena header layout, all shared constants, a **protocol version**, and
   a content hash. `aether.vfs.protocol` loads this descriptor and it drives
   `os/windows/{wire,ring,arena,section}.clj`. Change the Rust protocol →
   regenerate → the Clojure side follows automatically. No parallel constant
   table to drift.

2. **Committed + staleness gate.** The descriptor and golden vectors are
   committed. CI regenerates them and fails on any diff
   (`git diff --exit-code`). You cannot change the Rust protocol without
   regenerating and committing the artifact the Clojure side consumes — so a
   one-sided Rust change that forgets the Clojure side is impossible to merge.

3. **Cross-language golden vectors.** `xtask-descriptor` also emits canonical
   encode/decode vectors (bytes for each opcode req/resp + a dumped ring
   header). A Rust test asserts its codec produces those exact bytes; a Clojure
   test asserts *its* codec produces byte-identical output and decodes them back
   to the same values. Both sides pin to the same golden file.

4. **Runtime version handshake.** The ring header carries `protocol_version` +
   a truncated descriptor hash. On connect the shim (client) and the JVM server
   both check it in the mapped header; a mismatch hard-fails with a clear error
   instead of corrupting silently. This catches a stale packaged `shim.dll`
   built against a different protocol than the running JVM server. A test also
   asserts the packaged native artifacts' stamped version == the descriptor
   version.

**Ownership rule (documented + enforced by mechanism 2):** every protocol or
layout change starts in Rust (`vfs-protocol` / `vfs-ipc::layout`), then
`regenerate`, then the Clojure side consumes the new descriptor. The CI
staleness gate makes the direction non-optional.

## Reference Rust daemon

`vfs-core`, `vfs-server`, and `vfs-director` (the Rust userspace daemon) are
**not** on the runtime path anymore — the JVM is the daemon. They are retained
in `rust/` as a **reference implementation and conformance harness**: a
pure-Rust test can drive the same wire protocol end-to-end (Rust shim client ↔
Rust director server), giving an independent oracle for the wire/layout
contract that the JVM server is also checked against. If they become a
maintenance burden they can be dropped without affecting the product.

## Error handling

- Provider/dispatch errors map through aether's existing `error` namespace
  (errno categories) on both adapters; the Windows adapter translates an errno
  category to the ring `ST_*` status the shim expects (`ST_NOT_FOUND`,
  `ST_BAD_FH`, `ST_IS_DIR`, …), the same set `vfs-protocol` already defines.
- A malformed or oversized ring request is isolated to a status reply — it must
  never tear down the server loop (mirrors aether's FUSE adapter isolating a
  bad request to an errno).
- Protocol-version mismatch at handshake → hard fail with a diagnostic naming
  both versions.
- JVM GC pause risk: the game's synchronous file I/O blocks on the server, so
  the read path must not allocate on the hot path (direct `MemorySegment`
  writes via `ReadInto`, warmed server threads, bounded in-flight requests as
  aether already does for FUSE). Noted as a risk; aether already serves a live
  game over FUSE from the JVM, so JVM-as-game-filesystem is proven — Windows
  over IPC is the analog.

## Testing

- **OS-agnostic** provider/router/overlay/compose tests: carried over from
  aether unchanged; must stay green throughout (regression guard for M1).
- **Wire conformance** (Clojure ↔ golden ↔ Rust): mechanism 3 above.
- **Ring conformance harness (no game):** a small Rust ring **client** maps the
  JVM-created section and runs the handshake + `GETATTR`/`READ`/`WRITE`,
  asserting it gets provider data back. This is the Windows analog of aether's
  real-`/dev/fuse` `mount-test`, and is the M2 acceptance proof.
- **End-to-end:** an existing Windows fixture exe (`vfs-fixture-*`) opens,
  reads, and writes a virtual file served by a Clojure provider through real
  injection + hooks.
- **Descriptor staleness gate** in CI: mechanism 2.

## Milestones

- **M1 — Merge, restructure, and stand up the anti-drift scaffold.** One repo,
  `os/{linux,windows}` split, core protocols unified and unchanged, Rust under
  `rust/`. Build `xtask-descriptor` (descriptor + golden vectors),
  `aether.vfs.protocol` loader, the wire-conformance test, and the CI staleness
  gate. Full Linux test suite green (no regression). **The anti-drift system
  exists before any Windows delivery code.**
- **M2 — FFM ring server (read).** Clojure `section`/`arena`/`ring`/`wire` over
  FFM, proven against the Rust ring-client harness for `GETATTR`/`READDIR`/
  `OPEN`/`READ` with bulk-arena zero-copy. No injection yet.
- **M3 — Injection wired, end-to-end read.** JVM creates the section → Rust
  launch helper injects the shim → a real Windows fixture reads a virtual file
  served by a Clojure provider. Version handshake enforced.
- **M4 — Write path.** Shim write-side hooks (write-disposition create,
  `NtSetInformationFile` delete/rename) + JVM `Writable`/overlay end-to-end
  (create/delete/rename/copy-up/whiteout).
- **M5 — Unified entry + docs.** `aether.vfs.mount`/`launch` OS dispatch;
  README covering both OSes; packaging of native artifacts as JVM resources
  with the stamped-version test.

Each milestone gets its own plan → implementation cycle; this spec covers the
whole arc.

## Out of scope

- macOS delivery (aether's FUSE path may work via macFUSE, untested; not a goal
  here).
- Reconciling the mauvi mod manager onto this library (a separate future task,
  per aether's extraction design).
- A pure-Rust product surface — the consumer is always Clojure. The Rust
  reference daemon exists only for conformance.

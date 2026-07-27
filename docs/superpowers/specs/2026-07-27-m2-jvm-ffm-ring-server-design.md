# M2 — JVM FFM ring server (read path)

**Date:** 2026-07-27
**Status:** Approved (design); plan pending
**Predecessor:** M1 (merge + anti-drift scaffold) — complete, on `master`.
**Parent design:** `docs/superpowers/specs/2026-07-26-unified-cross-platform-vfs-design.md`

## What

Make the JVM the **Windows delivery adapter** for the unified `Provider`
interface, exactly as `os/linux/fuse.clj` is the Linux adapter. On Windows a
process's file ops arrive over the `vfs-ipc` shared-memory control ring; M2
builds the **server side of that ring, in Clojure, via FFM/Panama** — mapping
the shared section, mirroring the lock-free CAS ring state machine, dispatching
opcodes into an aether `Provider`, and writing bulk read data straight into the
shared arena (zero-copy). M2 covers the **read path only** and is proven by a
**separate Rust process** (`RingClient`) driving the JVM server across the
shared section.

M2 delivers the building blocks (`os/windows/{section,ring,arena,server}.clj`),
not the unified `mount` entry (M5) and not the injection/hooks (M3). There is no
game and no injected shim in M2 — the Rust harness stands in for the shim.

## Decisions (as approved)

1. **Spin-based, no OS events.** The shipping `vfs-ipc` protocol uses
   `SpinNotifier` — correctness rests entirely on the ring atomics; OS-event
   wakeups are explicitly deferred in Rust too. So the JVM server needs **no**
   `CreateEvent`/`WaitForSingleObject` FFM binding: it spin-polls the ring and
   mirrors the CAS state machine. (Event-based wakeup is a later optimization,
   in lockstep with the Rust side.)
2. **Single-threaded server** for M2 (the Rust `DEFAULT_WORKER_COUNT = 4` worker
   pool is a later scaling step). One serve loop is enough to prove correctness.
3. **Read path only:** `OP_GETATTR`, `OP_READDIR`, `OP_OPEN`, `OP_READ`,
   `OP_CLOSE`. Write opcodes are M4.
4. **JVM owns the section via FFM** (per the parent design): the JVM calls
   `CreateFileMapping`/`MapViewOfFile` and initializes the ring header.
5. **Cross-process Rust `RingClient` harness is the acceptance proof** — the
   analog of aether's real-`/dev/fuse` mount test. It exercises **ring + the
   zero-copy arena together** (getattr, readdir, a small inline read, and a
   large `>64 KiB` bulk read).
6. **Deferred F2 lands here.** As the server-side wire codecs are added, add
   golden vectors for `open-resp` and `read-resp-bulk` (and a ring-header
   byte-dump) under the M1 conformance test, so every message the JVM server
   speaks stays byte-pinned to Rust. F1 (enum-exhaustive emitter) remains a
   separate anti-drift hardening task, not part of M2.

## Architecture

```
   Rust process (harness = stand-in for the future shim)          JVM process (Clojure)
   ┌──────────────────────────────────────────┐                  ┌───────────────────────────────┐
   │ vfs-win::SharedMapping::open(name)         │  shared section  │ os/windows/section.clj          │
   │ vfs_ipc::RingClient(SpinNotifier)          │◀────ring + ─────▶│  CreateFileMapping+MapViewOfFile │
   │  submit(GETATTR/READDIR/OPEN/READ[/BULK])  │      arena        │  → MemorySegment                │
   │  decode resp (vfs-protocol); bulk→arena     │  (one mapping)   │ os/windows/ring.clj  (CAS SM)   │
   └──────────────────────────────────────────┘                  │ os/windows/arena.clj (zero-copy)│
                                                                   │ os/windows/server.clj           │
                                                                   │   dispatch → Provider (router/  │
                                                                   │   compose/inline) + fh table    │
                                                                   └───────────────────────────────┘
```

The section is a **single mapping** laid out `[ ring header + slots | arena
banks ]`, page-file-backed named shared memory. This is the same object the Rust
`IpcServe` builds today; M2 builds the JVM equivalent of its server loop.

## Components (all under `src/aether/vfs/os/windows/`)

All ring offsets, opcodes, statuses, flags, and sizes come from
`aether.vfs.protocol` (the M1 descriptor) — never literals.

### `section.clj` — FFM shared section *(Windows-only)*
FFM downcalls via `java.lang.foreign.Linker` to `kernel32`:
- `CreateFileMappingW(hFile = INVALID_HANDLE_VALUE (-1), lpAttributes = NULL,
  flProtect = PAGE_READWRITE (0x04), dwMaximumSizeHigh = high32(size),
  dwMaximumSizeLow = low32(size), lpName = wide(name))` → `HANDLE` (0 = fail →
  `GetLastError`).
- `MapViewOfFile(handle, FILE_MAP_ALL_ACCESS (0xF001F), 0, 0, size)` → base
  address; wrap `MemorySegment.ofAddress(base).reinterpret(size)`.
- Cleanup: `UnmapViewOfFile(base)`, `CloseHandle(handle)`.
Returns `{:handle h :segment seg :size n :name name}`; `close!` unmaps + closes.
Requires `--enable-native-access=ALL-UNNAMED` (already in the `:test` alias).

### `ring.clj` — ring state machine over a `MemorySegment` *(cross-platform)*
Pure over any `MemorySegment` (a heap segment works → testable on any OS):
- `init` — write header (`magic`, `version`, `slot_count`, `slot_stride`,
  `payload_cap`, seqs = 0) and set every slot `state = ST_FREE`; compute
  `slot_stride = align8(SLOT_HEADER_SIZE + payload_cap)`.
- `server-take` — scan slots; `VarHandle.compareAndSet(state, ST_SUBMITTED,
  ST_PROCESSING)` (acquire on success). Returns slot or nil.
- `read-request` — read `opcode`, `flags`, `req_id`, `payload_len`, payload.
- `server-complete` — write `status`, `payload_len`, payload, then
  `VarHandle.setRelease(state, ST_COMPLETED)`.
Scalar/atomic fields use native-order `JAVA_INT`/`JAVA_LONG` var handles
(x86 native = LE = Rust's `AtomicU32`); wire **payloads** use the explicit-LE
`aether.vfs.wire` codec. The `state` field is accessed only through the
`compareAndSet`/`getAcquire`/`setRelease` access modes to match Rust's
`Acquire`/`Release` orderings.

### `arena.clj` — bulk data arena *(cross-platform over a segment)*
Mirror of `vfs_ipc::DataArena`: `mapping-offset`, `bank-size = arena_len/banks`,
`bank-index(slot) = slot % banks`, `bank-mapping-offset(slot)`. `fill-bank`
gives the provider a `MemorySegment` **slice of the arena bank** to write into
directly (the `ReadInto` zero-copy destination), returning
`(mapping-offset, bytes-written)`.

### `server.clj` — serve loop + dispatch *(server logic cross-platform; run on Windows section)*
Spin loop: `server-take` → decode opcode → dispatch to a `Provider` → encode
response → `server-complete`; stop flag; runs on its own thread. Mirrors the
Rust `dispatch_director` semantics and keeps its own **open-handle table**
(`fh → {provider, backend-handle, size}`) like the Rust `Director`:
- `OP_GETATTR` → `provider/lookup` → `encode-getattr-resp` (found/kind/size/mtime).
- `OP_READDIR` → `provider/readdir` → `encode-readdir-resp`.
- `OP_OPEN` → `provider/open-file`, allocate `fh`, → `encode-open-resp`.
- `OP_READ` → look up `fh`; if `len ≤ BULK_THRESHOLD` (64 KiB) and not
  `FLAG_READ_BULK`, inline via `read-at` + `encode-read-resp`; else write into
  the arena bank (`read-into!`/`read-at`) and `encode-read-resp-bulk(len,
  arena_offset)`.
- `OP_CLOSE` → `provider/release-handle`, drop `fh`.
Takes any aether `Provider` (inline, `router`, `compose`).

### `wire.clj` extension + **F2 golden vectors**
Add the **server-side** codecs M1 didn't need (M1 added client-side encoders):
`decode-path-req`, `decode-open-req`, `encode-open-resp`, `decode-read-req`,
`encode-read-resp-bulk`, `decode-close-req`. Each is added under the existing
byte-for-byte conformance test. **F2:** extend `xtask-descriptor`'s
`golden_vectors` with `open-resp`, `read-resp-bulk`, and a ring-header
byte-dump, regenerate `resources/protocol-golden.edn`, and assert the new
Clojure codecs match — closing the "unpinned encoder" gap the M1 final review
raised.

### Rust harness — `rust/crates/vfs-ring-harness` (new bin crate)
Opens the JVM-created section via `vfs_win::SharedMapping::open(name, size)`,
builds `RingClient::new(seg, SpinNotifier)`, and submits the op sequence,
decoding responses with `vfs_protocol`; for the bulk read it reads bytes from
the arena at the returned mapping offset. Asserts each response against the
expected provider bytes; exits `0` on success, non-zero with a diagnostic on
mismatch. Section name + geometry arrive via argv/env from the orchestrating
test.

## Data flow (one bulk read)
1. Harness `submit(OP_READ, FLAG_READ_BULK, {fh, offset, len})`: `claim_free` →
   `publish_request` → `state = ST_SUBMITTED`.
2. JVM `server-take` CAS `SUBMITTED→PROCESSING`; `read-request` decodes the
   `ReadReq`.
3. Dispatch: look up `fh`, `provider/read-at`/`read-into!` writes bytes **into
   the arena bank** `MemorySegment` slice.
4. `server-complete`: payload = `encode-read-resp-bulk(bytes, arena_offset)`,
   `state = ST_COMPLETED` (release).
5. Harness `take_response`; sees `READ_RESP_BULK_BIT`; reads `bytes` from the
   arena at `arena_offset`; asserts.

## Testing & CI

- **Cross-platform component tests (run in the existing ubuntu Clojure job):**
  `ring` (CAS state machine), `arena` (bank math + zero-copy fill), `server`
  dispatch, and the extended `wire`/golden conformance — all exercised over a
  **heap `MemorySegment`**, no Windows section required. A Clojure-only
  producer/consumer test drives a slot through `SUBMITTED→PROCESSING→COMPLETED`
  to verify the atomics and framing without any native section.
- **Windows-only integration proof (new `windows-clojure` CI job):** the real
  `section.clj` FFM mapping + the **cross-process Rust `RingClient` harness**.
  A Clojure `deftest` (self-skips off Windows, like the FUSE mount-test
  self-skips off Linux) creates a named section, starts the server thread with
  an inline provider serving a small file and a `>64 KiB` file, spawns the
  prebuilt `vfs-ring-harness` exe with the section name, and asserts exit 0.
  CI gains a `windows-clojure` job (Java 26 + deps.clj + `cargo build -p
  vfs-ring-harness` + `clojure -M:test`) so this headline proof runs in CI and
  is not local-only.
- **Wire/golden staleness gate** (M1) continues to guard the new vectors.

**CI note (line endings):** any new file compared byte-for-byte (golden `.edn`)
stays `eol=lf` via the M1 `.gitattributes` — the M1 CRLF regression is already
guarded; new `resources/*.edn` inherit it.

## Risks

- **FFM atomics across the process boundary (the crux).** Java `VarHandle`
  `compareAndSet`/`getAcquire`/`setRelease` on a `MemorySegment` must match
  Rust's `AtomicU32` `Acquire`/`Release` CAS. Sound in principle (same hardware
  barriers; shared memory is cache-coherent on one machine), but it is the
  **first thing the plan proves** — a focused Clojure-server ↔ Rust-client
  atomics/handshake test before any Provider dispatch. If a mismatch appears,
  it surfaces here, cheaply, not buried under dispatch logic.
- **`MemorySegment.reinterpret` + native access** requires the `:test` alias
  flag (already present) and is a restricted method — confined to `section.clj`.
- **JVM serving latency/GC** is a real concern for a live game but out of scope
  for the M2 proof (no game); noted in the parent design.

## Milestones (tasks — detailed in the plan)

1. **`section.clj`** FFM map/unmap + a Windows smoke test (create → map → write/
   read a byte → unmap); the atomics handshake test (Clojure writes header +
   a slot state; a tiny Rust check reads it) to de-risk FFM ordering first.
2. **`ring.clj`** CAS state machine + cross-platform heap-segment tests
   (`init`/`server-take`/`read-request`/`server-complete` round-trip).
3. **`wire.clj`** server-side codecs + **F2** golden vectors (regenerate,
   conformance-test).
4. **`arena.clj`** bank layout + zero-copy `fill-bank` tests.
5. **`server.clj`** dispatch + fh table over a `Provider` (heap-segment test
   with an inline provider, inline + bulk).
6. **`vfs-ring-harness`** Rust crate (opens section, `RingClient`, asserts).
7. **Cross-process proof + `windows-clojure` CI job** — the Windows-only
   orchestrating test and the CI wiring.

## Out of scope (unchanged)
- OS event notifier (spin only, in lockstep with Rust).
- Write path — create/unlink/rename/truncate/whiteout (M4).
- Injection / hooks / the real shim (M3).
- Unified `mount`/`launch` entry (M5).
- F1 enum-exhaustive emitter (separate anti-drift hardening).
- Worker-pool concurrency (single-threaded server for M2).

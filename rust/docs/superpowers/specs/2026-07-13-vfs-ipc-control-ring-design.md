# vfs-ipc Control Ring — Design Spec

**Status:** Approved-to-proceed (user delegated the full cycle), ready for
implementation planning.
**Date:** 2026-07-13
**Slice:** Third implementable slice — the `vfs-ipc` **control ring transport
mechanics**: message framing, a fixed-slot ring with an atomic slot state
machine, client/server endpoints, and an abstracted wakeup `Notifier`.
**Parent docs:** *Out-of-Process (IPC) Architecture* (§3, §5, §7, §10, §13, §15
G11), *Rust Implementation Guide* (§3, H1–H3, H6).
**Depends on:** nothing at compile time (byte-buffer core, like `vfs-shared`).

---

## 1. Context & positioning

The out-of-process design moves `vfs-core` into a server; injected shims talk to
it over a **recursion-free** transport: shared memory + event/futex signaling,
using only syscalls we do **not** hook (IPC doc §3). Named pipes/sockets are
rejected — they route through `NtCreateFile`/`NtReadFile`, which our hooks catch
→ infinite recursion.

This slice builds the **control ring** (IPC §2 region B, §5): a bounded array of
request slots through which a client submits a request and receives a response.
It is the transport *mechanics* — framing, the slot state machine, and the
client/server endpoint APIs — with the OS-specific wakeup abstracted behind a
trait. It does **not** implement the FUSE opcode semantics (§7) — those are
server logic in a later slice; the ring treats `opcode`/`payload` as opaque.

### Scope decisions (consistent with slices 1–2)

1. **Transport mechanics only.** Framing + ring + slot state machine + endpoints
   + `Notifier` trait. No opcode handlers, no `vfs-core`/`vfs-shared` coupling.
2. **Byte-buffer / shared-segment, OS-independent.** Operates on a caller-owned
   shared-memory segment. The crate imports **no** OS file/section/process API
   (recursion-free by construction, G11). Stable Rust.
3. **Fixed per-slot payload region (MVP).** Each slot carries a fixed-size inline
   payload buffer (`payload_cap` bytes) used for both request and response. The
   separate zero-copy **bulk data arena** (IPC §5) for large/variable payloads is
   **deferred** — payloads larger than `payload_cap` return an error for now.
4. **Abstracted signaling.** A `Notifier` trait provides wake/wait; this slice
   ships a spin implementation (correctness rests on the atomics; the notifier is
   an advisory wakeup). The real Nt event / `NtWaitForAlertByThreadId`
   implementation is **deferred** to the OS-wiring/server slice.
5. **Centralized, audited `unsafe`.** A shared-memory ring inherently has
   concurrent writers to one segment; `unsafe` is unavoidable. **All** of it is
   confined to one small `SharedSeg` type (raw `*mut u8` + len with audited
   atomic/byte accessors). Crate is `#![deny(unsafe_code)]` with localized
   `#[allow(unsafe_code)]` only inside `SharedSeg`.
6. **Bitness-neutral** (`#[repr(C)]`, fixed-width, `offset_of!` LE, layout
   asserts) — same wire-format discipline as `vfs-shared` (H6/G9/D0).

---

## 2. Scope & crate boundary

`crates/vfs-ipc`, stable Rust, `#![deny(unsafe_code)]` with localized allows in
`SharedSeg` only. No dependencies.

### In scope

- `SharedSeg` — a raw shared-memory segment abstraction (`*mut u8` + len,
  `Send`/`Sync`) with the crate's entire audited `unsafe` surface: atomic views
  (`AtomicU32`/`AtomicU64` at an offset) and payload byte read/write.
- Message framing: `#[repr(C)]` `RingHeader`, `SlotHeader`; constants; offsets via
  `offset_of!`; compile-time size/align asserts.
- `Ring` layout helpers: `init(seg, slot_count, payload_cap)` (writer lays out an
  empty ring) and `open(seg)` (validate magic/version/geometry).
- Slot state machine: `FREE → CLAIMED → SUBMITTED → PROCESSING → COMPLETED → FREE`
  via `AtomicU32` CAS with the correct orderings.
- `RingClient::submit(opcode, flags, payload) -> Result<Response, IpcError>`.
- `RingServer::serve_one(handler) -> Result<bool, IpcError>` (single request;
  the caller loops).
- `Notifier` trait + `SpinNotifier` (advisory; endpoints spin on the atomics).
- The opcode **catalog constants** (LOOKUP/GETATTR/READDIR/OPEN/MATERIALIZE/READ/
  WRITE/SETATTR/RENAME/DELETE/MKDIR/CLOSE/REGISTER_PROCESS/HEARTBEAT) as `u32`
  reference values — the ring does **not** interpret them.

### Explicitly out of scope (later slices)

- Real Nt event / `NtWaitForAlertByThreadId` `Notifier` implementation.
- The shared bulk **data arena** (variable/large payloads, zero-copy).
- Opcode **handler semantics** (server logic) and any `vfs-core`/`vfs-shared` use.
- Handle passing / `materialize` (server + handle-dup slice).
- Multi-session, worker-pool tuning, ALPC fallback, backpressure policy beyond a
  simple full-ring error/spin.
- The SYNC BLOCK's real OS objects (the `Notifier` abstracts them).

---

## 3. Segment layout (`#[repr(C)]`, little-endian, bitness-neutral)

Ring segment: **`[RingHeader][Slot 0][Slot 1]…[Slot N-1]`**. Each slot is
`[SlotHeader][payload_cap bytes]`, stride = `SLOT_HEADER_SIZE + payload_cap`
rounded up to 8. All fields fixed-width; atomics are `AtomicU32`/`AtomicU64` at
fixed, aligned offsets.

```
RingHeader (40 bytes, 8-aligned)
  magic:u32@0        // 0x56464950  b"VFIP"
  version:u32@4      // = 1
  slot_count:u32@8
  slot_stride:u32@12 // bytes per slot (header + payload_cap, 8-rounded)
  payload_cap:u32@16
  _pad:u32@20
  req_seq:u64@24     // AtomicU64: hands out unique req_ids
  submit_seq:u32@32  // AtomicU32: bumped each submit (server waits on changes)
  _pad2:u32@36

SlotHeader (32 bytes, 8-aligned)
  state:u32@0        // AtomicU32: FREE/CLAIMED/SUBMITTED/PROCESSING/COMPLETED
  opcode:u32@4
  flags:u32@8
  payload_len:u32@12 // valid bytes in the payload region (req then resp)
  status:i32@16      // response status (NTSTATUS-ish); 0 = ok
  _pad:u32@20
  req_id:u64@24
```

- **Slot payload** starts at `slot_off + SLOT_HEADER_SIZE`, capacity
  `payload_cap`. The request payload is overwritten by the response payload.
- **States:** `FREE=0, CLAIMED=1, SUBMITTED=2, PROCESSING=3, COMPLETED=4`.
- Offsets single-sourced via `offset_of!`; compile-time asserts guard sizes:
  `RingHeader = 40`, `SlotHeader = 32`, both 8-aligned. Compile under x64 now and
  `i686` in CI later (D0/G9).
- Little-endian, same-machine (shared memory).

---

## 4. `SharedSeg` — the entire audited `unsafe` surface

```rust
/// A raw view over a shared-memory segment. Holds a raw pointer (NOT derived from
/// a `&[u8]`, to permit sound interior mutation) plus a length. This type is the
/// crate's ONLY `unsafe`; every method documents its `// SAFETY:` reasoning.
pub struct SharedSeg { ptr: *mut u8, len: usize }
unsafe impl Send for SharedSeg {}
unsafe impl Sync for SharedSeg {}

impl SharedSeg {
    /// From a raw pointer to a mapped, 8-aligned shared segment of `len` bytes.
    /// SAFETY (caller): ptr is valid for `len` bytes for the segment's lifetime,
    /// 8-aligned, and this is the agreed sharing region.
    pub unsafe fn from_raw(ptr: *mut u8, len: usize) -> Self;

    pub fn len(&self) -> usize;

    /// `&AtomicU32`/`&AtomicU64` at `off` (must be in-bounds & aligned; checked,
    /// returns None otherwise). The single audited pointer→atomic cast.
    fn atomic_u32(&self, off: usize) -> Option<&AtomicU32>;
    fn atomic_u64(&self, off: usize) -> Option<&AtomicU64>;

    /// Copy `data` into the segment at `off` / copy `len` bytes out. Bounds-checked.
    /// Soundness rests on the ring protocol: only the slot's current owner
    /// (CLAIMED client or PROCESSING server) writes that slot's payload.
    fn write_bytes(&self, off: usize, data: &[u8]) -> bool;
    fn read_bytes(&self, off: usize, len: usize) -> Option<Vec<u8>>;

    /// Plain LE scalar reads for non-atomic header fields (opcode/flags/etc.),
    /// only read while the slot state guarantees the writer is done.
    fn read_u32(&self, off: usize) -> Option<u32>;
    fn read_i32(&self, off: usize) -> Option<i32>;
    fn read_u64(&self, off: usize) -> Option<u64>;
    fn write_u32(&self, off: usize, v: u32) -> bool;
    fn write_i32(&self, off: usize, v: i32) -> bool;
    fn write_u64(&self, off: usize, v: u64) -> bool;
}
```

For tests, an `OwnedSeg` helper allocates an 8-aligned buffer and hands out a
`SharedSeg` over it (keeping the backing allocation alive) — no `unsafe` at the
call site.

**Memory ordering discipline:** state transitions use `AtomicU32` CAS/store with
`Release` on publish (CLAIMED→SUBMITTED, PROCESSING→COMPLETED) and `Acquire` on
observe (the consumer/waiter), establishing happens-before so payload/header
writes are visible before the reader acts. This is the ring's correctness core
and is covered by the concurrency test (§7).

---

## 5. Endpoints

```rust
pub enum IpcError { RingFull, PayloadTooLarge, BadResponse, Closed, Layout }

pub struct Response { pub status: i32, pub payload: Vec<u8> }
pub struct Request<'a> { pub slot: u32, pub opcode: u32, pub flags: u32,
                         pub req_id: u64, pub payload: Vec<u8>, _seg: &'a SharedSeg }

pub struct RingClient<'a, N: Notifier> { seg: &'a SharedSeg, geom: Geom, notifier: N }
pub struct RingServer<'a, N: Notifier> { seg: &'a SharedSeg, geom: Geom, notifier: N }

impl<'a, N: Notifier> RingClient<'a, N> {
    pub fn new(seg: &'a SharedSeg, notifier: N) -> Result<Self, IpcError>;
    /// Claim a free slot, write request, publish, wait for completion, read the
    /// response, free the slot. Blocks (via the notifier / spin) until answered.
    pub fn submit(&self, opcode: u32, flags: u32, payload: &[u8])
        -> Result<Response, IpcError>;
}

impl<'a, N: Notifier> RingServer<'a, N> {
    pub fn new(seg: &'a SharedSeg, notifier: N) -> Result<Self, IpcError>;
    /// Wait for a submitted slot, claim it (SUBMITTED→PROCESSING), run `handler`
    /// to produce (status, response bytes), publish COMPLETED, wake the client.
    /// Returns Ok(true) if one was handled, Ok(false) on a spurious wake.
    pub fn serve_one(&self, handler: impl FnOnce(&Request) -> (i32, Vec<u8>))
        -> Result<bool, IpcError>;
}
```

**Client `submit` sequence:**
1. Claim: scan slots, `CAS state FREE→CLAIMED (Acquire)`. If none → `RingFull`
   (caller may retry) — MVP does a bounded spin then `RingFull`.
2. If `payload.len() > payload_cap` → `PayloadTooLarge` (free the slot).
3. Write payload + `payload_len`, `opcode`, `flags`, `req_id =
   req_seq.fetch_add(1)`.
4. `store state SUBMITTED (Release)`; `submit_seq.fetch_add(1)`;
   `notifier.notify_server()`.
5. Wait until `state == COMPLETED (Acquire)` (loop: check, else
   `notifier.wait_client(slot)`).
6. Read `status` + response payload (`read_bytes(payload_len)`).
7. `store state FREE (Release)`; `notifier.notify_slot_free()`.
8. Return `Response`.

**Server `serve_one` sequence:**
1. `notifier.wait_server()` (or spin) then scan for a `SUBMITTED` slot;
   `CAS SUBMITTED→PROCESSING (Acquire)`. None found → `Ok(false)`.
2. Read `opcode/flags/req_id/payload` → `Request`.
3. `(status, resp) = handler(&req)`. If `resp.len() > payload_cap`, set an
   overflow `status` and truncate/empty (bulk arena is deferred).
4. Write response payload + `payload_len` + `status`.
5. `store state COMPLETED (Release)`; `notifier.notify_client(slot)`.
6. Return `Ok(true)`.

Only one party writes a given slot's payload at a time (CLAIMED client, then
PROCESSING server), so payload writes never race; cross-slot concurrency is fine.

---

## 6. `Notifier` trait

```rust
/// Advisory wakeups. Correctness rests on the atomics; a Notifier only avoids
/// busy-spinning. The real impl (deferred) uses Nt events / NtWaitForAlertByThreadId.
pub trait Notifier {
    fn notify_server(&self);
    fn wait_server(&self);
    fn notify_client(&self, slot: u32);
    fn wait_client(&self, slot: u32);
    fn notify_slot_free(&self);
}

/// Ships in this slice: pure spin (all waits are `spin_loop()` hints, all notifies
/// no-ops). Endpoints remain correct because they re-check the atomics in a loop.
pub struct SpinNotifier;
```

`SpinNotifier` makes the endpoints busy-wait — acceptable for tests and a
functional (if CPU-hungry) MVP. Swapping in a real futex/Nt notifier later
changes only wakeup latency/CPU, not correctness.

---

## 7. Error handling & testing

### Errors

`IpcError { RingFull, PayloadTooLarge, BadResponse, Closed, Layout }`. No panics
on malformed segments: `Ring::open` validates magic/version/geometry (offsets and
`slot_count*slot_stride` within `len`) and returns `Layout` on mismatch; every
`SharedSeg` access is bounds-checked.

### Testing

- **Compile-time layout asserts** (§3): `RingHeader`/`SlotHeader` size/align.
- **`SharedSeg` unit tests:** atomic views round-trip; bounds-checked reads/writes
  return `None`/`false` out of range; misaligned/oob atomic offset → `None`.
- **Ring layout tests:** `init` then `open` round-trips geometry; `open` rejects
  bad magic/version/oversized geometry (`Layout`).
- **Single-threaded loopback:** on one thread, `submit`-half then `serve_one`
  then read — assert the state machine transitions and payload/`status`
  round-trip. (Uses a manual driver since `submit` blocks; alternatively a
  two-phase split for the deterministic test — the plan pins the mechanism.)
- **Threaded round-trip (the real proof):** a server thread loops `serve_one`
  echoing `payload`+`opcode` into the response; N client threads each `submit`
  many requests and assert each `Response` matches what that client sent
  (req/resp correlation via `req_id`, no cross-talk, no lost/torn payloads). Uses
  `SpinNotifier` and a `SharedSeg` shared across threads. Mirrors `vfs-shared`'s
  seqlock concurrency test.
- **Backpressure:** more concurrent in-flight requests than slots → `submit`
  returns `RingFull` (or blocks and eventually succeeds) without deadlock or
  corruption.
- **`PayloadTooLarge`:** a payload over `payload_cap` is rejected and the claimed
  slot is returned to `FREE`.
- **Recursion-free guard (G11):** a test/CI check that `vfs-ipc`'s dependency and
  import surface contains no file/section/process API (this slice trivially holds
  — no OS deps at all; documented so it can't regress).

---

## 8. Dependencies & toolchain

- **Toolchain:** stable Rust.
- **Dependencies:** none.
- **Unsafe:** `#![deny(unsafe_code)]` crate-wide; `#[allow(unsafe_code)]` confined
  to the `SharedSeg` module (segment construction, atomic views, byte access).
- **Workspace:** add `crates/vfs-ipc` to `members`.

---

## 9. Out-of-scope reminders (keep the slice tight)

- No real OS signaling (spin `Notifier` only).
- No bulk data arena (fixed per-slot payload; oversize → error).
- No opcode semantics; the ring is opcode-agnostic.
- No handle passing, no `materialize`, no server/provider logic.
- No `vfs-core`/`vfs-shared` dependency.
- No multi-session, worker-pool, or ALPC.

*End of spec.*

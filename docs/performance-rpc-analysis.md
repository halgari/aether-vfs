# Director FUSE RPC — Performance Analysis & Optimization Wins

**Date:** 2026-07-15  
**Scope:** Shipped pure-RPC path (`vfs-ipc` ring + `vfs-server` open table + thin `FuseClient` / hooks).  
**Bench baseline:** `docs/benchmarks/fuse-rpc-performance.md` (~200–380 MiB/s sequential RPC; ~40 µs small READ RTT; OpenTable direct ~1.3 GiB/s; std::fs ~3 GiB/s).

Working tree was clean and already pushed to `halgari/vfs` (`2d1fa0b` bench + prior FUSE work). This document is analysis only (no code changes required for the analysis itself).

---

## 1. Current data path (where time goes)

### 1.1 One `NtReadFile` of *N* bytes under the managed root (FUSE active)

```
Game buffer
    ▲
    │  copy #5  (hook → user buffer)
    │
fuse_client::read_fragmented
    │  loop of RPCs, each ≤ payload_cap−8 (~256 KiB)
    │
RingClient::submit
    │  claim slot → write req → wait COMPLETED → read resp Vec
    │  copy #3: ring → Vec (take_response)
    │  copy #4: decode_read_resp → another Vec (or slice of same)
    │
Shared ring slot payload
    ▲
    │  copy #2  (server_complete write_bytes)
    │
Server handler / OpenTable::read
    │  allocates Vec, opens file, seek, read
    │  copy #1  (disk/zip → Vec)
    │
Zip container or disk file
```

**Counted copies for bulk data (today):** typically **3–4 full data copies** plus protocol header traffic:

| # | Location | What |
|---|----------|------|
| 1 | `OpenTable::read` | `File::read` into a fresh `Vec` |
| 2 | `server_complete` | `Vec` → ring slot bytes |
| 3 | `take_response` | ring slot → new `Vec` |
| 4 | `decode_read_resp` + `read_fragmented` | into game buffer (often one more copy) |

Also per RPC:

- Slot claim/CAS, atomics, spin or event wake  
- Encode/decode small headers  
- **`File::open` + `seek` every READ** on disk and zip windows (no pooled handle)  
- **Mutex on entire open table** held during I/O  
- **Single server thread** (`serve_one` loop)  
- Shim **rebuilds `RingClient` on every call** (`with_client`)  

### 1.2 What the bench already proved

| Observation | Implication |
|-------------|-------------|
| HEARTBEAT p50 ~1 µs | Ring atomics + spin path is not the long-term bottleneck for bulk |
| GETATTR/OPEN ~4–8 µs | Metadata RPC is fine |
| Small READ ~40 µs p50 | Fixed cost dominates; 4 KiB transfers are latency-bound |
| Large READ ~0.5–0.7 ms / 256 KiB | ~300–500 MiB/s *per RPC* in the best single-shot case |
| Sequential RPC ~200–380 MiB/s | Fragmentation + copies + reopen/seek |
| OpenTable direct ~1.3 GiB/s | Director-local I/O has huge headroom if we stop shipping bytes through the ring |

**Bottleneck hierarchy (bulk):**

1. **Copies through the ring** (and Vec churn)  
2. **Per-READ `File::open` + seek** (especially zip)  
3. **Single-threaded server + global open-table lock during I/O**  
4. **256 KiB payload cap → many RPCs** for multi-MB BSAs  
5. Fixed per-RPC overhead (~40 µs) for tiny reads  

Metadata is already “good enough.” Optimize the **data plane**.

---

## 2. Clear wins (ordered by impact / effort)

### Tier A — High impact, relatively small code

#### A1. **Keep open OS handles (or map containers) in the open table**

Today every `OpenTable::read` does `File::open` + `seek` + `read`.

**Win:** Drop reopen cost; sequential zip/disk reads become closer to OpenTable direct (~GiB/s class for hot files).  
**How:** On OPEN, open `File` (or `std::fs::File` + shared `Mutex`) and store in `OpenEntry`; optional whole-zip `Mmap` cache keyed by container path (director-only, already allowed by design).  
**Risk:** Handle limits, share modes; easy to pool by container.  
**Expected:** Large sequential RPC throughput move toward **0.8–1.2 GiB/s** still *with* ring copies if I/O stops being the limiter—or at least remove the zip/disk reopen tax so remaining cost is pure IPC.

#### A2. **Do not hold the open-table mutex across I/O**

Today `read` locks the map for the whole `File::open`/`read`.

**Win:** Parallel READs on different `fh`s (and less lock contention with OPEN/CLOSE).  
**How:** Clone/copy source metadata under lock (or `Arc<OpenFileInner>`), drop lock, then I/O.  
**Risk:** Low if file lifetime is refcounted until CLOSE.

#### A3. **Avoid double-allocate on the client**

`take_response` → `Vec`, then `decode_read_resp` → another `Vec`, then copy into user buffer.

**Win:** One less full copy + less allocator traffic.  
**How:** `decode_read_resp_into(&payload, &mut buf)` or zero-copy slice after validating header; better: read ring payload straight into the game buffer when the caller provides it (`submit_read_into`).  
**Expected:** Noticeable for large sequential (tens of percent), not a 10×.

#### A4. **Reuse `RingClient` / avoid `with_client` rebuild**

`FuseClient::with_client` constructs a new `RingClient` every call.

**Win:** Micro-latency (small), cleaner hot path.  
**How:** Store `RingClient` once (self-ref structure or raw seg pointer + geom cached after connect).

#### A5. **Pipelined / multi-slot READs**

Today each fragment is fully serial: submit → wait → next.

**Win:** Hide I/O latency when server can process multiple slots; better multi-core later.  
**How:** Claim 2–4 slots, post several READs, wait all; or async completion.  
**Expected:** Helps most when server is multi-threaded (A6) and I/O is the wait.

---

### Tier B — Medium effort, structural IPC improvements

#### B1. **Shared bulk data arena (design already deferred this)**

IPC design mentioned a **bulk arena** separate from fixed slot payloads.

**Model:**

- Control ring: small messages only (`READ` request + status + `bytes_read` + **arena offset**).  
- Data: director writes file bytes into a **shared section** region the client already maps.  
- Client: `memcpy` from arena → game buffer **or** (if NtReadFile buffer is the arena temporarily) zero-copy into the game.

**Copies:** disk → arena (1), arena → user (1) — **or** disk → user if director can write the user buffer (see §3).  
**Win:** Removes ring payload size as throughput ceiling; allows multi-MiB READs per RPC.  
**Expected:** Sequential throughput much closer to OpenTable direct if arena is large and I/O is pooled.

#### B2. **Larger `payload_cap` (cheap knob, limited)**

256 KiB → 1–4 MiB slots.

**Win:** Fewer RPCs for big sequential.  
**Cost:** Larger shared mapping, worse cache behavior, still 3–4 copies.  
**Verdict:** Easy A/B test; do not treat as the real architecture.

#### B3. **Server worker pool**

Multiple `serve_one` threads (or one dispatcher + N I/O workers).

**Win:** Parallel BSA/ESM readers from the game.  
**Depends on:** A2 (no lock during I/O), careful ring slot ownership.  
**Risk:** Open-table races already handled if Arc-based.

#### B4. **Real event notifier (production path)**

Spin is fine when both sides are hot; games often block.

**Win:** CPU; correctness under load.  
**Latency:** idle wake +10–50 µs typical—not a bulk win, but needed for real play.

#### B5. **Read-ahead / readahead cache in director**

On sequential access patterns, director prefetches next windows into arena.

**Win:** Hides disk latency for BSA linear scans.  
**Risk:** Memory; wrong for random access.

---

### Tier C — Higher design complexity (your suggestion and relatives)

#### C1. **Director `WriteProcessMemory` into the game’s NtReadFile buffer**

**Idea:** Game issues READ with user buffer pointer `P`. Shim sends `READ(fh, off, len, target_va=P)` over the control ring (or a side channel). Director opens process handle, reads file, **`WriteProcessMemory(game, P, data, len)`**. Response is status + bytes_read only (tiny).

**Copies:** disk → director temp → WPM into game (still often **2** copies; WPM is not free).  
**If** director can use a shared section mapped in both processes at a fixed VA, it can be closer to 1 copy.

| Pros | Cons |
|------|------|
| Removes ring payload size limit | Needs process handle + `PROCESS_VM_WRITE` |
| One RPC per large transfer | WPM has kernel transitions; can be **slower** than shared-section memcpy for medium sizes |
| Fits “director is kernel” story | Security / stability if bad VA; ASLR; guard pages |
| No extra bulk mapping protocol | Cross-process write may fault; partial write handling |
| | Shim must pass **valid** user buffer VA and keep buffer live until COMPLETED |
| | Harder to test; more failure modes |

**When WPM wins:**

- Transfers **≫** a few hundred KiB where ring copy bandwidth dominates.  
- Especially if director can **readfile directly into a page-aligned staging buffer** and WPM large chunks.

**When WPM loses:**

- Small READs (4 KiB): fixed costs dominate; current ~40 µs already includes I/O.  
- High-frequency medium READs where **shared bulk arena + local memcpy** is faster and simpler than WPM.  
- Mitigations: only use WPM above a threshold (e.g. ≥64 KiB or ≥256 KiB).

**Hybrid recommendation:**  
`READ` flags: `INLINE` (today, small) vs `REMOTE_WRITE` (large, director WPM) vs `ARENA` (shared bulk). Client chooses by size.

#### C2. **Shared section mapped into the game (“OPEN maps a view”)**

Earlier design option (2): on OPEN of a large file, director creates a section (or maps zip window into a section), duplicates the mapping into the game process (`NtMapViewOfSection` cross-process), returns base+length. Subsequent “reads” are local.

| Pros | Cons |
|------|------|
| Best throughput for large files (local memcpy or even no copy if app accepts mapping) | Complex lifetime (CLOSE unmap) |
| No per-READ director I/O after map | Zip is 16 GB—cannot map whole archive per process without care |
| | SEC_IMAGE still special; this is for data files |
| | More like option (2) we deferred |

**Pragmatic variant:** map **only the open file’s window** (or a sliding window / arena) not the whole zip.

#### C3. **Handle duplication / MATERIALIZE**

Original IPC catalog had `MATERIALIZE`. Director opens a real temp file or section and dups a handle into the game—Windows then reads via kernel file object.

| Pros | Cons |
|------|------|
| Best compatibility for APIs that insist on real handles | Contradicts zero-extract if using temp files |
| Section handles can stay zero-extract | Still need careful design |

Prefer **section + map view** over temp extract.

---

## 3. Deep dive: “Director writes into subprocess buffers”

### 3.1 Cost model (order-of-magnitude)

Let *B* = transfer size, *C_ring* = cost of copy via ring (2–3× mem bandwidth + RPC), *C_wpm* = WPM + one local buffer fill.

From bench: sequential ring ≈ 200–380 MiB/s ⇒ effective bandwidth *R*.  
Local OpenTable ≈ 1.3 GiB/s.  
std::fs ≈ 3 GiB/s.

WPM bandwidth on modern Windows is often in the **hundreds of MiB/s to low GiB/s** depending on size and alignment—sometimes competitive with multi-copy ring, sometimes not.

**Rule of thumb for this project:**

| Size | Prefer |
|------|--------|
| &lt; 4–16 KiB | Inline ring (fixed ~40 µs; WPM not worth it) |
| 16 KiB–256 KiB | Inline or slightly larger payload; optimize A1–A3 first |
| &gt; 256 KiB–few MiB | **Bulk arena** or **WPM** (measure both) |
| Multi-MiB sequential | **Arena map** or **file-window map** (C2) beats both pure ring and many small WPMs |

### 3.2 Safety contract if you implement WPM

1. Shim captures `buffer` VA + length from `NtReadFile` **before** RPC.  
2. Request includes `target_pid` (implicit: game), `target_va`, `len`, `fh`, `offset`.  
3. Director validates: `fh` open, `offset+len` in range, `len` ≤ max (e.g. 16 MiB), VA not null.  
4. Optional: probe `VirtualQueryEx` that region is committed writable.  
5. Read into director buffer (or scatter); `WriteProcessMemory`; return status.  
6. On failure: do **not** leave partial state without reporting short write.  
7. Never use WPM for executable pages as a substitute for SEC_IMAGE without separate design.

### 3.3 Better cousin: dual-mapped bulk arena (usually preferred)

```
┌──────────── director ────────────┐     ┌──────── game ────────┐
│  File/zip → write into arena[i]  │     │  arena mapped RO/RW  │
│  ring: READ_OK fh off len idx    │────▶│  memcpy(user, arena)│
└──────────────────────────────────┘     └──────────────────────┘
```

- One shared section created by director, opened by shim at bootstrap (already have named section machinery).  
- Control ring stays small (recursion-safe).  
- Director never needs VM_WRITE into arbitrary game heaps.  
- Client still one local copy unless the game can read from arena directly (rare for NtReadFile).

**Compared to WPM:** simpler failure modes, easier testing, same process model as today’s ring, no per-buffer VA validation. **Compared to pure ring:** cuts copies and lifts size cap.

### 3.4 Recommendation

| Priority | Approach |
|----------|----------|
| 1 | **A1 + A2 + A3** immediately (handles, no lock across I/O, fewer Vec copies) |
| 2 | **Bulk arena (B1)** for large READs — best balance of perf and complexity |
| 3 | **Thresholded WPM (C1)** only if arena cannot meet goals or for one-shot giant reads after measuring |
| 4 | **Per-file mapped window (C2)** for multi-GB BSA access patterns if still I/O bound |

Do **not** replace small READs with WPM. Keep pure ring for metadata + small data.

---

## 4. Other wins (non-IPC but related)

| Item | Note |
|------|------|
| **GETATTR cache in shim** | Path → (size, is_dir) TTL 0–100 ms for hot metadata storms |
| **READDIR cache** | Same for Data/ listing |
| **Adaptive chunk size** | Start 64 KiB, ramp to payload_cap on sequential detection |
| **Director container mmap cache** | One map per zip; READ is memcpy from map (huge win for zip windows) |
| **io_uring / overlapped I/O** | Later; Windows thread pool reads |
| **Event notifier** | Correctness + CPU; measure before/after |

---

## 5. Suggested implementation sequence

1. **Instrument:** per-opcode histograms in director (optional env `VFS_RPC_STATS`) so game runs show real mix (4 KiB vs 256 KiB).  
2. **A1** open-handle / zip mmap cache + **A2** lock scope. Re-run `vfs-fuse-bench`.  
3. **A3** client zero-copy into user buffer for inline READ.  
4. **B1** bulk arena + `READ` flag `F_BULK`; shim uses bulk when `len ≥ threshold`.  
5. Re-bench vs WPM prototype for 1–8 MiB single transfers.  
6. Only then consider production WPM or file-window maps.

---

## 6. Success metrics

| Metric | Today (order of magnitude) | Target after Tier A+B |
|--------|----------------------------|------------------------|
| HEARTBEAT / GETATTR p50 | 1–5 µs | Maintain |
| READ 4 KiB p50 | ~45 µs | ≤40 µs (small win) |
| Sequential RPC / bulk | 200–380 MiB/s | **≥1 GiB/s** for large sequential (arena or pooled I/O) |
| OpenTable direct | ~1.3 GiB/s | Ceiling for non-map designs |
| CPU on spin wait | High when busy | Event wait when idle |

---

## 7. Summary

The pure-RPC design is **correct and simple**; the bench shows **metadata is cheap** and **bulk pays for multiple copies + reopen/seek + 256 KiB framing**.

**Clearest wins:** keep files/maps open in the director, release locks during I/O, stop reallocating response buffers, then add a **shared bulk arena** for large transfers.  

**Director WPM into the game buffer** is a valid option for large READs but is **not strictly better** than a dual-mapped arena: WPM avoids ring payload limits yet adds process-write complexity and may not beat a well-designed shared section. Prefer arena first; use WPM as a measured alternative above a size threshold.

---

## 8. Related files

| File | Role |
|------|------|
| `crates/vfs-ipc/src/endpoint.rs` | submit / serve_one |
| `crates/vfs-ipc/src/ring.rs` | slot payload copy |
| `crates/vfs-server/src/open_table.rs` | open/read I/O |
| `crates/vfs-shim/src/fuse_client.rs` | fragmentation + copies |
| `crates/vfs-shim/src/hook.rs` | NtReadFile → fuse |
| `docs/benchmarks/fuse-rpc-performance.md` | measured numbers |
| `docs/superpowers/specs/2026-07-15-director-fuse-thin-shim-design.md` | pure RPC locked for phase 1 |

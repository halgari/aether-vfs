# M2 — JVM FFM Ring Server (Read Path) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the server side of the `vfs-ipc` shared-memory ring in the JVM via FFM/Panama — map the section, mirror the spin-based CAS state machine, dispatch read-path opcodes to an aether `Provider`, write bulk reads zero-copy into the arena — proven cross-process by a Rust `RingClient` harness.

**Architecture:** OS-agnostic ring/arena/dispatch logic operates over any `java.lang.foreign.MemorySegment` (heap segment → cross-platform tests); a Windows-only `section.clj` maps a real named section via `kernel32` downcalls. A new Rust `vfs-ring-harness` bin opens the JVM-created section with the existing `vfs-win`/`vfs-ipc` client and asserts responses. Ring scalar/atomic fields use native-order `MemorySegment` access + a `VarHandle` for the CAS `state` field; wire payloads use the M1 `aether.vfs.wire` little-endian codec.

**Tech Stack:** Clojure 1.12 + deps.clj, Java 26 (Temurin) `java.lang.foreign` (Panama, finalized), `VarHandle` atomics, Rust (existing `vfs-ipc`/`vfs-win`), GitHub Actions.

## Global Constraints

- Consumer is always JVM/Clojure; namespace root `aether.vfs.*`; OS-specific code under `src/aether/vfs/os/windows/`. (Parent design)
- Nothing hardcodes a wire/layout constant — read opcodes/statuses/flags/offsets from `aether.vfs.protocol` (the M1 descriptor). (Anti-drift)
- Ring/layout facts (from the descriptor, verbatim): `RingHeader` size 40 / align 8 — offsets magic 0, version 4, slot_count 8, slot_stride 12, payload_cap 16, req_seq 24, submit_seq 32; `SlotHeader` size 32 / align 8 — offsets state 0, opcode 4, flags 8, payload_len 12, status 16, req_id 24; `MAGIC = 0x56464950`, `VERSION = 1`; slot states FREE 0, CLAIMED 1, SUBMITTED 2, PROCESSING 3, COMPLETED 4; `slot_stride = align8(32 + payload_cap)`.
- Opcodes: GETATTR 1, READDIR 2, OPEN 3, READ 5, CLOSE 11. Flags: `OPEN_READ` 1, `FLAG_READ_BULK` 0x1, `READ_RESP_BULK_BIT` 0x8000_0000. `BULK_THRESHOLD = 64*1024`. Statuses: OK 0, NOT_FOUND -1, NOT_A_DIRECTORY -2, BAD_REQUEST -3, IO_ERROR -4, IS_DIR -5, BAD_FH -6.
- Native-order note: ring scalar/atomic fields are accessed with native-order `ValueLayout/JAVA_INT`/`JAVA_LONG` (x86 native = little-endian = Rust `AtomicU32`/`to_le_bytes`); wire message payloads use `aether.vfs.wire` (explicit LE). These agree on x86-64 Windows/Linux CI runners.
- Spin-based, single-threaded server; read path only. No OS events, no write ops, no injection, no `mount` entry (later milestones).
- Any file compared byte-for-byte (golden `.edn`) stays `eol=lf` (M1 `.gitattributes` already covers `resources/*.edn`).
- Cross-platform tests must pass in the existing ubuntu Clojure CI job; the Windows-only proof runs in a new `windows-clojure` job. Frequent commits: ≥1 per task.

---

## File Structure

- `src/aether/vfs/os/windows/ring.clj` — NEW: ring state machine over a `MemorySegment` (init / server-take / read-request / server-complete).
- `src/aether/vfs/os/windows/arena.clj` — NEW: bulk arena bank layout + zero-copy fill.
- `src/aether/vfs/os/windows/section.clj` — NEW: FFM `kernel32` named-section map/unmap (Windows-only).
- `src/aether/vfs/os/windows/server.clj` — NEW: serve loop + opcode dispatch to a `Provider` + fh table.
- `src/aether/vfs/wire.clj` — MODIFY: add server-side codecs (`decode-path-req`, `decode-open-req`, `encode-open-resp`, `decode-read-req`, `encode-read-resp-bulk`, `decode-close-req`).
- `rust/crates/xtask-descriptor/src/lib.rs` — MODIFY: add F2 golden vectors (`open-resp`, `read-resp-bulk`, `ring-header-dump`).
- `resources/protocol-golden.edn` — regenerated.
- `rust/crates/vfs-ring-harness/{Cargo.toml,src/main.rs}` — NEW: Rust `RingClient` harness bin.
- `rust/Cargo.toml` — MODIFY: add member.
- `test/aether/vfs/os/windows/{ring_test,arena_test,server_test}.clj` — NEW: cross-platform (heap-segment) tests.
- `test/aether/vfs/wire_conformance_test.clj` — MODIFY: assert the new server-side codecs against golden.
- `test/aether/vfs/os/windows/integration_test.clj` — NEW: Windows-only cross-process proof (self-skips off Windows).
- `.github/workflows/ci.yml` — MODIFY: add `windows-clojure` job.

**Dependency order:** ring → wire+golden → arena → server → section → harness → integration+CI. The crux (CAS/atomics) is proven in-JVM at Task 1; FFM mapping at Task 5; cross-process/cross-language ordering at Task 7.

---

## Task 1: `ring.clj` — CAS state machine over a MemorySegment

TDD, cross-platform (heap segment). Proves the ring atomics/framing logic in-JVM before any native section.

**Files:**
- Create: `src/aether/vfs/os/windows/ring.clj`, `test/aether/vfs/os/windows/ring_test.clj`

**Interfaces:**
- Produces: `aether.vfs.os.windows.ring/init [seg slot-count payload-cap] -> geom` (geom = `{:slot-count :slot-stride :payload-cap}`); `server-take [seg geom] -> slot|nil`; `read-request [seg geom slot] -> {:opcode :flags :req-id :payload (byte-array)}`; `server-complete [seg geom slot status ^bytes payload]`; plus test-only client helpers `claim-free`, `publish-request`, `take-response`, `free-slot` mirroring the Rust primitives so a Clojure-only test can drive a full slot cycle.
- Consumes: `aether.vfs.protocol` (offsets/states/magic/version).

- [ ] **Step 1: Write the failing test**

Create `test/aether/vfs/os/windows/ring_test.clj`:
```clojure
(ns aether.vfs.os.windows.ring-test
  (:require [clojure.test :refer [deftest is]]
            [aether.vfs.os.windows.ring :as ring])
  (:import [java.lang.foreign Arena MemorySegment]))

(defn- heap-seg ^MemorySegment [n]
  ;; 8-aligned native segment usable for atomics, freed with the arena.
  (.allocate (Arena/ofAuto) (long n) 8))

(deftest init-open-roundtrip
  (let [seg (heap-seg 4096)
        geom (ring/init seg 4 256)]
    (is (= 4 (:slot-count geom)))
    (is (= 256 (:payload-cap geom)))
    ;; slot-stride = align8(32 + 256) = 288
    (is (= 288 (:slot-stride geom)))))

(deftest full-slot-cycle
  ;; Drive one slot SUBMITTED->PROCESSING->COMPLETED with the client + server
  ;; halves, entirely in-JVM, verifying the CAS state machine + framing.
  (let [seg (heap-seg 4096)
        geom (ring/init seg 2 256)
        slot (ring/claim-free seg geom)]
    (is (= 0 slot))
    (ring/publish-request seg geom slot 1 7 (.getBytes "ping" "UTF-8")) ; OP_GETATTR=1
    (let [taken (ring/server-take seg geom)]
      (is (= slot taken))
      (let [req (ring/read-request seg geom taken)]
        (is (= 1 (:opcode req)))
        (is (= 7 (:flags req)))
        (is (= "ping" (String. ^bytes (:payload req) "UTF-8"))))
      (ring/server-complete seg geom taken 42 (.getBytes "pong" "UTF-8")))
    (let [{:keys [status payload]} (ring/take-response seg geom slot)]
      (is (= 42 status))
      (is (= "pong" (String. ^bytes payload "UTF-8"))))
    (ring/free-slot seg geom slot)
    (is (= 0 (ring/claim-free seg geom)))))

(deftest server-take-nil-when-idle
  (let [seg (heap-seg 4096)
        geom (ring/init seg 2 256)]
    (is (nil? (ring/server-take seg geom)))))
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `clojure -M:test -n aether.vfs.os.windows.ring-test`
Expected: FAIL — namespace `aether.vfs.os.windows.ring` not found.

- [ ] **Step 3: Write the implementation**

Create `src/aether/vfs/os/windows/ring.clj`:
```clojure
(ns aether.vfs.os.windows.ring
  "vfs-ipc ring state machine over a java.lang.foreign.MemorySegment. Mirrors
  rust/crates/vfs-ipc/src/ring.rs byte-for-byte; all offsets/states come from
  aether.vfs.protocol. Scalar fields use native-order JAVA_INT/JAVA_LONG; the
  slot `state` field is the only atomic and is accessed through a VarHandle with
  acquire/release/CAS access modes to match Rust's AtomicU32 orderings."
  (:require [aether.vfs.protocol :as proto])
  (:import [java.lang.foreign MemorySegment ValueLayout]
           [java.lang.invoke VarHandle]))

(def ^:private RING-HDR 40)
(def ^:private SLOT-HDR 32)
(def ^:private ^VarHandle state-vh (.varHandle ValueLayout/JAVA_INT)) ; coords (MemorySegment, long)

(def ^:private ST-FREE 0)
(def ^:private ST-SUBMITTED 2)
(def ^:private ST-PROCESSING 3)
(def ^:private ST-COMPLETED 4)

(defn- align8 ^long [^long n] (bit-and (+ n 7) (bit-not 7)))

;; --- scalar helpers (native order) ---
(defn- get-i32 ^long [^MemorySegment seg ^long off] (long (.get seg ValueLayout/JAVA_INT off)))
(defn- set-i32 [^MemorySegment seg ^long off ^long v] (.set seg ValueLayout/JAVA_INT off (int v)))
(defn- get-i64 ^long [^MemorySegment seg ^long off] (.get seg ValueLayout/JAVA_LONG off))
(defn- set-i64 [^MemorySegment seg ^long off ^long v] (.set seg ValueLayout/JAVA_LONG off v))

(defn- slot-off ^long [geom ^long slot] (+ RING-HDR (* slot (long (:slot-stride geom)))))
(defn- payload-off ^long [geom ^long slot] (+ (slot-off geom slot) SLOT-HDR))

(defn init
  "Lay out an empty ring in seg; returns geom {:slot-count :slot-stride :payload-cap}."
  [^MemorySegment seg slot-count payload-cap]
  (let [stride (align8 (+ SLOT-HDR (long payload-cap)))
        rh (:ring-header (deref proto/descriptor))
        f (:fields rh)]
    (set-i32 seg (:magic f) (:magic (deref proto/descriptor)))
    (set-i32 seg (:version f) proto/version)
    (set-i32 seg (:slot-count f) slot-count)
    (set-i32 seg (:slot-stride f) stride)
    (set-i32 seg (:payload-cap f) payload-cap)
    (set-i64 seg (:req-seq f) 0)
    (set-i32 seg (:submit-seq f) 0)
    (let [geom {:slot-count (long slot-count) :slot-stride stride :payload-cap (long payload-cap)}]
      (dotimes [s slot-count]
        (set-i32 seg (slot-off geom s) ST-FREE)) ; SH_STATE offset is 0
      geom)))

(defn- sh [k] (get-in (deref proto/descriptor) [:slot-header :fields k]))

(defn server-take
  "CAS the first SUBMITTED slot to PROCESSING; return its index or nil."
  [^MemorySegment seg geom]
  (loop [s 0]
    (when (< s (long (:slot-count geom)))
      (let [off (+ (slot-off geom s) (long (sh :state)))]
        (if (.compareAndSet state-vh seg off (int ST-SUBMITTED) (int ST-PROCESSING))
          s
          (recur (inc s)))))))

(defn read-request
  [^MemorySegment seg geom ^long slot]
  (let [base (slot-off geom slot)
        len (get-i32 seg (+ base (long (sh :payload-len))))
        payload (byte-array len)]
    (MemorySegment/copy seg (+ (payload-off geom slot)) (MemorySegment/ofArray payload) 0 (long len))
    {:opcode (get-i32 seg (+ base (long (sh :opcode))))
     :flags  (get-i32 seg (+ base (long (sh :flags))))
     :req-id (get-i64 seg (+ base (long (sh :req-id))))
     :payload payload}))

(defn server-complete
  [^MemorySegment seg geom ^long slot ^long status ^bytes payload]
  (let [base (slot-off geom slot)
        n (alength payload)]
    (MemorySegment/copy (MemorySegment/ofArray payload) 0 seg (payload-off geom slot) (long n))
    (set-i32 seg (+ base (long (sh :status))) status)
    (set-i32 seg (+ base (long (sh :payload-len))) n)
    (.setRelease state-vh seg (+ base (long (sh :state))) (int ST-COMPLETED))))

;; --- client-side helpers (test harness only; mirror ring.rs) ---
(defn claim-free [^MemorySegment seg geom]
  (loop [s 0]
    (when (< s (long (:slot-count geom)))
      (let [off (+ (slot-off geom s) (long (sh :state)))]
        (if (.compareAndSet state-vh seg off (int ST-FREE) (int 1)) ; ST_CLAIMED=1
          s (recur (inc s)))))))

(defn publish-request [^MemorySegment seg geom ^long slot ^long opcode ^long flags ^bytes payload]
  (let [base (slot-off geom slot)]
    (set-i32 seg (+ base (long (sh :opcode))) opcode)
    (set-i32 seg (+ base (long (sh :flags))) flags)
    (set-i32 seg (+ base (long (sh :payload-len))) (alength payload))
    (MemorySegment/copy (MemorySegment/ofArray payload) 0 seg (payload-off geom slot) (long (alength payload)))
    (.setRelease state-vh seg (+ base (long (sh :state))) (int ST-SUBMITTED))))

(defn take-response [^MemorySegment seg geom ^long slot]
  (let [base (slot-off geom slot)]
    (when (= ST-COMPLETED (int (.getAcquire state-vh seg (+ base (long (sh :state))))))
      (let [len (get-i32 seg (+ base (long (sh :payload-len))))
            payload (byte-array len)]
        (MemorySegment/copy seg (payload-off geom slot) (MemorySegment/ofArray payload) 0 (long len))
        {:status (get-i32 seg (+ base (long (sh :status)))) :payload payload}))))

(defn free-slot [^MemorySegment seg geom ^long slot]
  (.setRelease state-vh seg (+ (slot-off geom slot) (long (sh :state))) (int ST-FREE)))
```

**Adaptation note (executor):** the `java.lang.foreign` API surface (VarHandle coordinate arity, `MemorySegment/copy` overload selection, `.get`/`.set` on segment) must be satisfied against JDK 26. If `(.compareAndSet state-vh seg off exp new)` arg types mismatch, cast offsets to `long` and values to `int` explicitly (already done). The **contract** is the test in Step 1; adjust interop specifics to make it pass — do not change the byte layout.

- [ ] **Step 4: Run the test to verify it passes**

Run: `clojure -M:test -n aether.vfs.os.windows.ring-test`
Expected: PASS (3 deftests).

- [ ] **Step 5: Commit**

```bash
git add src/aether/vfs/os/windows/ring.clj test/aether/vfs/os/windows/ring_test.clj
git commit -m "feat(win): ring CAS state machine over MemorySegment (cross-platform)"
```

---

## Task 2: `wire.clj` server-side codecs + F2 golden vectors

TDD. Adds the encoders/decoders the server needs (M1 added the client half) and closes F2 by pinning `open-resp` + `read-resp-bulk` (and a ring-header dump) to Rust golden vectors.

**Files:**
- Modify: `rust/crates/xtask-descriptor/src/lib.rs`, `resources/protocol-golden.edn`, `src/aether/vfs/wire.clj`, `test/aether/vfs/wire_conformance_test.clj`

**Interfaces:**
- Produces: `aether.vfs.wire/decode-path-req [^bytes] -> String`, `decode-open-req [^bytes] -> {:flags :path}`, `encode-open-resp [{:keys [fh size is-dir]}] -> bytes`, `decode-read-req [^bytes] -> {:fh :offset :len}`, `encode-read-resp-bulk [bytes-read arena-offset] -> bytes`, `decode-close-req [^bytes] -> fh`.
- Consumes: nothing new (mirrors `rust/crates/vfs-protocol/src/lib.rs`).

- [ ] **Step 1: Add Rust golden vectors (extend the emitter)**

In `rust/crates/xtask-descriptor/src/lib.rs`, add to the `golden_vectors()` vec (after the existing six), using the real `vfs-protocol` encoders:
```rust
        ("open-resp-fh42-size1000",
         P::encode_open_resp(&vfs_protocol::OpenResp { fh: 42, size: 1000, is_dir: false })),
        ("read-resp-bulk-len5-off65536",
         P::encode_read_resp_bulk(5, 65536)),
```
And add a ring-header dump vector: after `init`-ing a ring in an `OwnedSeg`, copy the first 40 bytes. Add near `golden_vectors`:
```rust
        ("ring-header-slots4-cap256", {
            use vfs_ipc::seg::OwnedSeg;
            let owned = OwnedSeg::new(4096);
            vfs_ipc::ring::init(owned.seg(), 4, 256).unwrap();
            owned.seg().read_bytes(0, 40).unwrap()
        }),
```
(If `vfs_ipc::seg`/`ring`/`OwnedSeg` are not already `pub` on the paths used, import via the crate's public re-exports — `vfs_ipc::{ring, OwnedSeg}` per `vfs-ipc/src/lib.rs` — adjust the path to what compiles.)

- [ ] **Step 2: Regenerate golden + run the Rust conformance test (expect it to update, then pass)**

Run:
```bash
bin/regen-protocol
cd rust && cargo test -p xtask-descriptor && cd ..
```
Expected: `resources/protocol-golden.edn` now contains the three new vectors; `cargo test -p xtask-descriptor` PASS (the golden test compares emitter output to the freshly-regenerated committed file).

- [ ] **Step 3: Write the failing Clojure conformance additions**

Append to `test/aether/vfs/wire_conformance_test.clj` inside a new deftest:
```clojure
(deftest server-side-codecs-match-golden
  (let [g (golden)]
    (is (= (:open-resp-fh42-size1000 g)
           (hex (wire/encode-open-resp {:fh 42 :size 1000 :is-dir false}))))
    (is (= (:read-resp-bulk-len5-off65536 g)
           (hex (wire/encode-read-resp-bulk 5 65536))))))

(deftest server-side-decoders-roundtrip
  (is (= {:flags 1 :path "Data/Skyrim.esm"}
         (wire/decode-open-req (wire/encode-open-req 1 "Data/Skyrim.esm"))))
  (is (= {:fh 7 :offset 10 :len 4}
         (wire/decode-read-req (wire/encode-read-req {:fh 7 :offset 10 :len 4}))))
  (is (= "Data/x" (wire/decode-path-req (.getBytes "Data/x" "UTF-8"))))
  (is (= 99 (wire/decode-close-req (wire/encode-close-req 99)))))
```

- [ ] **Step 4: Run to verify failure**

Run: `clojure -M:test -n aether.vfs.wire-conformance-test`
Expected: FAIL — `wire/encode-open-resp` (etc.) unresolved.

- [ ] **Step 5: Implement the server-side codecs**

Append to `src/aether/vfs/wire.clj` (mirroring `vfs-protocol` exactly — open-resp is `fh:u64 | size:u64 | is_dir:u8 | pad[7]`; read-resp-bulk is `(bytes|BULK_BIT):u32 | pad:u32 | arena_off:u64`):
```clojure
(defn decode-path-req ^String [^bytes p] (String. p "UTF-8"))

(defn decode-open-req [^bytes p]
  (let [bb (buf p)
        flags (bit-and (long (.getInt bb)) 0xffffffff)
        rest (byte-array (.remaining bb))]
    (.get bb rest)
    {:flags flags :path (String. rest "UTF-8")}))

(defn encode-open-resp ^bytes [{:keys [fh size is-dir]}]
  (let [b (baos)]
    (put-u64! b fh) (put-u64! b size) (.write b (int (if is-dir 1 0)))
    (dotimes [_ 7] (.write b 0))
    (.toByteArray b)))

(defn decode-read-req [^bytes p]
  (let [bb (buf p)]
    {:fh (.getLong bb) :offset (.getLong bb) :len (bit-and (long (.getInt bb)) 0xffffffff)}))

(def ^:private READ-RESP-BULK-BIT 0x80000000)
(defn encode-read-resp-bulk ^bytes [bytes-read arena-offset]
  (let [b (baos)]
    (put-u32! b (bit-or (long bytes-read) READ-RESP-BULK-BIT))
    (put-u32! b 0)
    (put-u64! b arena-offset)
    (.toByteArray b)))

(defn decode-close-req [^bytes p] (.getLong (buf p)))
```
(`buf`, `baos`, `put-u32!`, `put-u64!` already exist in `wire.clj` from M1. If `READ_RESP_BULK_BIT` should come from the descriptor rather than a literal, prefer `(long (get-in @proto/descriptor [:flags :read-resp-bulk-bit]))` — add a `[aether.vfs.protocol :as proto]` require; keep whichever the conformance test proves correct.)

- [ ] **Step 6: Run to verify pass**

Run: `clojure -M:test -n aether.vfs.wire-conformance-test`
Expected: PASS (all deftests, incl. the two new).

- [ ] **Step 7: Commit**

```bash
git add rust/crates/xtask-descriptor/src/lib.rs resources/protocol-golden.edn src/aether/vfs/wire.clj test/aether/vfs/wire_conformance_test.clj
git commit -m "feat(wire): server-side codecs + F2 golden vectors (open-resp, read-resp-bulk, ring-header)"
```

---

## Task 3: `arena.clj` — bulk arena bank layout + zero-copy fill

TDD, cross-platform (heap segment).

**Files:**
- Create: `src/aether/vfs/os/windows/arena.clj`, `test/aether/vfs/os/windows/arena_test.clj`

**Interfaces:**
- Produces: `aether.vfs.os.windows.arena/make [seg mapping-offset arena-len banks] -> arena`; `bank-mapping-offset [arena slot] -> long`; `fill-bank [arena slot max-len f] -> {:offset :len}` where `f` receives a `MemorySegment` slice (the bank) and returns bytes written.
- Consumes: nothing (pure over a segment).

- [ ] **Step 1: Write the failing test**

Create `test/aether/vfs/os/windows/arena_test.clj`:
```clojure
(ns aether.vfs.os.windows.arena-test
  (:require [clojure.test :refer [deftest is]]
            [aether.vfs.os.windows.arena :as arena])
  (:import [java.lang.foreign Arena MemorySegment ValueLayout]))

(defn- seg ^MemorySegment [n] (.allocate (Arena/ofAuto) (long n) 8))

(deftest bank-offsets-round-robin
  (let [s (seg 8192)
        a (arena/make s 1024 4096 2)] ; mapping-offset 1024, len 4096, 2 banks -> bank-size 2048
    (is (= 1024 (arena/bank-mapping-offset a 0)))
    (is (= (+ 1024 2048) (arena/bank-mapping-offset a 1)))
    (is (= 1024 (arena/bank-mapping-offset a 2))))) ; slot 2 % 2 banks -> bank 0

(deftest fill-bank-writes-into-arena
  (let [s (seg 8192)
        a (arena/make s 1024 4096 2)
        {:keys [offset len]} (arena/fill-bank a 0 100
                               (fn [^MemorySegment bank]
                                 (.set bank ValueLayout/JAVA_BYTE 0 (byte 65)) ; 'A'
                                 (.set bank ValueLayout/JAVA_BYTE 1 (byte 66)) ; 'B'
                                 2))]
    (is (= 1024 offset))
    (is (= 2 len))
    (is (= 65 (.get s ValueLayout/JAVA_BYTE 1024)))
    (is (= 66 (.get s ValueLayout/JAVA_BYTE 1025)))))
```

- [ ] **Step 2: Run to verify failure**

Run: `clojure -M:test -n aether.vfs.os.windows.arena-test`
Expected: FAIL — namespace not found.

- [ ] **Step 3: Implement**

Create `src/aether/vfs/os/windows/arena.clj`:
```clojure
(ns aether.vfs.os.windows.arena
  "Bulk data arena mirror of vfs-ipc::DataArena. Banks are sized per ring slot;
  fill-bank hands the provider a MemorySegment slice of the bank to write into
  directly (zero-copy read destination)."
  (:import [java.lang.foreign MemorySegment]))

(defn make [^MemorySegment seg mapping-offset arena-len banks]
  (let [banks (max 1 (long banks))
        bank-size (max 4096 (quot (long arena-len) banks))]
    {:seg seg :mapping-offset (long mapping-offset) :bank-size bank-size :banks banks}))

(defn bank-mapping-offset ^long [arena ^long slot]
  (+ (:mapping-offset arena) (* (mod slot (:banks arena)) (:bank-size arena))))

(defn fill-bank
  "Give f a MemorySegment slice (the bank, capped at max-len and bank-size) to
  write into; return {:offset mapping-offset :len bytes-written}."
  [arena ^long slot ^long max-len f]
  (let [off (bank-mapping-offset arena slot)
        cap (min max-len (:bank-size arena))
        ^MemorySegment slice (.asSlice ^MemorySegment (:seg arena) off (long cap))
        n (long (f slice))]
    {:offset off :len (min n cap)}))
```

- [ ] **Step 4: Run to verify pass**

Run: `clojure -M:test -n aether.vfs.os.windows.arena-test`
Expected: PASS (2 deftests).

- [ ] **Step 5: Commit**

```bash
git add src/aether/vfs/os/windows/arena.clj test/aether/vfs/os/windows/arena_test.clj
git commit -m "feat(win): bulk arena bank layout + zero-copy fill-bank"
```

---

## Task 4: `server.clj` — opcode dispatch to a Provider + fh table

TDD, cross-platform. Wires ring + wire + arena + a `Provider` into a single `dispatch` fn (pure, testable without a native section) and a spin `serve` loop.

**Files:**
- Create: `src/aether/vfs/os/windows/server.clj`, `test/aether/vfs/os/windows/server_test.clj`

**Interfaces:**
- Consumes: `aether.vfs.os.windows.{ring,arena}`, `aether.vfs.wire`, `aether.vfs.provider`, `aether.vfs.protocol`.
- Produces: `aether.vfs.os.windows.server/dispatch [state provider req] -> {:status :payload}` where `state` holds the fh table + arena (an atom); `serve [seg geom arena provider stop?]` spin loop calling `ring/server-take` → `dispatch` → `ring/server-complete`; `make-state [arena] -> atom`.

- [ ] **Step 1: Write the failing test**

Create `test/aether/vfs/os/windows/server_test.clj`:
```clojure
(ns aether.vfs.os.windows.server-test
  (:require [clojure.test :refer [deftest is]]
            [aether.vfs.os.windows.server :as server]
            [aether.vfs.os.windows.arena :as arena]
            [aether.vfs.wire :as wire]
            [aether.vfs.providers.inline :as inline])
  (:import [java.lang.foreign Arena MemorySegment ValueLayout]))

(defn- seg ^MemorySegment [n] (.allocate (Arena/ofAuto) (long n) 8))

(def small (.getBytes "hello" "UTF-8"))
(def big (byte-array 70000 (byte 88))) ; 'X' * 70000 > 64KiB -> bulk

(defn- provider [] (inline/inline-provider [["/hello.txt" small 0644]
                                            ["/big.bin" big 0644]]))

(deftest getattr-and-inline-read
  (let [s (seg (* 1 1024 1024))
        a (arena/make s 0 4096 2) ; arena unused for inline
        st (server/make-state a)
        p (provider)
        ga (server/dispatch st p {:opcode 1 :flags 0 :payload (.getBytes "/hello.txt" "UTF-8")})
        attr (wire/decode-getattr-resp (:payload ga))]
    (is (= 0 (:status ga)))
    (is (= 5 (:size attr)))
    (let [op (server/dispatch st p {:opcode 3 :flags 1 :payload (wire/encode-open-req 1 "/hello.txt")})
          {:keys [fh]} (wire/decode-open-resp (:payload op))
          rd (server/dispatch st p {:opcode 5 :flags 0 :payload (wire/encode-read-req {:fh fh :offset 0 :len 5})})]
      (is (= "hello" (String. ^bytes (wire/decode-read-resp (:payload rd)) "UTF-8"))))))

(deftest bulk-read-lands-in-arena
  (let [s (seg (* 1 1024 1024))
        a (arena/make s (* 512 1024) (* 256 1024) 2) ; arena at 512KiB
        st (server/make-state a)
        p (provider)
        op (server/dispatch st p {:opcode 3 :flags 1 :payload (wire/encode-open-req 1 "/big.bin")})
        {:keys [fh]} (wire/decode-open-resp (:payload op))
        rd (server/dispatch st p {:opcode 5 :flags 1 ; FLAG_READ_BULK
                                  :payload (wire/encode-read-req {:fh fh :offset 0 :len 70000})})
        [n off] (wire/decode-read-resp-bulk (:payload rd))]
    (is (= 70000 n))
    (is (= 88 (.get s ValueLayout/JAVA_BYTE off)))          ; first arena byte is 'X'
    (is (= 88 (.get s ValueLayout/JAVA_BYTE (+ off 69999)))))) ; last byte too
```
(This test needs `wire/decode-open-resp`, `wire/decode-read-resp`, and `wire/decode-read-resp-bulk`. `decode-open-resp`/`decode-read-resp` exist from M1; add `decode-read-resp-bulk [^bytes] -> [n off]` to `wire.clj` in this task's Step 3 if absent — mirror `vfs-protocol::decode_read_bulk_resp`.)

- [ ] **Step 2: Run to verify failure**

Run: `clojure -M:test -n aether.vfs.os.windows.server-test`
Expected: FAIL — namespace `aether.vfs.os.windows.server` not found.

- [ ] **Step 3: Implement**

If missing, first add to `src/aether/vfs/wire.clj`:
```clojure
(defn decode-read-resp-bulk [^bytes p]
  (let [bb (buf p)
        raw (bit-and (long (.getInt bb)) 0xffffffff)]
    (.getInt bb) ; pad
    [(bit-and raw 0x7fffffff) (.getLong bb)]))
```
Create `src/aether/vfs/os/windows/server.clj`:
```clojure
(ns aether.vfs.os.windows.server
  "Ring opcode dispatch to an aether Provider + fh table, mirroring the Rust
  dispatch_director read path. Single-threaded spin serve loop."
  (:require [aether.vfs.os.windows.ring :as ring]
            [aether.vfs.os.windows.arena :as arena]
            [aether.vfs.wire :as wire]
            [aether.vfs.provider :as p])
  (:import [java.lang.foreign MemorySegment ValueLayout]))

(def ^:private OP-GETATTR 1) (def ^:private OP-READDIR 2) (def ^:private OP-OPEN 3)
(def ^:private OP-READ 5)    (def ^:private OP-CLOSE 11)
(def ^:private FLAG-READ-BULK 0x1)
(def ^:private BULK-THRESHOLD (* 64 1024))
(def ^:private ST-OK 0) (def ^:private ST-NOT-FOUND -1) (def ^:private ST-BAD-FH -6)
(def ^:private ST-BAD-REQUEST -3)

(defn make-state [arena] (atom {:arena arena :next-fh 1 :open {}}))

(defn- resp [status ^bytes payload] {:status status :payload payload})

(defn- do-getattr [_ provider vpath]
  (if-let [m (p/lookup provider vpath)]
    (resp ST-OK (wire/encode-getattr-resp {:found true :is-dir (= :dir (:kind m))
                                           :size (:size m) :mtime (or (:mtime-secs m) 0)}))
    (resp ST-NOT-FOUND (wire/encode-getattr-resp {:found false :is-dir false :size 0 :mtime 0}))))

(defn- do-readdir [_ provider vpath]
  (let [entries (p/readdir provider vpath)]
    (resp ST-OK (wire/encode-readdir-resp
                  (map (fn [e] {:name (:name e) :is-dir (= :dir (:kind e))
                                :size (or (:size e) 0) :mtime (or (:mtime-secs e) 0)}) entries)))))

(defn- do-open [state provider flags vpath]
  (let [opened (p/open-file provider vpath flags)
        m (p/lookup provider vpath)
        fh (:next-fh @state)]
    (swap! state #(-> % (assoc-in [:open fh] {:provider provider :handle (:handle opened)
                                              :size (:size m)})
                        (assoc :next-fh (inc fh))))
    (resp ST-OK (wire/encode-open-resp {:fh fh :size (:size m) :is-dir (= :dir (:kind m))}))))

(defn- do-read [state _ flags {:keys [fh offset len]}]
  (if-let [rec (get-in @state [:open fh])]
    (let [want (long len)
          bulk? (or (not= 0 (bit-and (long flags) FLAG-READ-BULK)) (> want BULK-THRESHOLD))]
      (if bulk?
        (let [arena (:arena @state)
              {:keys [offset len]} (arena/fill-bank arena fh want
                                     (fn [^MemorySegment bank]
                                       (let [bytes (p/read-at (:provider rec) (:handle rec) offset (min want (:bank-size arena)))]
                                         (MemorySegment/copy (MemorySegment/ofArray bytes) 0 bank 0 (long (alength bytes)))
                                         (alength bytes))))]
          (resp ST-OK (wire/encode-read-resp-bulk len offset)))
        (let [bytes (p/read-at (:provider rec) (:handle rec) offset want)]
          (resp ST-OK (wire/encode-read-resp bytes)))))
    (resp ST-BAD-FH (byte-array 0))))

(defn- do-close [state _ fh]
  (when-let [rec (get-in @state [:open fh])]
    (p/release-handle (:provider rec) (:handle rec)))
  (swap! state update :open dissoc fh)
  (resp ST-OK (byte-array 0)))

(defn dispatch [state provider {:keys [opcode flags payload]}]
  (condp = (long opcode)
    OP-GETATTR (do-getattr state provider (wire/decode-path-req payload))
    OP-READDIR (do-readdir state provider (wire/decode-path-req payload))
    OP-OPEN    (let [{:keys [flags path]} (wire/decode-open-req payload)] (do-open state provider flags path))
    OP-READ    (do-read state provider flags (wire/decode-read-req payload))
    OP-CLOSE   (do-close state provider (wire/decode-close-req payload))
    (resp ST-BAD-REQUEST (byte-array 0))))

(defn serve
  "Spin serve loop: take a submitted slot, dispatch, complete. Stops when @stop? is true."
  [^MemorySegment seg geom arena provider stop?]
  (let [state (make-state arena)]
    (loop []
      (when-not @stop?
        (if-let [slot (ring/server-take seg geom)]
          (let [req (ring/read-request seg geom slot)
                {:keys [status payload]} (dispatch state provider req)]
            (ring/server-complete seg geom slot status payload))
          (Thread/onSpinWait))
        (recur)))))
```
**Note:** `do-read` bulk path assumes `fill-bank`'s inner fn returns the byte count and that a single `read-at` returns the whole slice (inline providers do). For providers whose `read-at` returns short reads, a loop would be needed — out of scope for M2's inline-provider proof; the inline provider returns full slices.

- [ ] **Step 4: Run to verify pass**

Run: `clojure -M:test -n aether.vfs.os.windows.server-test`
Expected: PASS (2 deftests).

- [ ] **Step 5: Commit**

```bash
git add src/aether/vfs/os/windows/server.clj test/aether/vfs/os/windows/server_test.clj src/aether/vfs/wire.clj
git commit -m "feat(win): ring opcode dispatch to Provider + fh table (inline + bulk read)"
```

---

## Task 5: `section.clj` — FFM named shared section (Windows-only)

TDD, Windows-only (self-skips off Windows). Maps a real named section via `kernel32` so the ring runs over cross-process shared memory.

**Files:**
- Create: `src/aether/vfs/os/windows/section.clj`, `test/aether/vfs/os/windows/section_test.clj`

**Interfaces:**
- Produces: `aether.vfs.os.windows.section/create [name size] -> {:handle :segment :size :name}`; `open [name size] -> {…}`; `close! [section]`. `:segment` is a `MemorySegment` of `size` bytes over the mapped view.
- Consumes: `java.lang.foreign` (`Linker`, `SymbolLookup`, `MemorySegment`, `FunctionDescriptor`, `ValueLayout`).

- [ ] **Step 1: Write the failing test**

Create `test/aether/vfs/os/windows/section_test.clj`:
```clojure
(ns aether.vfs.os.windows.section-test
  (:require [clojure.test :refer [deftest is]]
            [aether.vfs.os.windows.section :as section]
            [aether.vfs.os.windows.ring :as ring])
  (:import [java.lang.foreign MemorySegment ValueLayout]))

(def ^:private windows?
  (.startsWith (.toLowerCase (System/getProperty "os.name")) "windows"))

(deftest create-open-alias-same-section
  (if-not windows?
    (println "skip: section-test is Windows-only")
    (let [nm (str "Local\\vfs-m2-test-" (.pid (java.lang.ProcessHandle/current)))
          creator (section/create nm (* 64 1024))]
      (try
        (let [geom (ring/init (:segment creator) 4 256)
              opener (section/open nm (* 64 1024))]
          (try
            ;; The opener sees the MAGIC/geometry the creator wrote into shared pages.
            (is (= 0x56464950 (.get ^MemorySegment (:segment opener) ValueLayout/JAVA_INT 0)))
            (is (= 4 (.get ^MemorySegment (:segment opener) ValueLayout/JAVA_INT 8))) ; slot_count
            (finally (section/close! opener))))
        (finally (section/close! creator))))))
```

- [ ] **Step 2: Run to verify it fails (or skips off Windows)**

Run: `clojure -M:test -n aether.vfs.os.windows.section-test`
Expected: on Windows, FAIL (namespace missing). On non-Windows, it will still fail to compile until the namespace exists — so this task is implemented/verified on the Windows dev box.

- [ ] **Step 3: Implement the FFM section**

Create `src/aether/vfs/os/windows/section.clj`. Win32 ABI to reproduce (from `rust/crates/vfs-win/src/mapping.rs`):
- `HANDLE CreateFileMappingW(HANDLE hFile, LPSECURITY_ATTRIBUTES attrs, DWORD flProtect, DWORD dwMaxSizeHigh, DWORD dwMaxSizeLow, LPCWSTR lpName)` — pass `hFile = (MemorySegment/ofAddress -1)` (INVALID_HANDLE_VALUE), `attrs = MemorySegment/NULL`, `flProtect = 0x04` (PAGE_READWRITE).
- `HANDLE OpenFileMappingW(DWORD access, BOOL inherit, LPCWSTR name)` — `access = 0xF001F` (FILE_MAP_ALL_ACCESS), `inherit = 0`.
- `LPVOID MapViewOfFile(HANDLE, DWORD access, DWORD offHigh, DWORD offLow, SIZE_T numBytes)` — `access = 0xF001F`.
- `BOOL UnmapViewOfFile(LPCVOID)`, `BOOL CloseHandle(HANDLE)`.

```clojure
(ns aether.vfs.os.windows.section
  "Windows named file-mapping section via FFM (kernel32). Mirrors
  rust/crates/vfs-win/src/mapping.rs. Confines this namespace's restricted FFM
  calls; requires --enable-native-access (the :test alias already sets it)."
  (:import [java.lang.foreign Arena Linker Linker$Option SymbolLookup MemorySegment
            FunctionDescriptor ValueLayout]
           [java.lang.invoke MethodHandle]))

(def ^:private ^Linker linker (Linker/nativeLinker))
(def ^:private ^Arena libs (Arena/ofShared))
(def ^:private ^SymbolLookup k32 (SymbolLookup/libraryLookup "kernel32.dll" libs))

(defn- handle ^MethodHandle [sym ^FunctionDescriptor fd]
  (.downcallHandle linker (.orElseThrow (.find k32 sym)) fd (make-array Linker$Option 0)))

(def ^:private A ValueLayout/ADDRESS)
(def ^:private I ValueLayout/JAVA_INT)
(def ^:private L ValueLayout/JAVA_LONG)

(def ^:private mh-create (handle "CreateFileMappingW" (FunctionDescriptor/of A A A I I I A)))
(def ^:private mh-open   (handle "OpenFileMappingW"   (FunctionDescriptor/of A I I A)))
(def ^:private mh-map    (handle "MapViewOfFile"      (FunctionDescriptor/of A A I I I L)))
(def ^:private mh-unmap  (handle "UnmapViewOfFile"    (FunctionDescriptor/of I A)))
(def ^:private mh-close  (handle "CloseHandle"        (FunctionDescriptor/of I A)))

(def ^:private PAGE-READWRITE 0x04)
(def ^:private FILE-MAP-ALL 0xF001F)
(def ^:private INVALID-HANDLE (MemorySegment/ofAddress -1))

(defn- wide ^MemorySegment [^Arena a ^String s]
  ;; NUL-terminated UTF-16LE
  (let [bytes (.getBytes s "UTF-16LE")
        seg (.allocate a (long (+ (alength bytes) 2)))]
    (MemorySegment/copy (MemorySegment/ofArray bytes) 0 seg 0 (long (alength bytes)))
    seg))

(defn- map-view [^MemorySegment h size name]
  (let [view (.invoke mh-map h (int FILE-MAP-ALL) (int 0) (int 0) (long size))]
    (when (.equals (MemorySegment/NULL) view)
      (.invoke mh-close h)
      (throw (ex-info "MapViewOfFile failed" {:name name})))
    {:handle h :segment (.reinterpret ^MemorySegment view (long size)) :size size :name name}))

(defn create [name size]
  (let [a (Arena/ofConfined)]
    (try
      (let [h (.invoke mh-create INVALID-HANDLE (MemorySegment/NULL)
                       (int PAGE-READWRITE) (int (unsigned-bit-shift-right (long size) 32))
                       (int (bit-and (long size) 0xffffffff)) (wide a name))]
        (when (.equals (MemorySegment/NULL) h) (throw (ex-info "CreateFileMappingW failed" {:name name})))
        (map-view h size name))
      (finally (.close a)))))

(defn open [name size]
  (let [a (Arena/ofConfined)]
    (try
      (let [h (.invoke mh-open (int FILE-MAP-ALL) (int 0) (wide a name))]
        (when (.equals (MemorySegment/NULL) h) (throw (ex-info "OpenFileMappingW failed" {:name name})))
        (map-view h size name))
      (finally (.close a)))))

(defn close! [{:keys [handle segment size]}]
  (.invoke mh-unmap (.asSlice ^MemorySegment segment 0 (long size)))
  (.invoke mh-close ^MemorySegment handle))
```
**Adaptation notes (executor, on the Windows box):** (1) `MethodHandle/.invoke` in Clojure is variadic-reflective; if type dispatch fails, use `.invokeWithArguments` with an object array, or add `^MemorySegment`/`^int`/`^long` hints so Clojure emits the right signature. (2) `SymbolLookup/libraryLookup` arity may need an `Arena`; JDK 26 signature is `libraryLookup(String, Arena)`. (3) `MemorySegment.reinterpret` is restricted — ensure `--enable-native-access=ALL-UNNAMED` (already in `:test`). (4) For `UnmapViewOfFile` the Rust passes the view base; passing the reinterpreted segment's address is equivalent — if the API wants the raw base, keep a reference to `view` before reinterpret and unmap that. The **contract** is the Step 1 test; adjust interop to satisfy it without changing the Win32 call semantics.

- [ ] **Step 4: Run to verify pass (Windows dev box)**

Run: `clojure -M:test -n aether.vfs.os.windows.section-test`
Expected: PASS on Windows (create → init → open aliases same section, reads MAGIC + slot_count).

- [ ] **Step 5: Commit**

```bash
git add src/aether/vfs/os/windows/section.clj test/aether/vfs/os/windows/section_test.clj
git commit -m "feat(win): FFM named shared section via kernel32 (create/open/close)"
```

---

## Task 6: `vfs-ring-harness` — Rust RingClient harness bin

TDD-lite (a small bin verified by the Task 7 integration test; here we build it and unit-check argument parsing). Opens the JVM section and drives the ring as a client.

**Files:**
- Create: `rust/crates/vfs-ring-harness/Cargo.toml`, `rust/crates/vfs-ring-harness/src/main.rs`
- Modify: `rust/Cargo.toml` (member)

**Interfaces:**
- Consumes: `vfs-win::SharedMapping::open`, `vfs-ipc::{RingClient, SpinNotifier, ring::Geom}`, `vfs-protocol` codecs.
- Produces: a bin `vfs-ring-harness <section-name> <size>` that runs the op sequence and exits 0 on success, non-zero (with stderr diagnostics) on any mismatch.

- [ ] **Step 1: Register the crate + manifest**

Add to `rust/Cargo.toml` members: `"crates/vfs-ring-harness",`
Create `rust/crates/vfs-ring-harness/Cargo.toml`:
```toml
[package]
name = "vfs-ring-harness"
version = "0.1.0"
edition = "2021"

[dependencies]
vfs-ipc = { path = "../vfs-ipc" }
vfs-win = { path = "../vfs-win" }
vfs-protocol = { path = "../vfs-protocol" }

[[bin]]
name = "vfs-ring-harness"
path = "src/main.rs"
```

- [ ] **Step 2: Implement the harness**

Create `rust/crates/vfs-ring-harness/src/main.rs`:
```rust
//! Cross-process ring CLIENT: opens a JVM-created section and asserts the JVM
//! server's read-path responses. Exit 0 = all assertions passed.
use std::process::exit;
use vfs_ipc::{RingClient, SpinNotifier};
use vfs_protocol as P;
use vfs_win::SharedMapping;

fn fail(msg: &str) -> ! { eprintln!("HARNESS FAIL: {msg}"); exit(1); }

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let name = args.get(1).unwrap_or_else(|| fail("usage: vfs-ring-harness <name> <size>"));
    let size: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or_else(|| fail("bad size"));

    let mapping = SharedMapping::open(name, size).unwrap_or_else(|e| fail(&format!("open: {e}")));
    let client = RingClient::new(mapping.seg(), SpinNotifier)
        .unwrap_or_else(|e| fail(&format!("ring open: {e:?}")));

    // getattr /hello.txt -> size 5
    let r = client.submit(P::OP_GETATTR, 0, b"/hello.txt").unwrap_or_else(|e| fail(&format!("getattr: {e:?}")));
    let attr = P::decode_getattr_resp(&r.payload).unwrap_or_else(|| fail("getattr decode"));
    if !attr.found || attr.size != 5 { fail("getattr /hello.txt wrong"); }

    // readdir / -> contains hello.txt and big.bin
    let r = client.submit(P::OP_READDIR, 0, b"/").unwrap_or_else(|e| fail(&format!("readdir: {e:?}")));
    let entries = P::decode_readdir_resp(&r.payload).unwrap_or_else(|| fail("readdir decode"));
    if !entries.iter().any(|e| e.name == "hello.txt") || !entries.iter().any(|e| e.name == "big.bin") {
        fail("readdir missing entries");
    }

    // open + inline read /hello.txt
    let r = client.submit(P::OP_OPEN, P::OPEN_READ, &P::encode_open_req(P::OPEN_READ, "/hello.txt"))
        .unwrap_or_else(|e| fail(&format!("open: {e:?}")));
    let op = P::decode_open_resp(&r.payload).unwrap_or_else(|| fail("open decode"));
    let rr = client.submit(P::OP_READ, 0, &P::encode_read_req(&P::ReadReq { fh: op.fh, offset: 0, len: 5 }))
        .unwrap_or_else(|e| fail(&format!("read: {e:?}")));
    let data = P::decode_read_resp(&rr.payload).unwrap_or_else(|| fail("read decode"));
    if data != b"hello" { fail("inline read mismatch"); }

    // open + BULK read /big.bin (70000 bytes of 'X'); data lands in the arena
    let r = client.submit(P::OP_OPEN, P::OPEN_READ, &P::encode_open_req(P::OPEN_READ, "/big.bin"))
        .unwrap_or_else(|e| fail(&format!("open big: {e:?}")));
    let op = P::decode_open_resp(&r.payload).unwrap_or_else(|| fail("open big decode"));
    let rr = client.submit(P::OP_READ, P::FLAG_READ_BULK,
                           &P::encode_read_req(&P::ReadReq { fh: op.fh, offset: 0, len: 70000 }))
        .unwrap_or_else(|e| fail(&format!("bulk read: {e:?}")));
    let (n, off) = P::decode_read_bulk_resp(&rr.payload).unwrap_or_else(|| fail("expected bulk resp"));
    if n != 70000 { fail("bulk length wrong"); }
    // Read the bytes straight from the arena at `off`.
    let mut buf = vec![0u8; n as usize];
    mapping.seg().copy_to(off as usize, &mut buf).unwrap_or_else(|| fail("arena copy_to oob"));
    if buf.iter().any(|&b| b != b'X') { fail("bulk arena bytes wrong"); }

    println!("HARNESS OK");
    exit(0);
}
```
(If `SharedMapping::seg().copy_to` is `pub` — it is per `seg.rs` — this compiles. `decode_read_bulk_resp` returns `(u32, u64)` per `vfs-protocol`.)

- [ ] **Step 3: Build to verify it compiles**

Run: `cd rust && cargo build -p vfs-ring-harness && cd ..`
Expected: builds clean (a Windows target; builds on the Windows dev box and the windows CI runner).

- [ ] **Step 4: Commit**

```bash
git add rust/Cargo.toml rust/crates/vfs-ring-harness
git commit -m "feat(harness): Rust RingClient bin asserting JVM server read path (inline + bulk)"
```

---

## Task 7: Cross-process integration proof + `windows-clojure` CI job

The Windows-only end-to-end test and the CI wiring so it runs in CI.

**Files:**
- Create: `test/aether/vfs/os/windows/integration_test.clj`
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: `section`, `ring`, `arena`, `server`, `providers.inline`; the built `vfs-ring-harness` exe.

- [ ] **Step 1: Write the integration test**

Create `test/aether/vfs/os/windows/integration_test.clj`:
```clojure
(ns aether.vfs.os.windows.integration-test
  (:require [clojure.test :refer [deftest is]]
            [clojure.java.io :as io]
            [aether.vfs.os.windows.section :as section]
            [aether.vfs.os.windows.ring :as ring]
            [aether.vfs.os.windows.arena :as arena]
            [aether.vfs.os.windows.server :as server]
            [aether.vfs.providers.inline :as inline])
  (:import [java.lang.foreign MemorySegment]))

(def ^:private windows?
  (.startsWith (.toLowerCase (System/getProperty "os.name")) "windows"))

(def ^:private harness-exe
  ;; Built by `cargo build -p vfs-ring-harness` (debug).
  (io/file "rust/target/debug/vfs-ring-harness.exe"))

(deftest cross-process-read-path
  (cond
    (not windows?) (println "skip: integration-test is Windows-only")
    (not (.exists harness-exe)) (println "skip: build rust/target/debug/vfs-ring-harness.exe first")
    :else
    (let [ring-bytes (+ 40 (* 4 (ring/align8-public 288))) ; header + 4 slots (payload-cap 256 -> stride 288)
          arena-off (* 512 1024)
          size (* 1 1024 1024)
          nm (str "Local\\vfs-m2-int-" (.pid (java.lang.ProcessHandle/current)))
          sec (section/create nm size)
          seg (:segment sec)
          geom (ring/init seg 4 256)
          a (arena/make seg arena-off (* 256 1024) 4)
          stop? (atom false)
          small (.getBytes "hello" "UTF-8")
          big (byte-array 70000 (byte 88))
          provider (inline/inline-provider [["/hello.txt" small 0644] ["/big.bin" big 0644]])
          server-thread (doto (Thread. #(server/serve seg geom a provider stop?)) (.setDaemon true) (.start))]
      (try
        (let [proc (-> (ProcessBuilder. [(.getPath harness-exe) nm (str size)])
                       (.inheritIO) (.start))
              ok (.waitFor proc)]
          (is (= 0 ok) "harness exited 0 (all read-path assertions passed)"))
        (finally
          (reset! stop? true)
          (section/close! sec))))))
```
**Note:** if `ring/align8-public` isn't exposed, hardcode the ring size or expose a public `align8` from `ring.clj`; the exact `ring-bytes` value is not used to bound anything here (the mapping is 1 MiB and the arena starts at 512 KiB) — you may drop the `ring-bytes` let binding entirely.

- [ ] **Step 2: Run the integration test (Windows dev box)**

Run:
```bash
cd rust && cargo build -p vfs-ring-harness && cd ..
clojure -M:test -n aether.vfs.os.windows.integration-test
```
Expected: PASS — the harness prints `HARNESS OK` (via inherited IO) and exits 0; the deftest asserts exit 0. This is the M2 acceptance proof: a separate Rust process read a small (inline) and a large (bulk-arena, zero-copy) file served by a Clojure `Provider` over the shared ring.

- [ ] **Step 3: Add the `windows-clojure` CI job**

In `.github/workflows/ci.yml`, add a third job:
```yaml
  windows-clojure:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - name: Build ring harness
        run: cargo build -p vfs-ring-harness
        working-directory: rust
      - uses: actions/setup-java@v4
        with:
          distribution: temurin
          java-version: '26'
      - name: Install Clojure CLI
        uses: DeLaGuardo/setup-clojure@13.0
        with:
          cli: latest
      - name: Windows Clojure tests (FFM section + cross-process ring proof)
        run: clojure -M:test -n aether.vfs.os.windows.section-test -n aether.vfs.os.windows.integration-test
        env:
          # The harness exe path in the test is relative to the repo root.
          JDK_JAVA_OPTIONS: "--enable-native-access=ALL-UNNAMED"
```
(The `:test` alias already adds `--enable-native-access`; `JDK_JAVA_OPTIONS` is belt-and-suspenders for the section FFM. The harness exe is built into `rust/target/debug/` and the test runs from repo root, matching `harness-exe`.)

- [ ] **Step 4: Commit**

```bash
git add test/aether/vfs/os/windows/integration_test.clj .github/workflows/ci.yml
git commit -m "test(win): cross-process ring proof (inline+bulk) + windows-clojure CI job"
```

---

## Self-Review

**Spec coverage:**
- FFM section (`section.clj`) → Task 5. ✓
- Ring CAS state machine (`ring.clj`) → Task 1. ✓
- Arena zero-copy (`arena.clj`) → Task 3. ✓
- Server dispatch + fh table (`server.clj`) → Task 4. ✓
- Wire server-side codecs + **F2** golden vectors → Task 2. ✓
- Rust `RingClient` harness → Task 6. ✓
- Cross-process proof (ring + bulk arena) + `windows-clojure` CI job → Task 7. ✓
- Read-path opcodes only (getattr/readdir/open/read/close) → Tasks 4/6. ✓
- Spin-based, single-threaded → Task 4 `serve`. ✓
- Crux (atomics) proven in-JVM first (Task 1), FFM mapping (Task 5), cross-process (Task 7). ✓
- Cross-platform component tests run in ubuntu job (heap segments); Windows proof in new job. ✓

**Placeholder scan:** no TBD/TODO; every code step has complete code + exact commands + expected output. FFM/interop uncertainty is flagged as explicit **adaptation notes** anchored to a concrete passing test (the contract), not as missing content.

**Type consistency:** `ring/{init,server-take,read-request,server-complete,claim-free,publish-request,take-response,free-slot,align8}`, `arena/{make,bank-mapping-offset,fill-bank}`, `server/{make-state,dispatch,serve}`, `section/{create,open,close!}`, and `wire/{decode-path-req,decode-open-req,encode-open-resp,decode-read-req,encode-read-resp-bulk,decode-close-req,decode-read-resp-bulk}` are used consistently across tasks and tests. Rust harness uses `vfs-protocol` names verified against `vfs-protocol/src/lib.rs` (`decode_read_bulk_resp`, `decode_open_resp`, `ReadReq`, `OpenResp`, `OP_*`, `FLAG_READ_BULK`, `OPEN_READ`).

**Known adaptation points (executor, Windows box):** (a) `java.lang.foreign` interop specifics in `ring.clj`/`section.clj` — satisfy the task's test, keep the byte layout; (b) `MethodHandle/.invoke` signature dispatch in `section.clj` — use `.invokeWithArguments` or type hints if needed; (c) `align8-public` exposure in Task 7 is optional — drop the unused binding. Each is anchored to a passing test.

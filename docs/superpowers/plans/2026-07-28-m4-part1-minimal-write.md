# M4 Part 1 — Minimal Write Proof Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prove one write end-to-end, pure-ring: a real injected process creates a virtual file, writes bytes via `NtWriteFile`, and reads them back — with the JVM's aether `overlay` Provider authoritative. Land the two subagent-friendly foundations (the `OP_WRITE` wire codec; the JVM write dispatch against `Writable`), then a controller-run spike that adds the crux new shim work (`NtWriteFile` hook + create-write routing) and proves the minimal write.

**Architecture:** Tasks 1–2 (subagent TDD) build the wire + JVM sides, testable in-JVM over a heap segment with an `overlay` Provider — no shim needed. Task 3 (controller-run, like M3's spike) adds `fuse_client.write` + a new `NtWriteFile` hook + write-disposition create routing in the Rust shim, and proves create+write+read-back through real injection. Its outcome pins the shim diff; productionization (write fixture + `launch.clj` overlay + CI) follows in a Part-1b plan once the mechanism is confirmed.

**Tech Stack:** Rust (`vfs-protocol`, `vfs-shim`, `vfs-ipc`, `xtask-descriptor`); Clojure 1.12 + deps.clj, Java 26, FFM; GitHub Actions.

## Global Constraints

- Nothing hardcodes a wire/layout constant — read from `aether.vfs.protocol`. New wire messages get golden vectors + byte-for-byte Clojure conformance (anti-drift). `resources/*.edn` stay `eol=lf`.
- The write path is **pure-ring**: the JVM `overlay` Provider is authoritative (copy-up + whiteouts; base never mutated). The shim's local overlay engine is NOT used for ring configs.
- Wire formats (mirror across Rust `vfs-protocol` ↔ Clojure `aether.vfs.wire`, little-endian): `OP_WRITE(6)` req = `fh:u64 | offset:u64 | len:u32 | pad:u32 | data[len]`; write resp = `bytes_written:u32 | pad:u32`. `OPEN_WRITE = 2` flag already exists.
- `xtask-descriptor` stays portable (no Windows-only dep). Any `os/windows/*` Clojure defers native lookups (lazy). Commit bodies end with `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`. ≥1 commit per task.

---

## Task 1: `OP_WRITE` wire codec (Rust + golden + Clojure)

TDD, cross-platform. Mirror the read codecs for the write request/response.

**Files:**
- Modify: `rust/crates/vfs-protocol/src/lib.rs`, `rust/crates/xtask-descriptor/src/lib.rs`, `resources/protocol-golden.edn`, `src/aether/vfs/wire.clj`, `test/aether/vfs/wire_conformance_test.clj`

**Interfaces:**
- Rust: `WriteReq { fh:u64, offset:u64, len:u32 }`, `encode_write_req(&WriteReq, data:&[u8]) -> Vec<u8>`, `decode_write_req(&[u8]) -> Option<(WriteReq, Vec<u8>)>`, `encode_write_resp(n:u32) -> Vec<u8>`, `decode_write_resp(&[u8]) -> Option<u32>`.
- Clojure: `wire/encode-write-req [{:fh :offset} ^bytes data]`, `decode-write-req [^bytes] -> {:fh :offset :data}`, `encode-write-resp [n]`, `decode-write-resp [^bytes] -> n`.

- [ ] **Step 1: Add Rust codecs (mirror `encode_read_req`/`encode_read_resp`)**

In `rust/crates/vfs-protocol/src/lib.rs`, add near the READ codecs:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteReq { pub fh: u64, pub offset: u64, pub len: u32 }

/// WRITE req: `fh:u64 | offset:u64 | len:u32 | pad:u32 | data[len]`
pub fn encode_write_req(r: &WriteReq, data: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(24 + data.len());
    b.extend_from_slice(&r.fh.to_le_bytes());
    b.extend_from_slice(&r.offset.to_le_bytes());
    b.extend_from_slice(&(data.len() as u32).to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(data);
    b
}
pub fn decode_write_req(p: &[u8]) -> Option<(WriteReq, Vec<u8>)> {
    if p.len() < 24 { return None; }
    let fh = u64::from_le_bytes(p[0..8].try_into().ok()?);
    let offset = u64::from_le_bytes(p[8..16].try_into().ok()?);
    let len = u32::from_le_bytes(p[16..20].try_into().ok()?) as usize;
    if p.len() < 24 + len { return None; }
    Some((WriteReq { fh, offset, len: len as u32 }, p[24..24 + len].to_vec()))
}
/// WRITE resp: `bytes_written:u32 | pad:u32`
pub fn encode_write_resp(n: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(8);
    b.extend_from_slice(&n.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b
}
pub fn decode_write_resp(p: &[u8]) -> Option<u32> {
    if p.len() < 4 { return None; }
    Some(u32::from_le_bytes(p[0..4].try_into().ok()?))
}
```
Add a Rust roundtrip test in the same `#[cfg(test)] mod tests` (mirror `read_req_resp_roundtrip`).

- [ ] **Step 2: Add golden vectors**

In `rust/crates/xtask-descriptor/src/lib.rs` `golden_vectors()`:
```rust
        ("write-req-fh7-off10-abc",
         P::encode_write_req(&vfs_protocol::WriteReq { fh: 7, offset: 10, len: 3 }, b"abc")),
        ("write-resp-3", P::encode_write_resp(3)),
```

- [ ] **Step 3: Regenerate + verify Rust**

Run:
```bash
bin/regen-protocol
cd rust && cargo test -p vfs-protocol -p xtask-descriptor && cd ..
```
Expected: golden gains the two vectors; both crates' tests PASS.

- [ ] **Step 4: Failing Clojure conformance test**

Append to `test/aether/vfs/wire_conformance_test.clj`:
```clojure
(deftest write-codecs-match-golden
  (let [g (golden)]
    (is (= (:write-req-fh7-off10-abc g) (hex (wire/encode-write-req {:fh 7 :offset 10} (.getBytes "abc" "UTF-8")))))
    (is (= (:write-resp-3 g) (hex (wire/encode-write-resp 3))))))

(deftest write-decoders-roundtrip
  (is (= {:fh 7 :offset 10 :data "abc"}
         (let [{:keys [fh offset data]} (wire/decode-write-req (wire/encode-write-req {:fh 7 :offset 10} (.getBytes "abc" "UTF-8")))]
           {:fh fh :offset offset :data (String. ^bytes data "UTF-8")})))
  (is (= 3 (wire/decode-write-resp (wire/encode-write-resp 3)))))
```

- [ ] **Step 5: Run to verify failure**

Run: `clojure -M:test -n aether.vfs.wire-conformance-test`
Expected: FAIL — `wire/encode-write-req` unresolved.

- [ ] **Step 6: Implement Clojure codecs**

Append to `src/aether/vfs/wire.clj` (helpers `baos`/`buf`/`put-u32!`/`put-u64!` exist):
```clojure
(defn encode-write-req ^bytes [{:keys [fh offset]} ^bytes data]
  (let [b (baos)] (put-u64! b fh) (put-u64! b offset)
    (put-u32! b (alength data)) (put-u32! b 0)
    (.write b data 0 (alength data)) (.toByteArray b)))

(defn decode-write-req [^bytes p]
  (let [bb (buf p) fh (.getLong bb) offset (.getLong bb)
        len (bit-and (long (.getInt bb)) 0xffffffff)]
    (.getInt bb) ; pad
    (let [d (byte-array len)] (.get bb d) {:fh fh :offset offset :data d})))

(defn encode-write-resp ^bytes [n]
  (let [b (baos)] (put-u32! b n) (put-u32! b 0) (.toByteArray b)))

(defn decode-write-resp [^bytes p] (bit-and (long (.getInt (buf p))) 0xffffffff))
```

- [ ] **Step 7: Run to verify pass + commit**

Run: `clojure -M:test -n aether.vfs.wire-conformance-test` → PASS.
```bash
git add rust/crates/vfs-protocol resources/protocol-golden.edn rust/crates/xtask-descriptor src/aether/vfs/wire.clj test/aether/vfs/wire_conformance_test.clj
git commit -m "feat(m4): OP_WRITE wire codec (Rust + golden + Clojure, conformance-pinned)"
```

---

## Task 2: JVM server write dispatch (write-open + `OP_WRITE`)

TDD, cross-platform (heap segment + an `overlay` Writable Provider). Implements the write-disposition `OP_OPEN` and `OP_WRITE` (currently `BAD_REQUEST`).

**Files:**
- Modify: `src/aether/vfs/os/windows/server.clj`, `test/aether/vfs/os/windows/server_test.clj`

**Interfaces:**
- Consumes: `aether.vfs.provider` `Writable` wrappers (`create`, `write-at` — verify exact names in `src/aether/vfs/provider.clj`), `aether.vfs.providers.overlay` (`overlay-provider`), `aether.vfs.wire` (Task 1).
- Produces: `server/dispatch` handles `OP_OPEN` with `OPEN_WRITE` (→ create/open-writable, fh table) and `OP_WRITE` (→ write-at), returning `ST_OK` + `encode-write-resp`.

- [ ] **Step 1: Write the failing test**

Add to `test/aether/vfs/os/windows/server_test.clj` (create a Writable overlay provider over a temp overrides dir; write a new file; read it back):
```clojure
(deftest write-open-write-and-read-back
  (let [s (seg (* 1 1024 1024))
        a (arena/make s 0 4096 2)
        st (server/make-state a)
        overrides (str (System/getProperty "java.io.tmpdir") "vfs-m4-" (System/nanoTime))
        _ (.mkdirs (java.io.File. overrides))
        base (inline/inline-provider [])                    ; empty base
        p (overlay/overlay-provider base overrides)          ; Writable
        ;; open /new.txt for write (OPEN_WRITE=2), create
        op (server/dispatch st p {:opcode 3 :flags 2 :payload (wire/encode-open-req 2 "/new.txt")})
        {:keys [fh]} (wire/decode-open-resp (:payload op))
        w  (server/dispatch st p {:opcode 6 :flags 0 :payload (wire/encode-write-req {:fh fh :offset 0} (.getBytes "hi!" "UTF-8"))})
        n  (wire/decode-write-resp (:payload w))]
    (is (= 0 (:status op)))
    (is (= 3 n))
    ;; read it back through a fresh open/read
    (let [op2 (server/dispatch st p {:opcode 3 :flags 1 :payload (wire/encode-open-req 1 "/new.txt")})
          {:keys [fh]} (wire/decode-open-resp (:payload op2))
          rd (server/dispatch st p {:opcode 5 :flags 0 :payload (wire/encode-read-req {:fh fh :offset 0 :len 3})})]
      (is (= "hi!" (String. ^bytes (wire/decode-read-resp (:payload rd)) "UTF-8"))))))
```
(Verify `overlay/overlay-provider` and the `Writable` fn names against `src/aether/vfs/providers/overlay.clj` and `provider.clj`; adapt the provider construction to what actually creates a writable overlay over an empty base + a host overrides dir.)

- [ ] **Step 2: Run to verify failure**

Run: `clojure -M:test -n aether.vfs.os.windows.server-test`
Expected: FAIL — `OP_WRITE` currently returns `BAD_REQUEST` (-3); write-open may error.

- [ ] **Step 3: Implement**

In `src/aether/vfs/os/windows/server.clj`: add `OP-WRITE 6`, `OPEN-WRITE 2`, and status `ST-READ-ONLY`/reuse `ST-BAD-REQUEST` for read-only providers. In `do-open`, branch on `(bit-and flags OPEN-WRITE)`: if set, `provider/create` (or open-writable) the path, allocate fh. Add `do-write`:
```clojure
(defn- do-write [state _ {:keys [fh offset data]}]
  (if-let [rec (get-in @state [:open fh])]
    (let [n (p/write-at (:provider rec) (:handle rec) offset data)]
      (resp ST-OK (wire/encode-write-resp (int n))))
    (resp ST-BAD-FH (byte-array 0))))
```
Wire `OP-WRITE` into `dispatch`: `OP-WRITE (do-write state provider (wire/decode-write-req payload))`. Adjust `do-open` to use `provider/create` when `OPEN-WRITE` is set (and map a `:read-only` raise to a status). Use `aether.vfs.provider`'s actual `Writable` wrapper names (`create`, `write-at`) — inspect `provider.clj`.

- [ ] **Step 4: Run to verify pass + commit**

Run: `clojure -M:test -n aether.vfs.os.windows.server-test` → PASS.
```bash
git add src/aether/vfs/os/windows/server.clj test/aether/vfs/os/windows/server_test.clj
git commit -m "feat(m4): JVM server write-open + OP_WRITE dispatch against Writable overlay"
```

---

## Task 3: Shim write hooks + minimal write proof (controller-run spike)

**Not a subagent TDD task — controller-run** (like the M3 spike): iterative native shim work + integration. Its output is the confirmed shim diff and a working create+write+read-back through real injection.

**Goal / decision:** a real injected process creates a virtual file, writes via `NtWriteFile`, and reads the bytes back — served by the JVM `overlay` Provider over the ring. Determine and implement the minimal shim changes:
- `fuse_client.write(fh, offset, &[u8]) -> Result<usize,i32>` (submit `OP_WRITE`, decode resp).
- `fuse_client` write-open (submit `OP_OPEN` with `OPEN_WRITE`) if the read `open` doesn't already carry flags.
- **New `NtWriteFile` hook**: for a virtual write handle (under-root, opened write) → `fuse_client.write`; else pass through. (Study the existing `NtReadFile` hook + virtual-handle table in `hook.rs` — mirror it for writes.)
- `create_hook`: a WRITE-disposition open of an under-root vpath → open a virtual write handle over the ring (reuse the virtual-handle machinery).

- [ ] **Step 1: Map the read-side virtual-handle + NtReadFile hook**

Read `vfs-shim/src/hook.rs` around the `NtReadFile` hook and the virtual file-handle table (how a virtual read handle is created in `create_hook` and dispatched in the read hook). This is the template for the write side.

- [ ] **Step 2: Add `fuse_client` write method(s)**

Add `write` (and a write-capable `open`/`create`) to `vfs-shim/src/fuse_client.rs` mirroring `read_fragmented`/`open` (submit `OP_WRITE`/`OP_OPEN`, use inline for small; bulk-arena for large is Part-2). Build `-p vfs-shim`.

- [ ] **Step 3: Add the `NtWriteFile` hook + create-write routing**

In `vfs-shim/src/hook.rs`: install an `NtWriteFile` detour; for a tracked virtual write handle, call `fuse_client::global().write(fh, offset, buf)` and complete the IRP with the byte count; else call the trampoline. Route `create_hook`'s WRITE-disposition decision for under-root vpaths to open a virtual write handle. Build `-p vfs-shim-dll`.

- [ ] **Step 4: Prove it (crude JVM driver, throwaway)**

Adapt the M3 `spike_driver.clj` (local throwaway) to: serve an `overlay` Provider (empty base + temp overrides dir), launch `vfs-fixture-write` (a new tiny bin that creates the vpath, writes bytes, closes, reopens, reads them back, exits 0 iff equal), and confirm exit 0. Log server opcodes (expect `OP_OPEN(write)` → `OP_WRITE` → `OP_READ`).

- [ ] **Step 5: Record findings**

Write `.superpowers/sdd/m4-spike-findings.md`: the exact shim diff (fuse_client write, NtWriteFile hook, create-write routing), the working driver/env, and any wire/JVM adjustments — the input to the Part-1b productionization plan (fixture + `launch.clj` overlay support + e2e in CI) and Part 2 (delete/rename/mkdir/truncate + F1).

---

## Self-Review

**Spec coverage (Part 1 scope):** `OP_WRITE` wire codec → Task 1; JVM write dispatch → Task 2; shim `NtWriteFile` hook + create-write routing + `fuse_client.write` + minimal proof → Task 3 (controller spike). Delete/rename/mkdir/truncate, F1 hardening, and productionized fixture/launch/CI → deferred to Part 1b / Part 2 (post-spike), as the spec sequences.

**Placeholder scan:** Tasks 1–2 have complete code + commands + expected output. Task 3 is a controller-run spike with concrete steps and a decision output (its per-hook specifics are discovered by studying the read-side template — the nature of a spike, not a placeholder).

**Type consistency:** `WriteReq`/`encode_write_req`/`encode_write_resp` (Task 1) used by golden + Clojure conformance; `wire/{encode,decode}-write-{req,resp}` (Task 1) used by Task 2's `do-write`; `overlay/overlay-provider` + `p/{create,write-at}` (Task 2) must be reconciled against the real `provider.clj`/`overlay.clj` names (flagged in Task 2).

**Executor note:** Tasks 1–2 are subagent TDD; Task 3 is controller-run (new native shim hooking — iterative). Verify `aether.vfs.provider` Writable wrapper names and `overlay-provider`'s construction before writing Task 2's dispatch.

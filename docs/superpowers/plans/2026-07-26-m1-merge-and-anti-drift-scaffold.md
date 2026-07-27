# M1 — Merge, Restructure & Anti-Drift Scaffold Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Merge `halgari/aether-vfs` (Clojure) and `halgari/vfs` (Rust) into one Clojure-rooted repository with an explicit `os/{linux,windows}` split, and stand up the CI-enforced protocol anti-drift system (generated descriptor + golden vectors + staleness gate) **before any Windows delivery code exists**.

**Architecture:** aether-vfs becomes the host repo; the Rust engine moves under `rust/` (its Cargo workspace intact). The Rust `vfs-ipc`/`vfs-protocol` crates remain the single source of truth for the wire format and ring/arena memory layout. A new `xtask-descriptor` Rust binary emits a machine-readable descriptor (`resources/protocol-descriptor.edn`) and cross-language golden vectors (`resources/protocol-golden.edn`); the Clojure side loads the descriptor instead of hardcoding constants, and a minimal Clojure wire codec is conformance-locked against the golden vectors from day one. A regen+`git diff --exit-code` gate (script + GitHub Actions) makes any one-sided protocol change unmergeable.

**Tech Stack:** Rust (stable, existing workspace), Clojure 1.12 + `deps.edn` + cognitect test-runner, `java.lang.foreign` (Panama) later (M2+), GitHub Actions.

## Global Constraints

- Consumer is always JVM/Clojure; the merged library is named **aether-vfs**, namespace root `aether.vfs.*`. (Spec §Decisions.1, .5)
- OS-agnostic Clojure never mentions an OS; OS-specific Clojure lives under `src/aether/vfs/os/{linux,windows}/`. (Spec §Decisions.5)
- Rust is the single source of truth for the protocol; Clojure never hardcodes a wire/layout magic number — it reads `resources/protocol-descriptor.edn`. (Spec §Anti-drift)
- Every protocol/layout change starts in Rust, then regenerate, then Clojure consumes; the staleness gate enforces this direction. (Spec §Anti-drift, ownership rule)
- Ring/layout facts are fixed and must be reproduced exactly by any mirror: `RingHeader` size 40 / align 8, offsets magic 0, version 4, slot_count 8, slot_stride 12, payload_cap 16, req_seq 24, submit_seq 32; `SlotHeader` size 32 / align 8, offsets state 0, opcode 4, flags 8, payload_len 12, status 16, req_id 24; `MAGIC = 0x56464950`, `VERSION = 1`. (`rust/crates/vfs-ipc/src/layout.rs`)
- Full Linux Clojure test suite and full Rust `cargo test` must stay green at every task boundary (regression guard).
- Frequent commits: one commit per task minimum.

---

## File Structure

Created / modified in M1 (paths are in the **merged** repo, after Task 1):

- `rust/` — the entire existing `vfs` Cargo workspace, moved verbatim (16 crates + root `Cargo.toml`).
- `rust/crates/xtask-descriptor/` — NEW Rust bin+lib crate: builds the descriptor + golden EDN.
- `src/aether/vfs/os/linux/fuse.clj` — moved from `src/aether/vfs/fuse.clj`, ns → `aether.vfs.os.linux.fuse`.
- `src/aether/vfs/os/linux/proton.clj` — moved from `src/aether/vfs/proton.clj`, ns → `aether.vfs.os.linux.proton`.
- `src/aether/vfs/protocol.clj` — NEW: loads and exposes the descriptor.
- `src/aether/vfs/wire.clj` — NEW: minimal opcode/message codec, mirrors `vfs-protocol`, conformance-locked to golden.
- `resources/protocol-descriptor.edn` — NEW, generated + committed.
- `resources/protocol-golden.edn` — NEW, generated + committed.
- `test/aether/vfs/protocol_test.clj` — NEW.
- `test/aether/vfs/wire_conformance_test.clj` — NEW.
- `test/aether/vfs/mount_test.clj`, `test/aether/vfs/proton_test.clj` — modified requires (fuse/proton relocation).
- `bin/regen-protocol` — NEW: regenerate descriptor+golden, used by the gate.
- `.github/workflows/ci.yml` — NEW: rust test + clojure test + staleness gate.
- `deps.edn` — modified: add `resources` to `:paths`.

---

## Task 1: Merge repos & restructure into one Clojure-rooted tree

Mechanical, not TDD. Deliverable: one repo where both `cargo test` (under `rust/`) and `clojure -M:test` pass, with the `os/linux` split in place. This task preserves both git histories via subtree merge.

**Files:**
- Create: `rust/` (moved workspace), `src/aether/vfs/os/linux/{fuse,proton}.clj`
- Modify: `deps.edn`, `test/aether/vfs/mount_test.clj:7`, `test/aether/vfs/proton_test.clj:3`, `README.md`
- Delete (via move): `src/aether/vfs/fuse.clj`, `src/aether/vfs/proton.clj`

**Interfaces:**
- Produces: namespace `aether.vfs.os.linux.fuse` (was `aether.vfs.fuse`) exposing the same public vars (`mount`, `mount-router`); namespace `aether.vfs.os.linux.proton` (was `aether.vfs.proton`) exposing the same vars (`proton-command`, `launch-proton!`, `teardown!`, etc.). The `rust/` Cargo workspace exposes all existing crates unchanged.

- [ ] **Step 1: Clone the host repo and add vfs as a subtree under `rust/`**

Run (from a scratch working dir):
```bash
git clone git@github.com:halgari/aether-vfs.git
cd aether-vfs
git checkout -b unified-cross-platform-vfs
git remote add vfs-src https://github.com/halgari/vfs.git
git fetch vfs-src
# History-preserving move of the entire vfs repo into rust/
git subtree add --prefix=rust vfs-src master
```
Expected: `rust/` now contains `Cargo.toml` + `crates/…` with vfs history retained.

- [ ] **Step 2: Move the design docs into the merged repo**

Run:
```bash
mkdir -p docs/superpowers/specs docs/superpowers/plans
git mv rust/docs/superpowers/specs/2026-07-26-unified-cross-platform-vfs-design.md docs/superpowers/specs/
git mv rust/docs/superpowers/plans/2026-07-26-m1-merge-and-anti-drift-scaffold.md docs/superpowers/plans/
git commit -m "chore: relocate unified-vfs design+plan to merged repo root"
```
Expected: this plan and its spec live at the merged repo root.

- [ ] **Step 3: Relocate the OS-specific Clojure namespaces**

Run:
```bash
mkdir -p src/aether/vfs/os/linux
git mv src/aether/vfs/fuse.clj  src/aether/vfs/os/linux/fuse.clj
git mv src/aether/vfs/proton.clj src/aether/vfs/os/linux/proton.clj
```
Then edit the `ns` forms:
- `src/aether/vfs/os/linux/fuse.clj:1` — change `(ns aether.vfs.fuse` → `(ns aether.vfs.os.linux.fuse`
- `src/aether/vfs/os/linux/proton.clj:1` — change `(ns aether.vfs.proton` → `(ns aether.vfs.os.linux.proton`

- [ ] **Step 4: Fix the two requires that reference the moved namespaces**

- `test/aether/vfs/mount_test.clj:7` — change `[aether.vfs.fuse :as fuse]` → `[aether.vfs.os.linux.fuse :as fuse]`
- `test/aether/vfs/proton_test.clj:3` — change `[aether.vfs.proton :as proton]` → `[aether.vfs.os.linux.proton :as proton]`

(These are the only two references — verified against the aether-vfs source graph. `fuse.clj` requires only `aether.vfs.{error,provider,types}`; `proton.clj` requires only `clojure.java.io`; no OS-agnostic namespace depends on either.)

- [ ] **Step 5: Update README require examples**

In `README.md`, change the two example requires:
- `[aether.vfs.fuse :as fuse]` → `[aether.vfs.os.linux.fuse :as fuse]`
- `[aether.vfs.proton :as proton]` → `[aether.vfs.os.linux.proton :as proton]`

- [ ] **Step 6: Verify the Clojure suite still passes**

Run:
```bash
clojure -M:test
```
Expected: PASS (same tests as before the move; `mount-test` prints `skip: /dev/fuse not available` if no FUSE device — that is a pass, not a failure).

- [ ] **Step 7: Verify the Rust workspace still builds and tests**

Run:
```bash
cd rust && cargo test && cd ..
```
Expected: PASS (unchanged from the vfs repo — the subtree move touched no Rust file).

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "refactor: merge vfs Rust engine under rust/, split os/linux Clojure namespaces"
```

---

## Task 2: `xtask-descriptor` crate — emit `protocol-descriptor.edn`

TDD. A new Rust crate that reads the source-of-truth constants/offsets from `vfs-ipc` + `vfs-protocol` and renders a deterministic EDN descriptor. Tests assert on the pure builder function (no file IO).

**Files:**
- Create: `rust/crates/xtask-descriptor/Cargo.toml`, `rust/crates/xtask-descriptor/src/lib.rs`, `rust/crates/xtask-descriptor/src/main.rs`
- Modify: `rust/Cargo.toml` (add member)

**Interfaces:**
- Produces: `xtask_descriptor::descriptor_edn() -> String` (deterministic EDN text) and `xtask_descriptor::content_hash(&str) -> u32` (FNV-1a). `main.rs` writes `descriptor_edn()` to `<out-dir>/protocol-descriptor.edn`.
- Consumes: `vfs_ipc::layout` constants/offsets; `vfs_protocol` opcode/status/flag constants.

- [ ] **Step 1: Register the crate and write its manifest**

Add to `rust/Cargo.toml` members list: `"crates/xtask-descriptor",`

Create `rust/crates/xtask-descriptor/Cargo.toml`:
```toml
[package]
name = "xtask-descriptor"
version = "0.1.0"
edition = "2021"

[dependencies]
vfs-ipc = { path = "../vfs-ipc" }
vfs-protocol = { path = "../vfs-protocol" }

[[bin]]
name = "xtask-descriptor"
path = "src/main.rs"
```

- [ ] **Step 2: Write the failing test**

Create `rust/crates/xtask-descriptor/src/lib.rs`:
```rust
//! Emits the protocol descriptor (single source of truth for the Clojure mirror).

use std::fmt::Write as _;
use vfs_ipc::layout as L;
use vfs_protocol as P;

/// FNV-1a over the descriptor text; low 32 bits used as the handshake hash (M3).
pub fn content_hash(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Deterministic EDN descriptor of the wire + ring/arena layout.
pub fn descriptor_edn() -> String {
    let mut s = String::new();
    let body = descriptor_body();
    // Wrap with a self-describing hash of the body so drift is detectable by hash alone.
    let _ = write!(
        s,
        "{{:magic 0x{:08X}\n :version {}\n :content-hash 0x{:08X}\n{}}}\n",
        L::MAGIC,
        L::VERSION,
        content_hash(&body),
        body
    );
    s
}

fn descriptor_body() -> String {
    let mut s = String::new();
    let _ = write!(s, " :opcodes {{");
    for (name, v) in [
        ("getattr", P::OP_GETATTR), ("readdir", P::OP_READDIR), ("open", P::OP_OPEN),
        ("materialize", P::OP_MATERIALIZE), ("read", P::OP_READ), ("write", P::OP_WRITE),
        ("setattr", P::OP_SETATTR), ("rename", P::OP_RENAME), ("delete", P::OP_DELETE),
        ("mkdir", P::OP_MKDIR), ("close", P::OP_CLOSE),
        ("register-process", P::OP_REGISTER_PROCESS), ("heartbeat", P::OP_HEARTBEAT),
    ] {
        let _ = write!(s, ":{name} {v} ");
    }
    let _ = write!(s, "}}\n :statuses {{");
    for (name, v) in [
        ("ok", P::ST_OK), ("not-found", P::ST_NOT_FOUND), ("not-a-directory", P::ST_NOT_A_DIRECTORY),
        ("bad-request", P::ST_BAD_REQUEST), ("io-error", P::ST_IO_ERROR), ("is-dir", P::ST_IS_DIR),
        ("bad-fh", P::ST_BAD_FH), ("no-space", P::ST_NO_SPACE),
    ] {
        let _ = write!(s, ":{name} {v} ");
    }
    let _ = write!(
        s,
        "}}\n :flags {{:open-read {} :open-write {} :read-bulk {} :read-resp-bulk-bit 0x{:08X}}}\n",
        P::OPEN_READ, P::OPEN_WRITE, P::FLAG_READ_BULK, P::READ_RESP_BULK_BIT
    );
    let _ = write!(
        s,
        " :slot-states {{:free {} :claimed {} :submitted {} :processing {} :completed {}}}\n",
        L::ST_FREE, L::ST_CLAIMED, L::ST_SUBMITTED, L::ST_PROCESSING, L::ST_COMPLETED
    );
    let _ = write!(
        s,
        " :ring-header {{:size {} :align 8 :fields {{:magic {} :version {} :slot-count {} :slot-stride {} :payload-cap {} :req-seq {} :submit-seq {}}}}}\n",
        L::RING_HEADER_SIZE, L::RH_MAGIC, L::RH_VERSION, L::RH_SLOT_COUNT, L::RH_SLOT_STRIDE,
        L::RH_PAYLOAD_CAP, L::RH_REQ_SEQ, L::RH_SUBMIT_SEQ
    );
    let _ = write!(
        s,
        " :slot-header {{:size {} :align 8 :fields {{:state {} :opcode {} :flags {} :payload-len {} :status {} :req-id {}}}}}\n",
        L::SLOT_HEADER_SIZE, L::SH_STATE, L::SH_OPCODE, L::SH_FLAGS, L::SH_PAYLOAD_LEN, L::SH_STATUS, L::SH_REQ_ID
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_has_stable_layout_facts() {
        let edn = descriptor_edn();
        assert!(edn.contains(":size 40"), "ring header size");
        assert!(edn.contains(":req-seq 24"));
        assert!(edn.contains(":submit-seq 32"));
        assert!(edn.contains(":read 5"));      // OP_READ
        assert!(edn.contains(":open-write 2"));
        assert!(edn.contains(":not-found -1"));
    }

    #[test]
    fn descriptor_is_deterministic() {
        assert_eq!(descriptor_edn(), descriptor_edn());
    }

    #[test]
    fn hash_changes_with_content() {
        assert_ne!(content_hash("a"), content_hash("b"));
    }
}
```

- [ ] **Step 3: Run the test to verify it fails to compile/find the crate**

Run:
```bash
cd rust && cargo test -p xtask-descriptor
```
Expected: FAIL — `main.rs` missing (bin target declared but no file). Compilation error names `src/main.rs`.

- [ ] **Step 4: Write the binary entry point**

Create `rust/crates/xtask-descriptor/src/main.rs`:
```rust
use std::path::PathBuf;

fn main() {
    let out_dir: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("../../resources"));
    std::fs::create_dir_all(&out_dir).expect("create out dir");
    std::fs::write(out_dir.join("protocol-descriptor.edn"), xtask_descriptor::descriptor_edn())
        .expect("write descriptor");
    // golden vectors added in Task 3
    println!("wrote descriptor to {}", out_dir.display());
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run:
```bash
cargo test -p xtask-descriptor
```
Expected: PASS (3 tests).

- [ ] **Step 6: Generate and commit the descriptor**

Run (from `rust/`):
```bash
cargo run -p xtask-descriptor -- ../resources
cd ..
git add rust/Cargo.toml rust/crates/xtask-descriptor resources/protocol-descriptor.edn
git commit -m "feat: xtask-descriptor emits protocol-descriptor.edn from vfs-ipc/vfs-protocol"
```
Expected: `resources/protocol-descriptor.edn` exists and is committed.

---

## Task 3: Golden vectors — emit `protocol-golden.edn` + Rust conformance test

TDD. Extend the emitter to render canonical `(input → exact bytes)` vectors using the real `vfs-protocol` encoders, and add a Rust test asserting the encoders reproduce the committed golden bytes. This is one half of the cross-language pin (the Clojure half is Task 5).

**Files:**
- Modify: `rust/crates/xtask-descriptor/src/lib.rs`, `rust/crates/xtask-descriptor/src/main.rs`
- Create: `resources/protocol-golden.edn`

**Interfaces:**
- Produces: `xtask_descriptor::golden_edn() -> String` and the committed `resources/protocol-golden.edn`, a map `{:vectors [ {:name kw :op kw :bytes "hexstring"} … ]}`.
- Consumes: `vfs_protocol` encoders (`encode_open_req`, `encode_getattr_resp`, `encode_read_req`, `encode_read_resp`, `encode_readdir_resp`, `encode_close_req`).

- [ ] **Step 1: Write the failing test**

Add to `rust/crates/xtask-descriptor/src/lib.rs`:
```rust
use vfs_protocol::{AttrResp, DirEntryWire, ReadReq};

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes { let _ = write!(s, "{b:02x}"); }
    s
}

/// Canonical (name, encoded-bytes) vectors. Fixed inputs → exact wire bytes.
pub fn golden_vectors() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("open-req-read-skyrim", P::encode_open_req(P::OPEN_READ, "Data/Skyrim.esm")),
        ("getattr-resp-file-123",
         P::encode_getattr_resp(&AttrResp { found: true, is_dir: false, size: 123, mtime: -7 })),
        ("read-req-fh7-off10-len4",
         P::encode_read_req(&ReadReq { fh: 7, offset: 10, len: 4 })),
        ("read-resp-abcd", P::encode_read_resp(b"abcd")),
        ("readdir-resp-two",
         P::encode_readdir_resp(&[
             DirEntryWire { name: "a.esp".into(), is_dir: false, size: 10, mtime: 1 },
             DirEntryWire { name: "sub".into(),   is_dir: true,  size: 0,  mtime: 0 },
         ])),
        ("close-req-99", P::encode_close_req(99)),
    ]
}

pub fn golden_edn() -> String {
    let mut s = String::from("{:vectors [\n");
    for (name, bytes) in golden_vectors() {
        let _ = write!(s, "  {{:name :{name} :bytes \"{}\"}}\n", hex(&bytes));
    }
    s.push_str("]}\n");
    s
}

#[cfg(test)]
mod golden_tests {
    use super::*;

    #[test]
    fn encoders_match_committed_golden() {
        // The committed file is the contract; regenerating must not change it silently.
        let committed = include_str!("../../../../resources/protocol-golden.edn");
        assert_eq!(golden_edn(), committed,
            "golden vectors drifted — regenerate with bin/regen-protocol and review");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run:
```bash
cd rust && cargo test -p xtask-descriptor golden
```
Expected: FAIL — `include_str!` cannot find `resources/protocol-golden.edn` (not generated yet). Compile error naming the missing file.

- [ ] **Step 3: Generate the golden file from the emitter**

Update `rust/crates/xtask-descriptor/src/main.rs` to also write golden:
```rust
    std::fs::write(out_dir.join("protocol-golden.edn"), xtask_descriptor::golden_edn())
        .expect("write golden");
```
Then run (from `rust/`):
```bash
cargo run -p xtask-descriptor -- ../resources
```
Expected: `resources/protocol-golden.edn` created.

- [ ] **Step 4: Run the test to verify it passes**

Run:
```bash
cargo test -p xtask-descriptor
```
Expected: PASS (golden test now matches the committed file).

- [ ] **Step 5: Commit**

```bash
cd ..
git add rust/crates/xtask-descriptor resources/protocol-golden.edn
git commit -m "feat: golden wire vectors emitted + Rust conformance test"
```

---

## Task 4: Clojure `aether.vfs.protocol` — descriptor loader

TDD. The Clojure entry point that loads `resources/protocol-descriptor.edn` and exposes it as data. Downstream OS/windows code (M2+) reads offsets/opcodes from here, never from literals.

**Files:**
- Create: `src/aether/vfs/protocol.clj`, `test/aether/vfs/protocol_test.clj`
- Modify: `deps.edn` (add `"resources"` to `:paths`)

**Interfaces:**
- Produces: `aether.vfs.protocol/descriptor` (a delay'd map), `(op kw)` → opcode long, `(status kw)` → status long, `(ring-header-offset kw)` → long, `(slot-header-offset kw)` → long, `aether.vfs.protocol/version` (long).
- Consumes: `resources/protocol-descriptor.edn` on the classpath.

- [ ] **Step 1: Add resources to the classpath**

In `deps.edn`, change `:paths ["src"]` → `:paths ["src" "resources"]`.

- [ ] **Step 2: Write the failing test**

Create `test/aether/vfs/protocol_test.clj`:
```clojure
(ns aether.vfs.protocol-test
  (:require [clojure.test :refer [deftest is]]
            [aether.vfs.protocol :as proto]))

(deftest loads-descriptor
  (is (= 1 proto/version))
  (is (= 0x56464950 (:magic @proto/descriptor))))

(deftest exposes-opcodes-and-statuses
  (is (= 5 (proto/op :read)))
  (is (= 3 (proto/op :open)))
  (is (= -1 (proto/status :not-found)))
  (is (= 0 (proto/status :ok))))

(deftest exposes-layout-offsets
  (is (= 24 (proto/ring-header-offset :req-seq)))
  (is (= 32 (proto/ring-header-offset :submit-seq)))
  (is (= 16 (proto/slot-header-offset :status)))
  (is (= 24 (proto/slot-header-offset :req-id))))
```

- [ ] **Step 3: Run the test to verify it fails**

Run:
```bash
clojure -M:test -v aether.vfs.protocol-test
```
Expected: FAIL — namespace `aether.vfs.protocol` not found.

- [ ] **Step 4: Write the implementation**

Create `src/aether/vfs/protocol.clj`:
```clojure
(ns aether.vfs.protocol
  "Loads the generated protocol descriptor (single source of truth: the Rust
  vfs-ipc/vfs-protocol crates). Nothing in this library hardcodes a wire or
  ring-layout magic number — it reads them from here. Regenerate the descriptor
  with bin/regen-protocol after any Rust protocol change."
  (:require [clojure.edn :as edn]
            [clojure.java.io :as io]))

(def ^:private resource-name "protocol-descriptor.edn")

(def descriptor
  (delay
    (with-open [r (io/reader (or (io/resource resource-name)
                                 (throw (ex-info "protocol-descriptor.edn not on classpath"
                                                 {:resource resource-name}))))]
      (edn/read (java.io.PushbackReader. r)))))

(def version (:version @descriptor))

(defn op ^long [k] (long (get-in @descriptor [:opcodes k])))
(defn status ^long [k] (long (get-in @descriptor [:statuses k])))
(defn ring-header-offset ^long [k] (long (get-in @descriptor [:ring-header :fields k])))
(defn slot-header-offset ^long [k] (long (get-in @descriptor [:slot-header :fields k])))
```

- [ ] **Step 5: Run the test to verify it passes**

Run:
```bash
clojure -M:test -v aether.vfs.protocol-test
```
Expected: PASS (3 deftests).

- [ ] **Step 6: Commit**

```bash
git add deps.edn src/aether/vfs/protocol.clj test/aether/vfs/protocol_test.clj
git commit -m "feat: aether.vfs.protocol loads generated descriptor (no hardcoded constants)"
```

---

## Task 5: Clojure `aether.vfs.wire` — minimal codec, conformance-locked to golden

TDD. A minimal Clojure encoder/decoder that mirrors `vfs-protocol` for the six golden messages, tested byte-for-byte against `resources/protocol-golden.edn`. This locks the Clojure codec to the Rust source of truth from day one; M2 extends it (write ops) under the same conformance test.

**Files:**
- Create: `src/aether/vfs/wire.clj`, `test/aether/vfs/wire_conformance_test.clj`

**Interfaces:**
- Produces: `aether.vfs.wire/encode-open-req [flags path] -> bytes`, `encode-getattr-resp [{:keys [found is-dir size mtime]}] -> bytes`, `encode-read-req [{:keys [fh offset len]}] -> bytes`, `encode-read-resp [^bytes data] -> bytes`, `encode-readdir-resp [entries] -> bytes` (entry = `{:name :is-dir :size :mtime}`), `encode-close-req [fh] -> bytes`; matching `decode-*` for `getattr-resp`, `read-resp`, `readdir-resp`. All little-endian, matching `vfs-protocol` byte-for-byte.
- Consumes: `aether.vfs.protocol` (opcodes/flags, for the encoders that embed them; the six golden messages embed only `OPEN_READ`).

- [ ] **Step 1: Write the failing conformance test**

Create `test/aether/vfs/wire_conformance_test.clj`:
```clojure
(ns aether.vfs.wire-conformance-test
  (:require [clojure.test :refer [deftest is]]
            [clojure.edn :as edn]
            [clojure.java.io :as io]
            [aether.vfs.wire :as wire]))

(defn- golden []
  (with-open [r (io/reader (io/resource "protocol-golden.edn"))]
    (into {} (map (juxt :name :bytes)) (:vectors (edn/read (java.io.PushbackReader. r))))))

(defn- hex [^bytes b]
  (apply str (map #(format "%02x" (bit-and % 0xff)) b)))

(deftest clojure-encoders-match-rust-golden
  (let [g (golden)]
    (is (= (:open-req-read-skyrim g)      (hex (wire/encode-open-req 1 "Data/Skyrim.esm"))))
    (is (= (:getattr-resp-file-123 g)     (hex (wire/encode-getattr-resp {:found true :is-dir false :size 123 :mtime -7}))))
    (is (= (:read-req-fh7-off10-len4 g)   (hex (wire/encode-read-req {:fh 7 :offset 10 :len 4}))))
    (is (= (:read-resp-abcd g)            (hex (wire/encode-read-resp (.getBytes "abcd")))))
    (is (= (:readdir-resp-two g)          (hex (wire/encode-readdir-resp [{:name "a.esp" :is-dir false :size 10 :mtime 1}
                                                                          {:name "sub"   :is-dir true  :size 0  :mtime 0}]))))
    (is (= (:close-req-99 g)              (hex (wire/encode-close-req 99))))))

(deftest decode-roundtrips
  (is (= {:found true :is-dir false :size 123 :mtime -7}
         (wire/decode-getattr-resp (wire/encode-getattr-resp {:found true :is-dir false :size 123 :mtime -7}))))
  (is (= "abcd" (String. ^bytes (wire/decode-read-resp (wire/encode-read-resp (.getBytes "abcd"))))))
  (is (= [{:name "a.esp" :is-dir false :size 10 :mtime 1}]
         (wire/decode-readdir-resp (wire/encode-readdir-resp [{:name "a.esp" :is-dir false :size 10 :mtime 1}])))))
```

- [ ] **Step 2: Run the test to verify it fails**

Run:
```bash
clojure -M:test -v aether.vfs.wire-conformance-test
```
Expected: FAIL — namespace `aether.vfs.wire` not found.

- [ ] **Step 3: Write the implementation**

Create `src/aether/vfs/wire.clj` (little-endian, mirroring `rust/crates/vfs-protocol/src/lib.rs` exactly):
```clojure
(ns aether.vfs.wire
  "Wire codec mirroring the Rust vfs-protocol crate byte-for-byte. Conformance
  is enforced against resources/protocol-golden.edn (wire-conformance-test).
  All integers little-endian. Extend under the same golden test — never change
  a byte layout here without changing it in Rust first and regenerating."
  (:import [java.io ByteArrayOutputStream]
           [java.nio ByteBuffer ByteOrder]))

(defn- baos ^ByteArrayOutputStream [] (ByteArrayOutputStream.))
(defn- put-u32! [^ByteArrayOutputStream b v]
  (let [x (long v)] (dotimes [i 4] (.write b (int (bit-and (bit-shift-right x (* 8 i)) 0xff))))))
(defn- put-u64! [^ByteArrayOutputStream b v]
  (let [x (long v)] (dotimes [i 8] (.write b (int (bit-and (bit-shift-right x (* 8 i)) 0xff))))))

(defn encode-open-req ^bytes [flags ^String path]
  (let [b (baos)] (put-u32! b flags) (.write b (.getBytes path "UTF-8")) (.toByteArray b)))

(defn encode-getattr-resp ^bytes [{:keys [found is-dir size mtime]}]
  (let [b (baos)]
    (.write b (int (if found 1 0)))
    (.write b (int (if is-dir 1 0)))
    (put-u64! b size)
    (put-u64! b mtime)
    (.toByteArray b)))

(defn encode-read-req ^bytes [{:keys [fh offset len]}]
  (let [b (baos)] (put-u64! b fh) (put-u64! b offset) (put-u32! b len) (put-u32! b 0) (.toByteArray b)))

(defn encode-read-resp ^bytes [^bytes data]
  (let [b (baos)] (put-u32! b (alength data)) (put-u32! b 0) (.write b data 0 (alength data)) (.toByteArray b)))

(defn encode-readdir-resp ^bytes [entries]
  (let [b (baos)]
    (put-u32! b (count entries))
    (doseq [{:keys [name is-dir size mtime]} entries]
      (let [nb (.getBytes ^String name "UTF-8")]
        (put-u32! b (alength nb))
        (.write b nb 0 (alength nb))
        (.write b (int (if is-dir 1 0)))
        (put-u64! b size)
        (put-u64! b mtime)))
    (.toByteArray b)))

(defn encode-close-req ^bytes [fh]
  (let [b (baos)] (put-u64! b fh) (.toByteArray b)))

;; --- decoders (little-endian views) ---

(defn- buf ^ByteBuffer [^bytes p] (doto (ByteBuffer/wrap p) (.order ByteOrder/LITTLE_ENDIAN)))

(defn decode-getattr-resp [^bytes p]
  (let [bb (buf p)
        found (not (zero? (.get bb)))
        is-dir (not (zero? (.get bb)))]
    {:found found :is-dir is-dir :size (.getLong bb) :mtime (.getLong bb)}))

(defn decode-read-resp ^bytes [^bytes p]
  (let [bb (buf p)
        n (bit-and (long (.getInt bb)) 0xffffffff)]
    (.getInt bb) ; pad
    (let [out (byte-array n)] (.get bb out) out)))

(defn decode-readdir-resp [^bytes p]
  (let [bb (buf p)
        n (.getInt bb)]
    (vec (for [_ (range n)]
           (let [nlen (.getInt bb)
                 nb (byte-array nlen)]
             (.get bb nb)
             {:name (String. nb "UTF-8")
              :is-dir (not (zero? (.get bb)))
              :size (.getLong bb)
              :mtime (.getLong bb)})))))
```

- [ ] **Step 4: Run the test to verify it passes**

Run:
```bash
clojure -M:test -v aether.vfs.wire-conformance-test
```
Expected: PASS (both deftests) — the Clojure encoders produce byte-identical output to the Rust golden vectors.

- [ ] **Step 5: Run the full suite (no regression)**

Run:
```bash
clojure -M:test
```
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/aether/vfs/wire.clj test/aether/vfs/wire_conformance_test.clj
git commit -m "feat: aether.vfs.wire codec conformance-locked to Rust golden vectors"
```

---

## Task 6: Staleness gate — regen script + CI

Deliverable: a one-command regenerator and a CI workflow that fails if the committed descriptor/golden are stale or if either test suite fails. This makes a one-sided Rust protocol change unmergeable.

**Files:**
- Create: `bin/regen-protocol`, `.github/workflows/ci.yml`

**Interfaces:**
- Produces: `bin/regen-protocol` (regenerates `resources/protocol-{descriptor,golden}.edn`); CI job that runs it and asserts a clean tree.

- [ ] **Step 1: Write the regen script**

Create `bin/regen-protocol`:
```bash
#!/usr/bin/env bash
# Regenerate the protocol descriptor + golden vectors from the Rust source of
# truth. Run this after ANY change to vfs-ipc/vfs-protocol, then commit the
# updated resources/ files together with the Rust change.
set -euo pipefail
cd "$(dirname "$0")/.."
( cd rust && cargo run --quiet -p xtask-descriptor -- ../resources )
echo "regenerated resources/protocol-descriptor.edn and resources/protocol-golden.edn"
```
Make it executable:
```bash
chmod +x bin/regen-protocol
```

- [ ] **Step 2: Verify regen is a no-op on a clean tree**

Run:
```bash
bin/regen-protocol
git diff --exit-code resources/
```
Expected: exit 0, no diff (the committed files already match the emitter output).

- [ ] **Step 3: Prove the gate catches drift (manual check)**

Run:
```bash
# Simulate a Rust-side protocol change without regenerating:
sed -n '1p' resources/protocol-descriptor.edn   # inspect
# Hand-edit resources/protocol-descriptor.edn (e.g. flip a number), then:
bin/regen-protocol
git diff --exit-code resources/ ; echo "exit=$?"
```
Expected: `git diff --exit-code` returns non-zero (exit=1) — drift detected. Then restore:
```bash
git checkout resources/protocol-descriptor.edn
```

- [ ] **Step 4: Write the CI workflow**

Create `.github/workflows/ci.yml`:
```yaml
name: ci
on:
  push:
  pull_request:
jobs:
  build-and-verify:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - name: Rust tests
        run: cd rust && cargo test
      - name: Regenerate protocol artifacts
        run: bin/regen-protocol
      - name: Fail on protocol drift
        run: git diff --exit-code resources/
      - uses: actions/setup-java@v4
        with:
          distribution: temurin
          java-version: '21'
      - name: Install Clojure CLI
        uses: DeLaGuardo/setup-clojure@13.0
        with:
          cli: latest
      - name: Clojure tests
        run: clojure -M:test
```
(The `mount-test` self-skips without `/dev/fuse`, which the GitHub runner lacks — that is a pass.)

- [ ] **Step 5: Commit**

```bash
git add bin/regen-protocol .github/workflows/ci.yml
git commit -m "ci: protocol staleness gate (regen + git diff --exit-code) + rust/clojure test jobs"
```

- [ ] **Step 6: Push and open the PR**

Run:
```bash
git push -u origin unified-cross-platform-vfs
gh pr create --title "M1: merge + os split + protocol anti-drift scaffold" \
  --body "Merges the Rust vfs engine under rust/, splits os/linux Clojure namespaces, and stands up the CI-enforced protocol anti-drift system (generated descriptor, golden vectors, staleness gate) before any Windows code. See docs/superpowers/plans/2026-07-26-m1-merge-and-anti-drift-scaffold.md."
```
Expected: CI runs green on the PR.

---

## Self-Review

**Spec coverage** (spec §Anti-drift is the M1 heart):
- Generated descriptor, Clojure never hardcodes → Tasks 2, 4. ✓
- Committed + staleness gate → Tasks 3 (committed golden), 6 (gate). ✓
- Cross-language golden vectors → Task 3 (Rust side) + Task 5 (Clojure side). ✓
- Runtime version handshake → **deferred to M3** (needs the ring header on the wire); the descriptor already carries `:version` + `:content-hash` so M3 can enforce it. Noted in spec §Anti-drift.4 and §Milestones (M3). ✓ (not an M1 gap)
- Ownership rule enforced by gate → Task 6. ✓
- One repo + os/{linux,windows} split → Task 1 (linux split; windows dir arrives in M2). ✓
- Rust reference daemon retained → Task 1 moves the whole workspace verbatim (director/server included). ✓
- Linux suite green throughout → verification steps in Tasks 1, 5. ✓

**Placeholder scan:** no TBD/TODO; every code step shows complete code; every run step shows the command + expected result. ✓

**Type consistency:** `descriptor_edn`/`golden_edn`/`content_hash` (Task 2/3) match their `main.rs` and test call sites; Clojure `proto/op`,`proto/status`,`proto/ring-header-offset`,`proto/slot-header-offset`,`proto/version`,`proto/descriptor` (Task 4) match the test; `wire/encode-*`/`decode-*` names (Task 5) match the conformance test; golden vector `:name` keywords (Task 3) match the Clojure lookups (Task 5). ✓

**Known adaptation point for the executor:** Task 3's `include_str!("../../../../resources/protocol-golden.edn")` path is relative to `rust/crates/xtask-descriptor/src/lib.rs`; if the compiler reports the path does not resolve, count the directory hops from that file to the repo-root `resources/` and adjust — the test intent (compare `golden_edn()` to the committed file) is the contract.

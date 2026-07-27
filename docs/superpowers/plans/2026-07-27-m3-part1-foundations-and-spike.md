# M3 Part 1 — Foundations + De-risk Spike Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the two spike-independent foundations M3 needs (the JVM server answering `OP_HEARTBEAT`; a read fixture + a Clojure `VFS_SHIM_CONFIG` encoder pinned to the Rust format), then run the de-risk spike that resolves M3's one real unknown — whether the existing shim serves *pure-ring* with an empty snapshot when driven by the JVM — before productionizing the launcher.

**Architecture:** Two subagent TDD tasks build the pieces the spike needs (Task 1: `OP_HEARTBEAT`; Task 2: `vfs-fixture-read` + Clojure config encoder + cross-language golden pin). Task 3 is a **controller-run** spike (like the M1 repo-merge bootstrap): it wires the JVM section + config + env to the *existing* `vfs-inject`/`vfs-shim`, launches the read fixture under real injection, and observes whether a real process reads JVM-`Provider` bytes. Its outcome (works as-is / needs a small fuse-authoritative hook tweak) drives M3 Part 2.

**Tech Stack:** Clojure 1.12 + deps.clj, Java 26, `java.lang.foreign`; Rust (`vfs-shim`, `vfs-inject`, `vfs-win`, `vfs-ipc`, `xtask-descriptor`); GitHub Actions.

## Global Constraints

- Consumer is JVM/Clojure; OS-specific Clojure under `src/aether/vfs/os/windows/`. Nothing hardcodes a wire/layout constant — read from `aether.vfs.protocol` (M1 descriptor). (Anti-drift)
- Any new `os/windows/*` namespace with native/FFM lookups MUST defer them (lazy `delay`) so the full `clojure -M:test` loads on Linux — the M2 `section.clj` regression lesson (`SymbolLookup/libraryLookup` at load time aborted the ubuntu suite). Cross-platform pieces (config encoder) run in the ubuntu CI job; Windows/injection pieces self-skip off Windows.
- `OP_HEARTBEAT = 13` (from the descriptor / `vfs-protocol`); the shim's `try_init_from_env` calls `heartbeat()` and refuses to attach the FuseClient unless the server answers `ST_OK` (0).
- `VFS_SHIM_CONFIG` wire format (mirror `vfs-shim::encode_config` → `encode_config_full(root,"",[],snapshot)`): `[u32 root_len LE][root utf8][u32 overlay_len=0][ "VFS1" ][u32 n_static=0][snapshot bytes]`. Pure-ring uses an **empty** snapshot.
- Commit message bodies end with exactly: `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`.
- Frequent commits: ≥1 per task.

---

## File Structure

- `src/aether/vfs/os/windows/server.clj` — MODIFY: add `OP_HEARTBEAT` dispatch.
- `test/aether/vfs/os/windows/server_test.clj` — MODIFY: heartbeat test.
- `rust/crates/vfs-fixture-read/{Cargo.toml,src/main.rs}` — NEW: read-and-assert target exe.
- `rust/Cargo.toml` — MODIFY: add member.
- `rust/crates/xtask-descriptor/src/lib.rs` — MODIFY: add a golden config vector.
- `resources/protocol-golden.edn` — regenerated (adds the config vector).
- `src/aether/vfs/os/windows/shim_config.clj` — NEW: Clojure `VFS_SHIM_CONFIG` encoder.
- `test/aether/vfs/os/windows/shim_config_test.clj` — NEW: byte-for-byte vs golden.

---

## Task 1: JVM server answers `OP_HEARTBEAT`

TDD, cross-platform (heap segment). Required for the shim's FuseClient to attach.

**Files:**
- Modify: `src/aether/vfs/os/windows/server.clj`, `test/aether/vfs/os/windows/server_test.clj`

**Interfaces:**
- Produces: `server/dispatch` returns `{:status 0 :payload (byte-array 0)}` for `{:opcode 13 …}`.

- [ ] **Step 1: Write the failing test**

Add to `test/aether/vfs/os/windows/server_test.clj`:
```clojure
(deftest heartbeat-returns-ok
  (let [s (seg (* 1 1024 1024))
        a (arena/make s 0 4096 2)
        st (server/make-state a)
        p (provider)
        hb (server/dispatch st p {:opcode 13 :flags 0 :payload (byte-array 0)})]
    (is (= 0 (:status hb)))))
```
(`seg`, `arena`, `provider`, `server` are already required in this test ns.)

- [ ] **Step 2: Run to verify failure**

Run: `clojure -M:test -n aether.vfs.os.windows.server-test`
Expected: FAIL — heartbeat currently hits the `dispatch` default → `ST-BAD-REQUEST` (-3), not 0.

- [ ] **Step 3: Implement**

In `src/aether/vfs/os/windows/server.clj`, add an `OP-HEARTBEAT` constant and a `condp` branch in `dispatch`:
```clojure
(def ^:private OP-HEARTBEAT 13)
```
and inside `dispatch`'s `condp`, before the default:
```clojure
    OP-HEARTBEAT (resp ST-OK (byte-array 0))
```

- [ ] **Step 4: Run to verify pass**

Run: `clojure -M:test -n aether.vfs.os.windows.server-test`
Expected: PASS (all server tests incl. the new heartbeat one).

- [ ] **Step 5: Commit**

```bash
git add src/aether/vfs/os/windows/server.clj test/aether/vfs/os/windows/server_test.clj
git commit -m "feat(win): server answers OP_HEARTBEAT (required for shim FuseClient attach)"
```

---

## Task 2: read fixture + Clojure `VFS_SHIM_CONFIG` encoder (golden-pinned)

TDD. A minimal Rust target exe that reads a file and asserts its bytes (the thing the injector will launch), plus a Clojure encoder for the shim config file, pinned byte-for-byte to a Rust golden vector (anti-drift, like M1/M2).

**Files:**
- Create: `rust/crates/vfs-fixture-read/{Cargo.toml,src/main.rs}`; `src/aether/vfs/os/windows/shim_config.clj`; `test/aether/vfs/os/windows/shim_config_test.clj`
- Modify: `rust/Cargo.toml`; `rust/crates/xtask-descriptor/src/lib.rs`; `resources/protocol-golden.edn`

**Interfaces:**
- Produces: bin `vfs-fixture-read` — reads `%VFS_FIXTURE_PATH%`, compares to `%VFS_FIXTURE_EXPECT%` (a decimal expected length) and (optionally) a fill byte `%VFS_FIXTURE_FILL%`; exit 0 on match, non-zero + stderr on mismatch. `aether.vfs.os.windows.shim-config/encode [root snapshot-bytes] -> ^bytes` mirroring `vfs-shim::encode_config`.
- Consumes: `vfs-shim::{encode_config, decode_config}` (Rust golden side).

- [ ] **Step 1: Add the read fixture crate**

Add to `rust/Cargo.toml` members: `"crates/vfs-fixture-read",`
Create `rust/crates/vfs-fixture-read/Cargo.toml`:
```toml
[package]
name = "vfs-fixture-read"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "vfs-fixture-read"
path = "src/main.rs"
```
Create `rust/crates/vfs-fixture-read/src/main.rs`:
```rust
//! Injection read target: opens a (virtual) file via the normal Win32 path
//! (std::fs::read → CreateFileW → NtCreateFile, so the injected shim's hooks
//! intercept it), and asserts its length/content. Exit 0 iff it matches.
use std::process::exit;

fn main() {
    let path = std::env::var("VFS_FIXTURE_PATH").unwrap_or_else(|_| {
        eprintln!("VFS_FIXTURE_PATH unset"); exit(2);
    });
    let expect_len: usize = std::env::var("VFS_FIXTURE_EXPECT")
        .ok().and_then(|s| s.parse().ok())
        .unwrap_or_else(|| { eprintln!("VFS_FIXTURE_EXPECT unset/bad"); exit(2); });
    let fill: Option<u8> = std::env::var("VFS_FIXTURE_FILL").ok()
        .and_then(|s| s.parse().ok());

    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) => { eprintln!("FIXTURE FAIL: read {path}: {e}"); exit(1); }
    };
    if data.len() != expect_len {
        eprintln!("FIXTURE FAIL: len {} != {expect_len}", data.len()); exit(1);
    }
    if let Some(b) = fill {
        if data.iter().any(|&x| x != b) {
            eprintln!("FIXTURE FAIL: content byte != {b}"); exit(1);
        }
    }
    println!("FIXTURE OK: {} bytes", data.len());
    exit(0);
}
```

- [ ] **Step 2: Build the fixture**

Run: `cd rust && cargo build -p vfs-fixture-read && cd ..`
Expected: builds clean.

- [ ] **Step 3: Add the Rust golden config vector (failing Clojure test target)**

In `rust/crates/xtask-descriptor/src/lib.rs`, add a `vfs-shim` path dependency (in its `Cargo.toml`: `vfs-shim = { path = "../vfs-shim" }`), and add to `golden_vectors()`:
```rust
        ("shim-config-root-runtime-empty-snapshot",
         vfs_shim::encode_config(r"C:\GameLayers\runtime", &[])),
```
(If `vfs-shim` pulls Windows-only deps that break the ubuntu `cargo test -p xtask-descriptor` build, instead hardcode the expected bytes here by constructing them inline per the documented format — root_len, root, overlay_len 0, `VFS1`, n_static 0, empty snapshot — and add a Rust test asserting `vfs_shim::encode_config(...)==` those bytes, keeping `xtask-descriptor` free of the Windows dep. Prefer whichever keeps the ubuntu build green.)

- [ ] **Step 4: Regenerate golden + verify Rust**

Run:
```bash
bin/regen-protocol
cd rust && cargo test -p xtask-descriptor && cd ..
```
Expected: `resources/protocol-golden.edn` gains `shim-config-root-runtime-empty-snapshot`; `cargo test -p xtask-descriptor` PASS.

- [ ] **Step 5: Write the failing Clojure encoder test**

Create `test/aether/vfs/os/windows/shim_config_test.clj`:
```clojure
(ns aether.vfs.os.windows.shim-config-test
  (:require [clojure.test :refer [deftest is]]
            [clojure.edn :as edn]
            [clojure.java.io :as io]
            [aether.vfs.os.windows.shim-config :as cfg]))

(defn- golden []
  (with-open [r (io/reader (io/resource "protocol-golden.edn"))]
    (into {} (map (juxt :name :bytes)) (:vectors (edn/read (java.io.PushbackReader. r))))))

(defn- hex [^bytes b] (apply str (map #(format "%02x" (bit-and % 0xff)) b)))

(deftest encodes-shim-config-like-rust
  (is (= (:shim-config-root-runtime-empty-snapshot (golden))
         (hex (cfg/encode "C:\\GameLayers\\runtime" (byte-array 0))))))
```

- [ ] **Step 6: Run to verify failure**

Run: `clojure -M:test -n aether.vfs.os.windows.shim-config-test`
Expected: FAIL — `aether.vfs.os.windows.shim-config` not found.

- [ ] **Step 7: Implement the Clojure encoder**

Create `src/aether/vfs/os/windows/shim_config.clj`:
```clojure
(ns aether.vfs.os.windows.shim-config
  "Encodes the VFS_SHIM_CONFIG file the injected shim reads. Mirrors
  vfs-shim::encode_config (encode_config_full with empty overlay + no static
  imports): [u32 root_len][root utf8][u32 overlay_len=0][\"VFS1\"][u32 0][snapshot].
  Pinned byte-for-byte to the Rust golden vector."
  (:import [java.io ByteArrayOutputStream]))

(defn- put-u32! [^ByteArrayOutputStream b v]
  (let [x (long v)] (dotimes [i 4] (.write b (int (bit-and (bit-shift-right x (* 8 i)) 0xff))))))

(defn encode ^bytes [^String root ^bytes snapshot]
  (let [b (ByteArrayOutputStream.)
        rb (.getBytes root "UTF-8")]
    (put-u32! b (alength rb)) (.write b rb 0 (alength rb))
    (put-u32! b 0)                       ; overlay_len = 0
    (.write b (.getBytes "VFS1" "UTF-8") 0 4)
    (put-u32! b 0)                       ; n_static = 0
    (.write b snapshot 0 (alength snapshot))
    (.toByteArray b)))
```

- [ ] **Step 8: Run to verify pass**

Run: `clojure -M:test -n aether.vfs.os.windows.shim-config-test`
Expected: PASS — the Clojure config encoder is byte-identical to the Rust `encode_config`.

- [ ] **Step 9: Commit**

```bash
git add rust/Cargo.toml rust/crates/vfs-fixture-read rust/crates/xtask-descriptor \
  resources/protocol-golden.edn src/aether/vfs/os/windows/shim_config.clj \
  test/aether/vfs/os/windows/shim_config_test.clj
git commit -m "feat(m3): read fixture + Clojure VFS_SHIM_CONFIG encoder (golden-pinned)"
```

---

## Task 3: De-risk spike (controller-run) — pure-ring read through real injection

**Not a subagent TDD task — this is a controller-run environment spike** (like the M1 repo-merge bootstrap): iterative, Windows-specific, and its outcome shapes M3 Part 2. It uses Task 1 (`OP_HEARTBEAT`) and Task 2 (fixture + config encoder) plus the *existing* `vfs-inject`/`vfs-shim`.

**Goal / decision to produce:** does a real process, injected with the existing shim and pointed at a JVM-created section with an **empty-snapshot** config, read bytes served only by a Clojure `Provider`? → **YES** (productionize as-is in Part 2) or **NEEDS TWEAK** (a small fuse-authoritative change so the hook consults the FuseClient for paths under root before the empty snapshot).

- [ ] **Step 1: Determine the existing injector's invocation contract**

Read `rust/crates/vfs-inject/src/lib.rs` (`RunConfig`) and `rust/crates/vfs-inject/src/bin/*.rs` (e.g. `local_run.rs`, `vfs-spawn-child.rs`, `vfs-acceptance.rs`) to find the existing entry that: spawns a target suspended, injects `vfs-shim-dll`, resumes, waits on `VFS_SHIM_READY`. Identify which env vars it honors and how the shim DLL path is passed. Record the exact invocation.

- [ ] **Step 2: Build the shim DLL + injector + fixture**

Run: `cd rust && cargo build -p vfs-shim-dll -p vfs-inject -p vfs-fixture-read && cd ..`
Note the built `vfs_shim_dll.dll` (or `.dll` name) path under `rust/target/debug/`.

- [ ] **Step 3: Wire a crude JVM-driven launch (throwaway Clojure)**

In a REPL / scratch `comment` form (not committed as production), from Clojure on Windows:
- Create a section (`aether.vfs.os.windows.section/create`) of ~4 MiB with a known name; `ring/init` (slot-count 32, payload-cap from the shim default 1 MiB — match `VFS_RING_PAYLOAD_CAP`); make an arena after the ring.
- Write a `VFS_SHIM_CONFIG` temp file via `shim-config/encode` with `root = "C:\\GameLayers\\runtime"` and empty snapshot; create a `VFS_SHIM_READY` temp path.
- Set env for the child: `VFS_RING_SECTION`, `VFS_RING_BYTES`, `VFS_RING_PAYLOAD_CAP`, `VFS_ARENA_LEN`, `VFS_VIRTUAL_DIR=C:\GameLayers\runtime`, `VFS_SHIM_CONFIG`, `VFS_SHIM_READY`, plus the fixture's `VFS_FIXTURE_PATH=C:\GameLayers\runtime\hello.txt`, `VFS_FIXTURE_EXPECT=5`, `VFS_FIXTURE_FILL` (for a bulk file).
- Start `server/serve` on a daemon thread with an inline `Provider` serving `/hello.txt` = "hello" (and a `>64 KiB` `/big.bin`).
- Invoke the existing injector (per Step 1) with the target = `vfs-fixture-read.exe` and the shim DLL, inheriting that env.
- Wait for the fixture's exit code.

- [ ] **Step 4: Observe and decide**

Expected on success: the fixture prints `FIXTURE OK` and exits 0 — a real process read `/hello.txt`'s bytes from the JVM `Provider` through the injected shim's `NtCreateFile`/`NtReadFile` hooks. Capture: the fixture exit code + stdout/stderr; whether the shim attached the FuseClient (add temporary logging if needed); and whether the empty-snapshot engine interfered.
- If **YES**: record the exact working invocation + env; Part 2 productionizes it.
- If **NEEDS TWEAK**: identify where in `vfs-shim/src/hook.rs` the create/getattr path consults the engine/snapshot before the FuseClient, and scope a minimal "under root → FuseClient first" change for Part 2.

- [ ] **Step 5: Write the spike findings**

Record in `.superpowers/sdd/m3-spike-findings.md` (gitignored scratch): the working invocation/env, the FuseClient-attach outcome, the empty-snapshot behavior, and the decision (as-is vs the specific shim tweak). This is the input to the M3 Part 2 plan.

---

## Self-Review

**Spec coverage (Part 1 scope):**
- `OP_HEARTBEAT` (spec §"two facts") → Task 1. ✓
- Read fixture (`vfs-fixture-read`) → Task 2. ✓
- Clojure `VFS_SHIM_CONFIG` encoder + cross-language pin → Task 2. ✓
- De-risk spike (spec §"De-risk spike (milestone 1)") → Task 3. ✓
- Load-safety (lazy FFM) constraint carried in Global Constraints (shim-config.clj has no native lookups, so it's inherently load-safe on Linux). ✓
- Part 2 items (generic injector extraction, `launch.clj`, end-to-end proof + CI, handshake) → deliberately deferred until the spike confirms the mechanism; planned after Task 3.

**Placeholder scan:** Tasks 1–2 have complete code + exact commands + expected output. Task 3 is explicitly a controller-run investigation with concrete steps and a decision output (not a TODO) — its per-step specifics (exact injector invocation) are discovered in Step 1 by design, which is the nature of a spike, not a placeholder.

**Type consistency:** `server/dispatch` heartbeat (Task 1) matches the test; `cfg/encode [root snapshot]` (Task 2) matches the test and the Rust `encode_config` format; the golden vector name `shim-config-root-runtime-empty-snapshot` matches between the Rust emitter and the Clojure test.

**Note for the executor:** Task 3 is run by the controller, not dispatched to a subagent. Tasks 1 and 2 are the subagent TDD tasks.

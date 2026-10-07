# M3 Part 2 — Launch Productionization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Productionize the JVM-driven injection read path that the Part 1 spike proved: a Clojure `launch.clj` that creates the ring section, generates the shim config (root + valid empty-tree snapshot), sets the env, invokes a generic Rust injector, serves a `Provider` over the ring, and tears down — validated by a Windows-only end-to-end test (real injected shim reads inline + bulk files) wired into CI.

**Architecture:** The Part 1 spike (`spike_driver.clj`, a local throwaway) is the reference. Part 2 turns it into: (1) a golden-pinned empty-tree-snapshot accessor; (2) a formalized generic injector (`vfs-injector`, from the proven `vfs-spike-inject`); (3) `aether.vfs.os.windows.launch`; (4) the e2e proof + CI. **No shim changes** (the spike showed pure-ring works as-is). `server.clj` already has `OP_HEARTBEAT` (Part 1 Task 1) and `norm-vpath` (Part 1 spike).

**Tech Stack:** Clojure 1.12 + deps.clj, Java 26, `java.lang.foreign`; Rust (`vfs-inject`, `vfs-shim-dll`, `vfs-payload`, `xtask-descriptor`, `vfs-core`, `vfs-shared`); GitHub Actions.

## Global Constraints

- OS-specific Clojure under `src/aether/vfs/os/windows/`. Any namespace with native/FFM lookups defers them (lazy) so the full `clojure -M:test` loads on Linux (M2 `section.clj` lesson). `launch.clj` only calls `section.clj` (already lazy) — it must not add its own load-time native calls.
- Nothing hardcodes a wire/layout constant — read from `aether.vfs.protocol`. Any bytes compared cross-language get a golden vector (anti-drift).
- The shim's env contract (set by the JVM for the child; the shim's `fuse_client`/`bootstrap` read these): `VFS_RING_SECTION`, `VFS_RING_BYTES`, `VFS_RING_PAYLOAD_CAP`, `VFS_ARENA_OFFSET`, `VFS_ARENA_LEN`, plus the injector's `VFS_SHIM_CONFIG`/`VFS_SHIM_READY` (set by `run_target_with_shim`). `VFS_VIRTUAL_DIR` is stripped by `run_target_with_shim` → the shim uses its default root `C:\GameLayers\runtime`; use that root.
- `VFS_SHIM_CONFIG` must carry a **valid empty-tree snapshot** (spike finding: zero bytes → `Engine::new` fails → ready timeout).
- Proven geometry (spike): payload_cap 65536, slot_count 8, arena after the ring. Keep geometry in ONE place (launch opts) and derive both `ring/init` and the env from it.
- `resources/*.edn` stay `eol=lf` (`.gitattributes`). Commit bodies end with `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`. ≥1 commit per task.

---

## File Structure

- `rust/crates/xtask-descriptor/{Cargo.toml,src/lib.rs}` — MODIFY: add `vfs-core`+`vfs-shared` deps; emit an `empty-tree-snapshot` golden vector.
- `resources/protocol-golden.edn` — regenerated.
- `src/aether/vfs/os/windows/shim_config.clj` — MODIFY: add `empty-tree-snapshot` accessor (bytes, pinned to golden).
- `test/aether/vfs/os/windows/shim_config_test.clj` — MODIFY: pin the accessor to golden.
- `rust/crates/vfs-inject/src/bin/vfs-injector.rs` — NEW (rename/formalize of `vfs-spike-inject.rs`); delete the spike bin.
- `rust/crates/vfs-inject/tests/injector_args.rs` — NEW: arg-parse unit test for the injector.
- `src/aether/vfs/os/windows/launch.clj` — NEW: the launcher.
- `test/aether/vfs/os/windows/launch_test.clj` — NEW: Windows-only e2e (inline + bulk read through real injection); self-skips off Windows / when artifacts missing.
- `.github/workflows/ci.yml` — MODIFY: `windows-clojure` job builds the injector + shim + payload + fixture and runs `launch-test`.

**Dependency order:** Task 1 (snapshot golden) → Task 2 (injector) → Task 3 (launch.clj, uses 1+2) → Task 4 (e2e proof + CI).

---

## Task 1: empty-tree snapshot golden + Clojure accessor

TDD, cross-platform. The JVM needs a valid empty-tree snapshot for `VFS_SHIM_CONFIG`; pin it to a Rust golden emitted from the real `vfs_shared::bridge::flatten`.

**Files:**
- Modify: `rust/crates/xtask-descriptor/Cargo.toml`, `rust/crates/xtask-descriptor/src/lib.rs`, `resources/protocol-golden.edn`, `src/aether/vfs/os/windows/shim_config.clj`, `test/aether/vfs/os/windows/shim_config_test.clj`

**Interfaces:**
- Produces: golden vector `empty-tree-snapshot`; `aether.vfs.os.windows.shim-config/empty-tree-snapshot -> ^bytes` (the valid empty-tree snapshot, byte-identical to golden).
- Consumes: `vfs_core::{build, Layer, LayerId}`, `vfs_shared::bridge::flatten` (both portable — only `blake3`; safe for the ubuntu `cargo test -p xtask-descriptor` build).

- [ ] **Step 1: Add deps + emit the golden vector**

In `rust/crates/xtask-descriptor/Cargo.toml` `[dependencies]`, add:
```toml
vfs-core = { path = "../vfs-core" }
vfs-shared = { path = "../vfs-shared", features = ["vfs-core"] }
```
(If `vfs-shared`'s vfs-core-enabling feature has a different name, use whatever `vfs-shared/Cargo.toml` declares — it gates `bridge::flatten`. Verify `cargo build -p xtask-descriptor` still works on this box, and that neither pulls a Windows-only crate — confirmed earlier they don't.)

In `rust/crates/xtask-descriptor/src/lib.rs`, add to `golden_vectors()`:
```rust
        ("empty-tree-snapshot", {
            use vfs_core::{build, Layer, LayerId};
            let tree = build(vec![Layer { id: LayerId(0), entries: vec![] }]).unwrap();
            vfs_shared::bridge::flatten(&tree)
        }),
```

- [ ] **Step 2: Regenerate golden + verify Rust**

Run:
```bash
bin/regen-protocol
cd rust && cargo test -p xtask-descriptor && cd ..
```
Expected: `resources/protocol-golden.edn` gains `empty-tree-snapshot` (a 128-byte vector, hex starting `53534656…`); `cargo test -p xtask-descriptor` PASS.

- [ ] **Step 3: Write the failing Clojure test**

Add to `test/aether/vfs/os/windows/shim_config_test.clj`:
```clojure
(deftest empty-tree-snapshot-matches-golden
  (is (= (:empty-tree-snapshot (golden))
         (hex (cfg/empty-tree-snapshot)))))
```

- [ ] **Step 4: Run to verify failure**

Run: `clojure -M:test -n aether.vfs.os.windows.shim-config-test`
Expected: FAIL — `cfg/empty-tree-snapshot` unresolved.

- [ ] **Step 5: Implement the accessor**

Add to `src/aether/vfs/os/windows/shim_config.clj`:
```clojure
(defn- hex->bytes ^bytes [^String h]
  (byte-array (for [i (range 0 (count h) 2)]
                (unchecked-byte (Integer/parseInt (subs h i (+ i 2)) 16)))))

(def ^:private empty-tree-snapshot-hex
  ;; vfs_shared::bridge::flatten(vfs_core::build([Layer{0,[]}])) — a valid empty
  ;; tree. VFS_SHIM_CONFIG requires a valid snapshot (an empty byte-array fails
  ;; the shim's Engine::new). Pinned byte-for-byte to the golden vector.
  (str "5353465601000000000000000000000080000000000000000100000030000000"
       "0000000080000000000000008000000000000000000000008000000000000000"
       "0000000000000000800000000000000000000000000000000000000000000000"
       "0000000000000000000000000000000000000000000000000000000000000000"))

(defn empty-tree-snapshot ^bytes [] (hex->bytes empty-tree-snapshot-hex))
```
(If the golden's hex differs from the above, use the golden's value — the test is the contract. The constant is fine because the empty tree is fixed; the golden pin catches any format change.)

- [ ] **Step 6: Run to verify pass**

Run: `clojure -M:test -n aether.vfs.os.windows.shim-config-test`
Expected: PASS (config-encoder test + the new snapshot test).

- [ ] **Step 7: Commit**

```bash
git add rust/crates/xtask-descriptor resources/protocol-golden.edn \
  src/aether/vfs/os/windows/shim_config.clj test/aether/vfs/os/windows/shim_config_test.clj
git commit -m "feat(m3): empty-tree snapshot golden + Clojure accessor (valid VFS_SHIM_CONFIG snapshot)"
```

---

## Task 2: formalize the generic injector (`vfs-injector`)

Rename the proven `vfs-spike-inject` to `vfs-injector` and add an arg-parse unit test. Logic is unchanged (it works — spike-verified).

**Files:**
- Create: `rust/crates/vfs-inject/src/bin/vfs-injector.rs`, `rust/crates/vfs-inject/tests/injector_args.rs`
- Delete: `rust/crates/vfs-inject/src/bin/vfs-spike-inject.rs`

**Interfaces:**
- Produces: bin `vfs-injector <target_exe> <shim_dll> <payload_dll> <config_file> <ready_file> [-- target_args...]` — wraps `run_target_with_shim` (dual-layer), inherits env to the child, exits with the target's exit code (or 2 on usage error, 3 on inject error).

- [ ] **Step 1: Create the formalized bin**

Create `rust/crates/vfs-inject/src/bin/vfs-injector.rs` with the same body as `vfs-spike-inject.rs` (see `git show` of the Part-1 spike commit for the exact code), but: rename the log prefix `[spike-inject]` → `[vfs-injector]`, and extract the CLI parse into a testable fn:
```rust
/// Parse argv into (target, shim, payload, config, ready, target_args), or Err(usage).
pub fn parse_args(a: &[String]) -> Result<(String, String, String, String, String, Vec<String>), String> {
    if a.len() < 6 {
        return Err("usage: vfs-injector <target> <shim_dll> <payload_dll> <config> <ready> [-- args...]".into());
    }
    let target_args = if a.len() > 6 && a[6] == "--" { a[7..].to_vec() } else { a[6..].to_vec() };
    Ok((a[1].clone(), a[2].clone(), a[3].clone(), a[4].clone(), a[5].clone(), target_args))
}
```
`main` calls `parse_args(&std::env::args().collect::<Vec<_>>())`, then builds the same `RunConfig` and calls `run_target_with_shim` (ready_timeout 20s, `detach:false`, `preinit_redirects: vec![]`, `target_pe_bytes: None`).

- [ ] **Step 2: Delete the spike bin**

```bash
git rm rust/crates/vfs-inject/src/bin/vfs-spike-inject.rs
```

- [ ] **Step 3: Write the arg-parse test**

Create `rust/crates/vfs-inject/tests/injector_args.rs`:
```rust
// Exercises the injector CLI parse via a re-declaration mirror is fragile; instead
// test through the bin is Windows-launch-heavy. Keep this a pure parse test by
// making parse_args reachable: declare it in a small module included by the bin.
```
Because a `[[bin]]` isn't importable, move `parse_args` into `vfs-inject`'s lib (`src/inject.rs` or a new `src/cli.rs`, re-exported) and have the bin call `vfs_inject::parse_injector_args`. Then the test:
```rust
use vfs_inject::parse_injector_args;

#[test]
fn parses_positional_and_double_dash_args() {
    let a: Vec<String> = ["prog","t.exe","s.dll","p.dll","c.cfg","r.flag","--","x","y"]
        .iter().map(|s| s.to_string()).collect();
    let (t,s,p,c,r,args) = parse_injector_args(&a).unwrap();
    assert_eq!((t.as_str(),s.as_str(),p.as_str(),c.as_str(),r.as_str()), ("t.exe","s.dll","p.dll","c.cfg","r.flag"));
    assert_eq!(args, vec!["x".to_string(),"y".to_string()]);
}

#[test]
fn rejects_too_few_args() {
    assert!(parse_injector_args(&["prog".into(),"t".into()]).is_err());
}
```
(Move `parse_injector_args` to the lib so both the bin and the test use it — the bin becomes a thin `main` calling the lib fn + `run_target_with_shim`.)

- [ ] **Step 4: Build + test**

Run:
```bash
cd rust && cargo build -p vfs-inject --bin vfs-injector && cargo test -p vfs-inject --test injector_args && cd ..
```
Expected: bin builds; 2 arg-parse tests pass.

- [ ] **Step 5: Commit**

```bash
git add rust/crates/vfs-inject
git commit -m "refactor(m3): formalize generic injector vfs-injector (+ arg-parse test)"
```

---

## Task 3: `launch.clj` — the productionized launcher

The Clojure entry that does what `spike_driver.clj` proved, cleanly.

**Files:**
- Create: `src/aether/vfs/os/windows/launch.clj`

**Interfaces:**
- Produces: `aether.vfs.os.windows.launch/launch [provider opts] -> int` where `opts` = `{:target-exe, :target-args, :injector, :shim-dll, :payload, :root (default "C:\\GameLayers\\runtime"), :payload-cap (default 65536), :slot-count (default 8), :arena-len (default (* 4 1024 1024)), :ready-timeout-ms, :child-env}`. Creates the section, serves `provider`, injects+launches `target-exe`, waits, tears down, returns the target's exit code.
- Consumes: `aether.vfs.os.windows.{section,ring,arena,server,shim-config}`, `clojure.java.io`.

- [ ] **Step 1: Implement `launch.clj`**

Create `src/aether/vfs/os/windows/launch.clj` (productionized from the proven spike; geometry in one place; teardown in `finally`):
```clojure
(ns aether.vfs.os.windows.launch
  "Windows launcher: creates a JVM ring section, serves a Provider, injects the
  shim into a target (via the generic vfs-injector, dual-layer), and returns the
  target's exit code. Proven by the Part 1 spike: a real process reads
  Provider-served bytes through the injected shim's NtCreateFile/NtReadFile hooks.
  Windows-only (uses os/windows/section FFM)."
  (:require [aether.vfs.os.windows.section :as section]
            [aether.vfs.os.windows.ring :as ring]
            [aether.vfs.os.windows.arena :as arena]
            [aether.vfs.os.windows.server :as server]
            [aether.vfs.os.windows.shim-config :as cfg]
            [clojure.java.io :as io])
  (:import [java.lang.foreign MemorySegment]))

(defn- align8 ^long [^long n] (bit-and (+ n 7) (bit-not 7)))

(defn launch
  "Launch target-exe with the shim injected, serving `provider` over the ring.
  Returns the target's exit code. Windows-only."
  ^long [provider {:keys [target-exe target-args injector shim-dll payload root
                          payload-cap slot-count arena-len ready-timeout-ms child-env]
                   :or {root "C:\\GameLayers\\runtime" payload-cap 65536 slot-count 8
                        arena-len (* 4 1024 1024) target-args [] child-env {}}}]
  (let [stride (align8 (+ 32 (long payload-cap)))
        ring-bytes (+ 40 (* (long slot-count) stride))
        arena-off ring-bytes
        size (+ ring-bytes (long arena-len))
        nm (str "Local\\vfs-m3-" (.pid (java.lang.ProcessHandle/current)) "-" (System/nanoTime))
        sec (section/create nm size)
        seg (:segment sec)
        geom (ring/init seg slot-count payload-cap)
        a (arena/make seg arena-off arena-len slot-count)
        stop? (atom false)
        server-thread (doto (Thread. #(server/serve seg geom a provider stop?))
                        (.setDaemon true) (.start))
        tmp (System/getProperty "java.io.tmpdir")
        cfg-file (str tmp "vfs-m3-" (System/nanoTime) ".cfg")
        ready-file (str tmp "vfs-m3-" (System/nanoTime) ".ready")]
    (try
      (with-open [o (io/output-stream cfg-file)]
        (.write o ^bytes (cfg/encode root (cfg/empty-tree-snapshot))))
      (let [cmd (into [injector target-exe shim-dll payload cfg-file ready-file]
                      (when (seq target-args) (into ["--"] target-args)))
            pb (ProcessBuilder. ^java.util.List cmd)
            ^java.util.Map env (.environment pb)]
        (.put env "VFS_RING_SECTION" nm)
        (.put env "VFS_RING_BYTES" (str size))
        (.put env "VFS_RING_PAYLOAD_CAP" (str payload-cap))
        (.put env "VFS_ARENA_OFFSET" (str arena-off))
        (.put env "VFS_ARENA_LEN" (str arena-len))
        (doseq [[k v] child-env] (.put env (str k) (str v)))
        (.inheritIO pb)
        (let [proc (.start pb)]
          (.waitFor proc)))
      (finally
        (reset! stop? true)
        (try (section/close! sec) (catch Throwable _))
        (try (io/delete-file cfg-file true) (catch Throwable _))
        (try (io/delete-file ready-file true) (catch Throwable _))))))
```

- [ ] **Step 2: Verify it loads on any OS (load-safety)**

Run: `clojure -M -e "(require 'aether.vfs.os.windows.launch) (println :loaded)"`
Expected: prints `:loaded` (no native call at load time — `section.clj`'s kernel32 lookup is lazy, `launch` doesn't touch it until called).

- [ ] **Step 3: Commit**

```bash
git add src/aether/vfs/os/windows/launch.clj
git commit -m "feat(m3): launch.clj — JVM section + serve + inject + teardown"
```

---

## Task 4: end-to-end proof + `windows-clojure` CI

The M3 acceptance proof: a real injected process reads inline + bulk files served by a Clojure `Provider`, via `launch.clj`.

**Files:**
- Create: `test/aether/vfs/os/windows/launch_test.clj`
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: `launch`, `providers.inline`; the built `vfs-injector.exe`, `vfs_shim_dll.dll`, `vfs_payload.dll`, `vfs-fixture-read.exe` under `rust/target/debug/`.

- [ ] **Step 1: Write the e2e test**

Create `test/aether/vfs/os/windows/launch_test.clj`:
```clojure
(ns aether.vfs.os.windows.launch-test
  (:require [clojure.test :refer [deftest is]]
            [clojure.java.io :as io]
            [aether.vfs.os.windows.launch :as launch]
            [aether.vfs.providers.inline :as inline]))

(def ^:private windows?
  (.startsWith (.toLowerCase (System/getProperty "os.name")) "windows"))

(def ^:private rd "rust/target/debug/")
(defn- artifact [n] (io/file (str rd n)))
(def ^:private artifacts ["vfs-injector.exe" "vfs_shim_dll.dll" "vfs_payload.dll" "vfs-fixture-read.exe"])

(defn- run-fixture [files fixture-path expect-len fill]
  (launch/launch (inline/inline-provider files)
                 {:target-exe (.getPath (artifact "vfs-fixture-read.exe"))
                  :injector   (.getPath (artifact "vfs-injector.exe"))
                  :shim-dll   (.getPath (artifact "vfs_shim_dll.dll"))
                  :payload    (.getPath (artifact "vfs_payload.dll"))
                  :child-env  (cond-> {"VFS_FIXTURE_PATH" fixture-path
                                       "VFS_FIXTURE_EXPECT" (str expect-len)}
                                fill (assoc "VFS_FIXTURE_FILL" (str fill)))}))

(deftest injected-read-inline-and-bulk
  (cond
    (not windows?) (println "skip: launch-test is Windows-only")
    (not (every? #(.exists (artifact %)) artifacts))
    (println "skip: build rust artifacts first (cargo build -p vfs-inject --bin vfs-injector -p vfs-shim-dll -p vfs-payload -p vfs-fixture-read)")
    :else
    (do
      ;; inline read: /hello.txt = "hello" (5 bytes)
      (is (= 0 (run-fixture [["/hello.txt" (.getBytes "hello" "UTF-8") 0644]]
                            "C:\\GameLayers\\runtime\\hello.txt" 5 nil))
          "injected process read the inline virtual file from the Provider")
      ;; bulk read: /big.bin = 70000 bytes of 'X' (>64KiB → arena zero-copy path)
      (is (= 0 (run-fixture [["/big.bin" (byte-array 70000 (byte 88)) 0644]]
                            "C:\\GameLayers\\runtime\\big.bin" 70000 88))
          "injected process read the bulk virtual file (arena path) from the Provider"))))
```

- [ ] **Step 2: Build artifacts + run the e2e (Windows dev box)**

Run:
```bash
cd rust && cargo build -p vfs-inject --bin vfs-injector -p vfs-shim-dll -p vfs-payload -p vfs-fixture-read && cd ..
clojure -M:test -n aether.vfs.os.windows.launch-test
```
Expected: PASS — both assertions (inline + bulk) exit 0. (This is the M3 milestone: a real injected process reading Provider-served bytes through real hooks, inline and bulk.)

- [ ] **Step 3: Wire into the `windows-clojure` CI job**

In `.github/workflows/ci.yml`, in the `windows-clojure` job: add the injector/shim/payload/fixture to the harness build step and add `launch-test` to the test `-n` list:
```yaml
      - name: Build ring harness + injection artifacts
        run: cargo build -p vfs-ring-harness -p vfs-inject --bin vfs-injector -p vfs-shim-dll -p vfs-payload -p vfs-fixture-read
        working-directory: rust
```
and extend the test step:
```yaml
        run: clojure -M:test -n aether.vfs.os.windows.section-test -n aether.vfs.os.windows.integration-test -n aether.vfs.os.windows.launch-test
```
(Keep the existing `shell: pwsh`, the deps.clj install step, and `JDK_JAVA_OPTIONS`.)

- [ ] **Step 4: Commit**

```bash
git add test/aether/vfs/os/windows/launch_test.clj .github/workflows/ci.yml
git commit -m "test(m3): end-to-end injected read (inline+bulk) via launch.clj + windows-clojure CI"
```

---

## Version handshake (note — largely already satisfied)

The shim's `vfs_ipc::ring::open` validates the ring header **magic + version** on connect; the JVM `ring/init` writes `VERSION` from the descriptor. So a shim built against a different ring version already fails to attach (FuseClient never installs) — the version floor is enforced. The descriptor **content-hash** handshake (stretch) is deferred to a future hardening; it is not required for M3's read-path proof. No task here beyond this note; if desired later, stamp the descriptor hash into a reserved header field and have the shim check it.

---

## Self-Review

**Spec coverage (M3 productionization):**
- `launch.clj` (create section + config + env + inject + serve + teardown) → Task 3. ✓
- Empty-tree snapshot generation (spike requirement) → Task 1 (golden-pinned). ✓
- Generic injector (formalized from the proven bin) → Task 2. ✓
- End-to-end proof (inline + bulk through real hooks) + CI → Task 4. ✓
- Version handshake → note (already enforced by `ring::open`; hash stretch deferred). ✓
- No shim changes (spike finding) — honored; nothing in `vfs-shim` is touched. ✓
- Load-safety (lazy native) → Task 3 Step 2 explicitly verifies `launch.clj` loads on any OS. ✓

**Placeholder scan:** Tasks have complete code + exact commands + expected output. Task 2 references the Part-1 spike commit for the exact injector body (a real, in-repo source, not a placeholder) and specifies the one change (move `parse_args` to the lib for testability).

**Type consistency:** `cfg/encode` + `cfg/empty-tree-snapshot` (Task 1) used by `launch.clj` (Task 3); `launch/launch` signature (Task 3) used by `launch-test` (Task 4); `vfs-injector` CLI (Task 2) matches `launch.clj`'s `cmd` vector and the CI build.

**Note for executor:** Task 4 runs real DLL injection on the Windows dev box; the mechanism is proven (Part 1 spike), so it should pass once artifacts are built. If the bulk read fails, check the arena geometry (payload_cap/slot_count/arena_len) is consistent between `launch.clj` and the env — the spike used payload_cap 65536, slot_count 8.

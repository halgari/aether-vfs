# M5 — Unified Entry + Packaging + Docs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** One public namespace `aether.vfs` exposing `run` — execute a target program inside a `Provider`-backed VFS (Windows injection, Linux FUSE mount+spawn+teardown), with override+bundled Windows-artifact resolution, a tools.build jar bundling those artifacts, per-OS e2e in CI, and docs.

**Architecture:** `aether.vfs/run` detects the OS and delegates via `requiring-resolve` (never eagerly loading the other OS's adapter): Windows → `os/windows/launch/launch` with artifacts from `os/windows/artifacts/resolve!`; Linux → new `os/linux/launch/run` (mount → spawn from mountpoint → wait → unmount). No admin, no mount namespaces.

**Tech Stack:** Clojure (deps.edn, tools.build), jnr-fuse, existing Rust injection artifacts, GitHub Actions.

## Global Constraints

- **Temp paths in tests AND impl MUST be built with `java.io.File`/`io/file` join, never `(str tmpdir …)`** — Linux `java.io.tmpdir` has no trailing separator (bit M4 repeatedly).
- `.clj` files with Windows path backslashes: use Write/Edit tools, never bash heredocs (heredocs collapse `\\`→`\`).
- Binaries stay OUT of git: `resources/native/` is gitignored; build.clj + CI populate it.
- `aether.vfs` must have NO `:require` on either OS adapter (use `requiring-resolve`) so loading it on either OS never pulls in the other's native deps.
- Commit bodies end with `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`. ≥1 commit/task.
- Existing `os/windows/launch/launch` is unchanged (it already accepts `:injector`/`:shim-dll`/`:payload`/`:target-exe`/`:target-args`/`:child-env`/`:root`); `run` supplies resolved paths.

---

## Task 1: Windows native-artifact resolution

Self-contained, cross-platform-testable. Resolve injector/shim/payload: `:native-dir` opt → `AETHER_VFS_NATIVE_DIR` env → bundled classpath resources extracted to a cache dir.

**Files:**
- Create: `src/aether/vfs/os/windows/artifacts.clj`
- Create test: `test/aether/vfs/os/windows/artifacts_test.clj`
- Create test fixture: `test/native/test/dummy.bin` (a few bytes, any content)

**Interfaces:**
- Produces: `(aether.vfs.os.windows.artifacts/resolve! {:native-dir <string-or-nil>}) => {:injector <path> :shim-dll <path> :payload <path>}`; throws `ex-info` (`:tried [:native-dir :env :bundled]`) when unresolved. Also `(extract-bundled! subdir name ^File cache) => ^File|nil` and `artifact-names` (map).

- [ ] **Step 1: Write the failing test** — `test/aether/vfs/os/windows/artifacts_test.clj`:

```clojure
(ns aether.vfs.os.windows.artifacts-test
  (:require [clojure.test :refer [deftest is]]
            [clojure.java.io :as io]
            [aether.vfs.os.windows.artifacts :as art]))

(defn- temp-dir ^java.io.File []
  (let [d (io/file (System/getProperty "java.io.tmpdir") (str "art-test-" (System/nanoTime)))]
    (.mkdirs d) d))

(deftest native-dir-override-wins
  (let [dir (temp-dir)]
    (doseq [n (vals art/artifact-names)] (spit (io/file dir n) "x"))
    (let [{:keys [injector shim-dll payload]} (art/resolve! {:native-dir (.getPath dir)})]
      (is (= (.getPath (io/file dir (:injector art/artifact-names))) injector))
      (is (.exists (io/file shim-dll)))
      (is (.exists (io/file payload))))))

(deftest native-dir-incomplete-falls-through-to-error
  ;; a dir missing one artifact is not a valid override tier; with no env and no
  ;; bundled native/windows resources (absent on the test classpath) → ex-info.
  (let [dir (temp-dir)]
    (spit (io/file dir (:injector art/artifact-names)) "x") ; only 1 of 3
    (is (thrown? clojure.lang.ExceptionInfo (art/resolve! {:native-dir (.getPath dir)})))))

(deftest extract-bundled-copies-resource-and-size-skips
  (let [cache (temp-dir)
        f1 (art/extract-bundled! "native/test" "dummy.bin" cache)]
    (is (some? f1))
    (is (.exists f1))
    (let [len (.length f1)
          f2 (art/extract-bundled! "native/test" "dummy.bin" cache)] ; second call: size-skip
      (is (= len (.length f2))))))

(deftest extract-bundled-missing-resource-nil
  (is (nil? (art/extract-bundled! "native/test" "does-not-exist.bin" (temp-dir)))))
```

- [ ] **Step 2: Create the test fixture** — write `test/native/test/dummy.bin` with a few bytes (e.g. the ASCII "aether-vfs-dummy"). (`test/` is a classpath root under the `:test` alias, so `io/resource "native/test/dummy.bin"` resolves; it is NOT in the jar.)

- [ ] **Step 3: Run → fail** — `clojure -M:test -n aether.vfs.os.windows.artifacts-test` → FAIL (ns missing).

- [ ] **Step 4: Implement** — `src/aether/vfs/os/windows/artifacts.clj`:

```clojure
(ns aether.vfs.os.windows.artifacts
  "Resolve the three Windows native artifacts the injected-launch path needs.
  Override order: :native-dir opt -> AETHER_VFS_NATIVE_DIR env -> bundled
  classpath resources (native/windows/<name>) extracted to a cache dir. The
  extraction mechanism itself is OS-neutral (so it is unit-tested on any OS)."
  (:require [clojure.java.io :as io])
  (:import [java.io File]))

(def artifact-names
  "Logical artifact key -> on-disk filename."
  {:injector "vfs-injector.exe"
   :shim-dll "vfs_shim_dll.dll"
   :payload  "vfs_payload.dll"})

(defn- from-dir
  "If `dir` holds ALL three artifacts, return the {:injector.. :shim-dll.. :payload..}
  path map; else nil (an incomplete dir is not a valid tier)."
  [^String dir]
  (when dir
    (let [paths (into {} (map (fn [[k n]] [k (io/file dir n)])) artifact-names)]
      (when (every? #(.exists ^File %) (vals paths))
        (into {} (map (fn [[k ^File f]] [k (.getPath f)])) paths)))))

(defn- cache-dir ^File []
  (io/file (System/getProperty "java.io.tmpdir") "aether-vfs-native"))

(defn extract-bundled!
  "Copy classpath resource <subdir>/<name> to <cache>/<name>, unless a same-size
  copy already exists. Returns the extracted File, or nil if the resource is
  absent from the classpath. Marks the file readable+executable."
  (^File [name cache] (extract-bundled! "native/windows" name cache))
  (^File [subdir name ^File cache]
   (when-some [url (io/resource (str subdir "/" name))]
     (.mkdirs cache)
     (let [dest (io/file cache name)
           bytes (with-open [s (.openStream url)] (.readAllBytes s))]
       (when (or (not (.exists dest)) (not= (alength bytes) (.length dest)))
         (io/copy bytes dest))
       (.setReadable dest true)
       (.setExecutable dest true)
       dest))))

(defn resolve!
  "=> {:injector p :shim-dll p :payload p}. :native-dir opt / AETHER_VFS_NATIVE_DIR
  env win (only if the dir holds all three); else bundled resources extracted to
  the cache dir. Throws ex-info if nothing resolves to a complete set."
  [{:keys [native-dir]}]
  (or (from-dir native-dir)
      (from-dir (System/getenv "AETHER_VFS_NATIVE_DIR"))
      (let [cache (cache-dir)
            resolved (into {} (map (fn [[k n]]
                                     [k (some-> (extract-bundled! n cache) .getPath)]))
                           artifact-names)]
        (when (every? some? (vals resolved)) resolved))
      (throw (ex-info "Cannot resolve Windows native artifacts (injector/shim/payload)"
                      {:tried [:native-dir :env :bundled]
                       :native-dir native-dir
                       :names (vals artifact-names)}))))
```

- [ ] **Step 5: Run → pass** — `clojure -M:test -n aether.vfs.os.windows.artifacts-test` → PASS (4 tests).
- [ ] **Step 6: Commit** — `feat(m5): Windows native-artifact resolution (override + bundled fallback)`.

---

## Task 2: Linux launcher (mount + run + teardown)

Linux-only e2e (real FUSE). Generalizes `proton.clj`: mount a Provider, run the target from the mountpoint, wait, unmount. No admin.

**Files:**
- Create: `src/aether/vfs/os/linux/launch.clj`
- Create test: `test/aether/vfs/os/linux/launch_test.clj`

**Interfaces:**
- Consumes: `aether.vfs.os.linux.fuse/mount` (`[provider mountpoint] => Closeable`), `aether.vfs.providers.inline/inline-provider`.
- Produces: `(aether.vfs.os.linux.launch/run provider {:exec [cmd & args] :mountpoint <string-or-nil> :env {}}) => ^long exit-code`.

- [ ] **Step 1: Write the failing test** — `test/aether/vfs/os/linux/launch_test.clj`:

```clojure
(ns aether.vfs.os.linux.launch-test
  (:require [clojure.test :refer [deftest is]]
            [aether.vfs.os.linux.launch :as launch]
            [aether.vfs.providers.inline :as inline]))

(def ^:private linux?
  (.startsWith (.toLowerCase (System/getProperty "os.name")) "linux"))

(deftest run-executes-target-against-mount
  (if-not linux?
    (println "skip: linux/launch-test is Linux-only")
    (let [provider (inline/inline-provider [["/hello.txt" (.getBytes "hello" "UTF-8") 0644]])
          ;; the target reads the virtual file THROUGH the mount via $AETHER_VFS_MOUNT
          exit (launch/run provider
                 {:exec ["sh" "-c" "test \"$(cat \"$AETHER_VFS_MOUNT/hello.txt\")\" = hello"]})]
      (is (= 0 exit) "target saw the Provider-served file through the FUSE mount"))))

(deftest run-requires-exec
  (is (thrown? clojure.lang.ExceptionInfo (launch/run (inline/inline-provider []) {}))))
```

- [ ] **Step 2: Run → fail** — `clojure -M:test -n aether.vfs.os.linux.launch-test` → FAIL (ns missing). (On Windows the first test self-skips; `run-requires-exec` still runs and fails until impl.)

- [ ] **Step 3: Implement** — `src/aether/vfs/os/linux/launch.clj`:

```clojure
(ns aether.vfs.os.linux.launch
  "Linux launcher for the unified aether.vfs/run: mount a Provider at a
  mountpoint (user-space FUSE, no admin), run a target program against it
  (cwd = mountpoint, $AETHER_VFS_MOUNT set), wait, then unmount + clean up a
  temp mountpoint we created. Generalizes the proton.clj pattern."
  (:require [aether.vfs.os.linux.fuse :as fuse]
            [clojure.java.io :as io])
  (:import [java.io Closeable File]))

(defn- fresh-mountpoint ^File []
  (doto (io/file (System/getProperty "java.io.tmpdir") (str "aether-vfs-mnt-" (System/nanoTime)))
    (.mkdirs)))

(defn- wait-ready!
  "The non-blocking FUSE mount runs its loop in a background thread; poll until
  the mountpoint answers a listing (or `timeout-ms` elapses)."
  [^File mountpoint ^long timeout-ms]
  (let [deadline (+ (System/currentTimeMillis) timeout-ms)]
    (loop []
      (cond
        (try (some? (.list mountpoint)) (catch Throwable _ false)) true
        (> (System/currentTimeMillis) deadline) false
        :else (do (Thread/sleep 25) (recur))))))

(defn run
  "Mount `provider` at a mountpoint and run `:exec` inside it. Returns the
  target's exit code. Linux-only (user-space FUSE)."
  ^long [provider {:keys [exec mountpoint env] :or {env {}}}]
  (when-not (seq exec) (throw (ex-info "aether.vfs.os.linux.launch/run: :exec required" {})))
  (let [owned? (nil? mountpoint)
        ^File mp (if mountpoint (io/file mountpoint) (fresh-mountpoint))
        ^Closeable guard (fuse/mount provider (.getPath mp))]
    (try
      (wait-ready! mp 3000)
      (let [pb (ProcessBuilder. ^java.util.List (vec exec))
            e (.environment pb)]
        (.put e "AETHER_VFS_MOUNT" (.getPath mp))
        (doseq [[k v] env] (.put e (str k) (str v)))
        (.directory pb mp)
        (.inheritIO pb)
        (let [proc (.start pb)]
          (try (long (.waitFor proc))
               (finally (when (.isAlive proc) (.destroyForcibly proc))))))
      (finally
        (try (.close guard) (catch Throwable _))
        (when owned? (try (.delete mp) (catch Throwable _)))))))
```

- [ ] **Step 4: Run → pass** — on Linux: `clojure -M:test -n aether.vfs.os.linux.launch-test` → PASS. (Controller note: on the Windows dev box only `run-requires-exec` runs; the mount e2e self-skips and is exercised by the ubuntu CI job.)
- [ ] **Step 5: Commit** — `feat(m5): Linux launcher (FUSE mount + run target + teardown, no admin)`.

---

## Task 3: Unified `aether.vfs/run` + OS dispatch

The public entry. Pure opts-normalization is unit-tested cross-platform; real dispatch is proven by the per-OS e2e (Tasks 2 & 5).

**Files:**
- Create: `src/aether/vfs.clj`
- Create test: `test/aether/vfs_test.clj`

**Interfaces:**
- Consumes (via `requiring-resolve`): `aether.vfs.os.windows.artifacts/resolve!`, `aether.vfs.os.windows.launch/launch`, `aether.vfs.os.linux.launch/run` (Tasks 1 & 2).
- Produces: `(aether.vfs/run provider opts) => ^long exit`; pure helpers `to-windows-opts`/`to-linux-opts`/`os-kind` (for tests).

- [ ] **Step 1: Write the failing test** — `test/aether/vfs_test.clj`:

```clojure
(ns aether.vfs-test
  (:require [clojure.test :refer [deftest is]]
            [aether.vfs :as vfs]))

(deftest os-kind-is-a-known-keyword
  (is (contains? #{:windows :linux :unsupported} (#'vfs/os-kind))))

(deftest to-windows-opts-maps-exec-and-artifacts
  (let [o (#'vfs/to-windows-opts
            {:exec ["game.exe" "--a" "--b"] :env {"K" "V"} :mountpoint "R" :windows {:slot-count 4}}
            {:injector "i" :shim-dll "s" :payload "p"})]
    (is (= "game.exe" (:target-exe o)))
    (is (= ["--a" "--b"] (:target-args o)))
    (is (= {"K" "V"} (:child-env o)))
    (is (= "i" (:injector o)))
    (is (= "R" (:root o)))
    (is (= 4 (:slot-count o))))) ; :windows passthrough merged

(deftest to-linux-opts-passes-exec-env-mount
  (let [o (#'vfs/to-linux-opts {:exec ["sh" "-c" "true"] :env {"K" "V"} :mountpoint "/mnt" :linux {:x 1}})]
    (is (= ["sh" "-c" "true"] (:exec o)))
    (is (= {"K" "V"} (:env o)))
    (is (= "/mnt" (:mountpoint o)))
    (is (= 1 (:x o)))))

(deftest run-requires-exec
  (is (thrown? clojure.lang.ExceptionInfo (vfs/run (reify Object) {}))))
```

- [ ] **Step 2: Run → fail** — `clojure -M:test -n aether.vfs-test` → FAIL (ns missing).

- [ ] **Step 3: Implement** — `src/aether/vfs.clj`:

```clojure
(ns aether.vfs
  "Unified cross-platform entry point. `run` executes a target program inside a
  Provider-backed virtual filesystem, scoped to the program's lifetime:
  FUSE mount on Linux, injected shim on Windows. See the M5 design spec.

  Opts: :exec [cmd & args] (required); :mountpoint (Linux mount dir / Windows
  virtual root); :native-dir (Windows artifacts override, else bundled);
  :env {k v}; :windows {..}/:linux {..} OS-specific passthrough."
  (:require [clojure.string :as str]))

(defn- os-kind []
  (let [n (str/lower-case (System/getProperty "os.name"))]
    (cond (str/starts-with? n "windows") :windows
          (str/starts-with? n "linux")   :linux
          :else :unsupported)))

(defn- to-windows-opts [{:keys [exec mountpoint env windows]} artifacts]
  (merge {:target-exe  (first exec)
          :target-args (vec (rest exec))
          :injector    (:injector artifacts)
          :shim-dll    (:shim-dll artifacts)
          :payload     (:payload artifacts)
          :child-env   (or env {})}
         (when mountpoint {:root mountpoint})
         windows))

(defn- to-linux-opts [{:keys [exec mountpoint env linux]}]
  (merge {:exec exec :mountpoint mountpoint :env (or env {})} linux))

(defn run
  "Run :exec inside a VFS serving `provider`; block until it exits; tear the VFS
  down. Returns the target's exit code (long). Dispatches on the host OS;
  throws ex-info on an unsupported OS.

  NOTE (Windows): the effective virtual root is currently the injected shim's
  default (C:\\GameLayers\\runtime); :mountpoint is passed through but the shim
  root is not yet reconfigurable (tracked follow-up)."
  ^long [provider {:keys [exec native-dir] :as opts}]
  (when-not (seq exec)
    (throw (ex-info "aether.vfs/run: :exec (command vector) is required" {:opts opts})))
  (case (os-kind)
    :windows (let [resolve! (requiring-resolve 'aether.vfs.os.windows.artifacts/resolve!)
                   launch   (requiring-resolve 'aether.vfs.os.windows.launch/launch)]
               (launch provider (to-windows-opts opts (resolve! {:native-dir native-dir}))))
    :linux   (let [lrun (requiring-resolve 'aether.vfs.os.linux.launch/run)]
               (lrun provider (to-linux-opts opts)))
    (throw (ex-info (str "aether.vfs/run is unsupported on this OS: "
                         (System/getProperty "os.name"))
                    {:os (System/getProperty "os.name")}))))
```

- [ ] **Step 4: Run → pass** — `clojure -M:test -n aether.vfs-test` → PASS (4 tests).
- [ ] **Step 5: Commit** — `feat(m5): unified aether.vfs/run entry with OS dispatch`.

---

## Task 4: Packaging (build.clj + :build alias + gitignore)

tools.build: stage Rust release artifacts into resources and build an importable jar.

**Files:**
- Create: `build.clj` (repo root)
- Modify: `deps.edn` (add `:build` alias)
- Modify: `.gitignore` (add `resources/native/`)

**Interfaces:**
- Produces: `clojure -T:build jar` → `target/aether-vfs-<version>.jar` bundling `src`, `resources` (incl. staged `native/windows/*`). `clojure -T:build stage-native` stages artifacts only.

- [ ] **Step 1: Add the `:build` alias** — in `deps.edn`, add under `:aliases`:

```clojure
  :build {:deps {io.github.clojure/tools.build {:mvn/version "0.10.5"}}
          :ns-default build}
```

- [ ] **Step 2: gitignore the staged natives** — append to `.gitignore`:

```
resources/native/
```

- [ ] **Step 3: Write `build.clj`** (repo root):

```clojure
(ns build
  (:require [clojure.java.io :as io]
            [clojure.tools.build.api :as b]))

(def lib 'com.halgari/aether-vfs)
(def version (or (System/getenv "AETHER_VFS_VERSION") "0.1.0-SNAPSHOT"))
(def class-dir "target/classes")
(def jar-file (format "target/%s-%s.jar" (name lib) version))
(def stage-dir "resources/native/windows")
(def artifacts ["vfs-injector.exe" "vfs_shim_dll.dll" "vfs_payload.dll"])

(defn stage-native
  "Build the Windows Rust artifacts (release) and copy them into resources so the
  jar bundles them. Best-effort: warns (does not fail) when cargo or the built
  artifacts are unavailable, so a non-Windows jar build still succeeds."
  [_]
  (let [{:keys [exit]} (try
                         (b/process {:command-args ["cargo" "build" "--release"
                                                    "-p" "vfs-inject" "-p" "vfs-shim-dll"
                                                    "-p" "vfs-payload"]
                                     :dir "rust"})
                         (catch Throwable _ {:exit 1}))]
    (when-not (zero? exit)
      (println "WARN: cargo build --release unavailable/failed; staging whatever exists")))
  (.mkdirs (io/file stage-dir))
  (doseq [n artifacts]
    (let [src (io/file "rust/target/release" n)]
      (if (.exists src)
        (do (b/copy-file {:src (str src) :target (str (io/file stage-dir n))})
            (println "staged" n))
        (println "WARN: missing" (str src) "- skipped")))))

(defn jar
  "Stage natives then build an importable source jar (src + resources)."
  [_]
  (stage-native nil)
  (b/write-pom {:class-dir class-dir :lib lib :version version
                :basis (b/create-basis {:project "deps.edn"}) :src-dirs ["src"]})
  (b/copy-dir {:src-dirs ["src" "resources"] :target-dir class-dir})
  (b/jar {:class-dir class-dir :jar-file jar-file})
  (println "wrote" jar-file))

(defn clean [_] (b/delete {:path "target"}))
```

- [ ] **Step 4: Verify the build** — run `clojure -T:build jar` (on this Windows box it also builds the Rust release artifacts; that is slow but real). Expect `wrote target/aether-vfs-0.1.0-SNAPSHOT.jar` and staged artifact lines. Confirm the jar contains the natives: `jar tf target/aether-vfs-0.1.0-SNAPSHOT.jar | grep native/windows` shows the three files. (If release build is too slow in review, `clojure -T:build stage-native` + inspect `resources/native/windows/` is an acceptable partial check — note it in the report.)
- [ ] **Step 5: Commit** — `build(m5): tools.build jar bundling Windows native artifacts`.

---

## Task 5: End-to-end via `run` + CI wiring + docs (controller-run)

**Controller-run** (needs real Windows injection to verify). Prove the unified entry through real injection (override AND bundled tiers), wire CI to stage the artifacts and run both e2e paths, and document usage.

**Files:**
- Create test: `test/aether/vfs/run_e2e_test.clj` (Windows-gated injection via `vfs/run`)
- Modify: `.github/workflows/ci.yml` (stage step + rely on Task 2's Linux test in the ubuntu suite)
- Create/Modify: `README.md` (usage section)

- [ ] **Step 1: Windows e2e via `run`** — `test/aether/vfs/run_e2e_test.clj`: mirror the existing `launch_test/injected-read-inline-and-bulk` but drive `aether.vfs/run`. Two cases: (a) **override tier** — `{:native-dir "rust/target/debug" :exec [<fixture-read.exe>] :env {"VFS_FIXTURE_PATH" "C:\\GameLayers\\runtime\\hello.txt" "VFS_FIXTURE_EXPECT" "5"}}`, provider = inline `/hello.txt`="hello", assert exit 0; (b) **bundled tier** — copy the 3 debug artifacts into `resources/native/windows/` (in the test's setup, File-joined), then `vfs/run` with NO `:native-dir`, assert exit 0. Windows-gated + artifact-existence skip like the existing launch-test. (The fixture exe / injector / shim / payload live in `rust/target/debug`.)

- [ ] **Step 2: CI — stage native artifacts (windows-clojure job)** — in `.github/workflows/ci.yml`, after the existing "Build ring harness + injection artifacts" step and before the Clojure test run, add:

```yaml
      - name: Stage native artifacts for bundled-resolution test
        shell: pwsh
        working-directory: .
        run: |
          New-Item -ItemType Directory -Force resources/native/windows | Out-Null
          Copy-Item rust/target/debug/vfs-injector.exe   resources/native/windows/
          Copy-Item rust/target/debug/vfs_shim_dll.dll   resources/native/windows/
          Copy-Item rust/target/debug/vfs_payload.dll    resources/native/windows/
```

(The ubuntu job already runs the full Clojure suite, which now includes Task 2's `run-executes-target-against-mount` FUSE e2e — libfuse2 is already installed there per M1. No ubuntu YAML change needed unless the run is scoped; confirm the suite runs `aether.vfs.os.linux.launch-test`.)

- [ ] **Step 3: README usage section** — add a "Running a program inside the VFS" section to `README.md` (create if absent): the `aether.vfs/run` example (a Provider + `:exec`), per-OS behavior (Windows injection vs Linux FUSE mount, no admin), native-artifact resolution (`:native-dir`/`AETHER_VFS_NATIVE_DIR`/bundled), and building the jar (`clojure -T:build jar`). Note the Windows-root follow-up caveat.

- [ ] **Step 4: Verify locally (controller)** — build debug artifacts (`cd rust && cargo build -p vfs-inject -p vfs-shim-dll -p vfs-payload -p vfs-fixture-read`), then `clojure -M:test -n aether.vfs.run-e2e-test` → both cases exit 0 through real injection. Also run `clojure -M:test -n aether.vfs-test aether.vfs.os.windows.artifacts-test` green.
- [ ] **Step 5: Commit** — `test(m5): e2e aether.vfs/run via injection (override + bundled) + CI staging + docs`.

---

## Self-Review

Coverage: artifact resolution (Task 1), Linux launcher (Task 2), unified `run`+dispatch (Task 3), packaging (Task 4), e2e+CI+docs (Task 5) — every spec component mapped. Interfaces are consistent: Task 3 consumes Task 1's `resolve! => {:injector :shim-dll :payload}` and Task 2's `run [provider {:exec :mountpoint :env}]` verbatim; Task 4 stages into `resources/native/windows/` which Task 1's bundled tier and Task 5's tests read. Global constraints (File-join temp paths, no-git-binaries, no eager OS-adapter require, commit trailer) carried into each task. No admin/namespace work. Windows launch unchanged (backward-compatible). Tasks 1–4 subagent; Task 5 controller (needs real injection).

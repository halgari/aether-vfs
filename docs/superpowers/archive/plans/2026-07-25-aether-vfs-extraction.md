# aether-vfs Extraction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port mauvi-clj's `pulsar.vfs` layer + Proton launch runtime into this repo as the standalone `aether.vfs.*` library, mauvi-clj untouched.

**Architecture:** Rename-only port. `Provider`/`ReadInto`/`Writable` protocols are the core contract; generic providers compose under them; one jnr-fuse mount adapter; one Proton process runtime. `pulsar.store.error`'s two helpers fold into `aether.vfs.error`.

**Tech Stack:** Clojure 1.12.0, jnr-fuse 0.5.8, cognitect test-runner. JDK with FFM (`--enable-native-access=ALL-UNNAMED` in the :test alias).

## Global Constraints

- **Source repo is read-only:** copy FROM `/home/tbaldrid/oss/mauvi-clj`; NEVER modify anything there.
- **Rename table** (apply to every copied file — ns forms, requires, aliased calls):
  - `pulsar.vfs.X` → `aether.vfs.X` (files: `src/pulsar/vfs/` → `src/aether/vfs/`)
  - `pulsar.store.error` → `aether.vfs.error` (its `raise`/`category` live there now; alias `error` keeps working)
  - `pulsar.store.test-util` → `aether.vfs.test-util`
  - keyword `:pulsar/error` → `:aether.vfs/error`
  - env `MAUVI_NO_READ_INTO` → `AETHER_VFS_NO_READ_INTO`; `MAUVI_MAX_FUSE_READS` → `AETHER_VFS_MAX_FUSE_READS`; `MAUVI_PROTON_PATH` → `AETHER_VFS_PROTON_PATH`
- **No behavior changes.** Docstring references to mauvi-internal design docs (D66/D67, "Plan-1", "the Rust vfsd/fuser port", "crate") may be trimmed/reworded to stand alone; code is copied verbatim apart from the rename table.
- Test namespaces mirror source: `test/aether/vfs/<name>_test.clj`, ns `aether.vfs.<name>-test`.
- Run tests with `clojure -M:test` (all) or `clojure -M:test -n aether.vfs.<name>-test` (one ns), always from `/home/tbaldrid/oss/aether-vfs`.
- Commit after each task with a conventional message (`feat: …`, `test: …`, `docs: …`).

---

### Task 1: Scaffold (DONE at plan time)

`deps.edn`, `.gitignore`, spec + this plan already exist and are committed by the planning session. Nothing to do.

---

### Task 2: error, types, provider (+ test util)

**Files:**
- Create: `src/aether/vfs/error.clj` ← merge of mauvi `src/pulsar/store/error.clj` (defns `raise`, `category`) and `src/pulsar/vfs/error.clj` (var `errnos`, defn `errno`, macros `on-not-found`, `with-io`)
- Create: `src/aether/vfs/types.clj` ← `src/pulsar/vfs/types.clj`
- Create: `src/aether/vfs/provider.clj` ← `src/pulsar/vfs/provider.clj`
- Create: `test/aether/vfs/test_util.clj` (content below)
- Create: `test/aether/vfs/error_test.clj` ← `test/pulsar/vfs/error_test.clj`
- Create: `test/aether/vfs/types_test.clj` ← `test/pulsar/vfs/types_test.clj`

**Interfaces:**
- Produces: ns `aether.vfs.error` with `(raise category msg)`, `(raise category msg data)`, `(category e)`, `(errno category)`, `(on-not-found expr fallback)`, `(with-io & body)`; ns `aether.vfs.types` (`root`, `child`, `relative`, `parent`, `from-wire`, `o-rdonly` `o-wronly` `o-rdwr` `o-accmode`, `writable?`); ns `aether.vfs.provider` (protocols `Provider`, `ReadInto`, `Writable`; wrappers `create` `unlink` `rename` `mkdir` `rmdir` `truncate`); ns `aether.vfs.test-util` (`tmp-dir`, `error-category`). All later tasks consume these.

- [ ] **Step 1: Write `test/aether/vfs/test_util.clj`:**

```clojure
(ns aether.vfs.test-util)

(defonce ^:private tmp-counter (atom 0))

(defn tmp-dir
  "Unique-per-call temp dir path (not created)."
  []
  (str (System/getProperty "java.io.tmpdir")
       "/aether-vfs-" (.pid (java.lang.ProcessHandle/current))
       "-" (swap! tmp-counter inc)))

(defn error-category
  "Runs thunk; returns the :aether.vfs/error category it throws, or nil if it
  returns normally."
  [thunk]
  (try
    (thunk)
    nil
    (catch clojure.lang.ExceptionInfo e
      (:aether.vfs/error (ex-data e)))))
```

- [ ] **Step 2: Copy the two error test files** (`error_test.clj`, `types_test.clj`) from `test/pulsar/vfs/`, applying the rename table.
- [ ] **Step 3: Run `clojure -M:test -n aether.vfs.error-test -n aether.vfs.types-test`** — expect FAIL (namespaces `aether.vfs.error`/`aether.vfs.types` not found).
- [ ] **Step 4: Write `src/aether/vfs/error.clj`.** Single ns: docstring describes both roles (error categories via ex-info `:aether.vfs/error` + errno mapping for FUSE). Copy `raise`/`category` bodies from `pulsar/store/error.clj` (key becomes `:aether.vfs/error`), then `errnos`/`errno`/`on-not-found`/`with-io` from `pulsar/vfs/error.clj`; the macros call the local `raise`/`category` directly (drop the `pulsar.store.error` require; keep the java.io/java.nio imports). Fully qualify `raise`/`category` inside the macros as `aether.vfs.error/raise` etc. so they expand correctly in consumer namespaces.
- [ ] **Step 5: Copy `types.clj` and `provider.clj`** applying the rename table (`provider.clj` requires `[aether.vfs.error :as error]`).
- [ ] **Step 6: Run the same test command** — expect PASS.
- [ ] **Step 7: Commit:** `feat: core error/types/provider namespaces + test util`

---

### Task 3: router, read-pool, inode

**Files:**
- Create: `src/aether/vfs/router.clj` ← `src/pulsar/vfs/router.clj`
- Create: `src/aether/vfs/read_pool.clj` ← `src/pulsar/vfs/read_pool.clj`
- Create: `src/aether/vfs/inode.clj` ← `src/pulsar/vfs/inode.clj`
- Create: `test/aether/vfs/router_test.clj`, `read_pool_test.clj`, `inode_test.clj` ← same names under `test/pulsar/vfs/`

**Interfaces:**
- Consumes: `aether.vfs.error`, `aether.vfs.provider` (Task 2).
- Produces: `aether.vfs.router` (`router`, `provider-for`), `aether.vfs.read-pool` (`read-pool`, `submit!`, `shutdown!`), `aether.vfs.inode` (used by no other task; ported for the future low-level adapter).

- [ ] **Step 1: Copy the three test files**, rename table applied. Note `router_test.clj` calls `pulsar.store.error/raise` fully qualified inside a `reify` — becomes `aether.vfs.error/raise`.
- [ ] **Step 2: Run `clojure -M:test -n aether.vfs.router-test -n aether.vfs.read-pool-test -n aether.vfs.inode-test`** — expect FAIL (missing namespaces).
- [ ] **Step 3: Copy the three source files**, rename table applied.
- [ ] **Step 4: Re-run** — expect PASS.
- [ ] **Step 5: Commit:** `feat: router, read-pool, inode`

---

### Task 4: providers (fsutil, inline, layered, passthrough, overlay)

**Files:**
- Create: `src/aether/vfs/providers/fsutil.clj`, `inline.clj`, `layered.clj`, `passthrough.clj`, `overlay.clj` ← same names under `src/pulsar/vfs/providers/`
- Create: `test/aether/vfs/providers_test.clj` ← `test/pulsar/vfs/providers_test.clj`
- Create: `test/aether/vfs/overlay_test.clj` ← `test/pulsar/vfs/overlay_test.clj`

**Interfaces:**
- Consumes: Task 2 namespaces (`aether.vfs.{error,types,provider}`), `aether.vfs.test-util`.
- Produces: `aether.vfs.providers.inline/inline-provider`, `aether.vfs.providers.layered/layered-provider`, `aether.vfs.providers.overlay/overlay-provider`, `aether.vfs.providers.passthrough/passthrough-provider`, `aether.vfs.providers.fsutil` (internal helpers).

- [ ] **Step 1: Copy the two test files**, rename table applied (they use `[aether.vfs.test-util :refer [error-category tmp-dir]]`).
- [ ] **Step 2: Run `clojure -M:test -n aether.vfs.providers-test -n aether.vfs.overlay-test`** — expect FAIL.
- [ ] **Step 3: Copy the five provider sources**, rename table applied.
- [ ] **Step 4: Re-run** — expect PASS.
- [ ] **Step 5: Commit:** `feat: inline/layered/overlay/passthrough providers`

---

### Task 5: compose (+ rewritten compose test)

**Files:**
- Create: `src/aether/vfs/compose.clj` ← `src/pulsar/vfs/compose.clj`
- Create: `test/aether/vfs/compose_test.clj` — REWRITTEN (below), not copied: mauvi's version drives `build-data-root`/`build-data-root-over` through its chunk-store SnapshotProvider, which stays in mauvi. Inline providers exercise the same layering semantics.

**Interfaces:**
- Consumes: Task 4 providers; Task 2 namespaces.
- Produces: `aether.vfs.compose` (`build-data-root`, `build-data-root-over`, `build-inline-root`). In compose.clj, the `snapshot` parameter docstrings stay (any read-only Provider slots in); trim the D66/D67 reference per Global Constraints.

- [ ] **Step 1: Write `test/aether/vfs/compose_test.clj`:**

```clojure
(ns aether.vfs.compose-test
  "Mauvi drives these compositions with its chunk-store SnapshotProvider; any
  read-only Provider slots into the same seams — inline providers here."
  (:require [clojure.java.io :as io]
            [clojure.test :refer [deftest is]]
            [aether.vfs.compose :as compose]
            [aether.vfs.provider :as p]
            [aether.vfs.providers.inline :as inline]
            [aether.vfs.providers.passthrough :as passthrough]
            [aether.vfs.test-util :refer [tmp-dir]]
            [aether.vfs.types :as types]))

(defn- fresh-dir []
  (let [d (tmp-dir)]
    (.mkdirs (io/file d))
    d))

(defn- spit-bytes! [path s]
  (io/make-parents (io/file path))
  (spit (io/file path) s))

(deftest overlay-reads-base-and-captures-writes
  (let [base (inline/inline-provider
              [["/Data/Skyrim.esm" (.getBytes "ESM-BYTES") 0644]])
        overrides (fresh-dir)
        root (compose/build-data-root base overrides)
        ;; (a) base file reads through the overlay
        h (p/open-file root "/Data/Skyrim.esm" types/o-rdonly)]
    (is (pos? (alength ^bytes (p/read-at root (:handle h) 0 16))))
    (p/release-handle root (:handle h))
    ;; (b) creating a new file lands in the overrides dir and reads back
    (let [c (p/create root "/new.txt" types/o-wronly 0644)]
      (p/write-at root (:handle c) 0 (.getBytes "hi"))
      (p/release-handle root (:handle c))
      (is (= "hi" (slurp (io/file overrides "new.txt")))))))

(deftest layered-over-passthrough-shows-base-and-mods
  ;; real base-game dir (passthrough bottom)
  (let [base-dir (fresh-dir)
        _ (spit-bytes! (str base-dir "/meshes/base.nif") "BASE")
        _ (spit-bytes! (str base-dir "/shared.txt") "FROM-BASE") ; 9 bytes
        base (passthrough/passthrough-provider base-dir)
        ;; mod winners (top): one mod-only file + a shared path that wins
        mods (inline/inline-provider
              [["/textures/mod.dds" (byte-array 10 (byte 1)) 0644]
               ["/shared.txt" (.getBytes "MOD-WIN") 0644]]) ; 7 bytes
        root (compose/build-data-root-over mods base (fresh-dir))
        ;; (a) base-only file is visible through the passthrough bottom
        h (p/open-file root "/meshes/base.nif" types/o-rdonly)]
    (is (= "BASE" (String. ^bytes (p/read-at root (:handle h) 0 4))))
    (p/release-handle root (:handle h))
    ;; (b) mod-only file is visible from the top layer
    (is (= 10 (:size (p/lookup root "/textures/mod.dds"))))
    ;; (c) shared path: the mod (size 7) wins over the base (size 9)
    (is (= 7 (:size (p/lookup root "/shared.txt"))))))

(deftest inline-root-serves-bytes-and-captures-writes
  (let [overrides (fresh-dir)
        root (compose/build-inline-root
              (inline/inline-provider [["/Plugins.txt" (.getBytes "*iNeed.esp") 0644]])
              overrides)
        ;; (a) the inline file reads back its exact bytes
        h (p/open-file root "/Plugins.txt" types/o-rdonly)]
    (is (= "*iNeed.esp" (String. ^bytes (p/read-at root (:handle h) 0 32))))
    (p/release-handle root (:handle h))
    ;; (b) a new write lands in overrides, not anywhere near the inline map
    (let [c (p/create root "/scratch" types/o-wronly 0644)]
      (p/write-at root (:handle c) 0 (.getBytes "z"))
      (p/release-handle root (:handle c))
      (is (= "z" (slurp (io/file overrides "scratch")))))))
```

NOTE: mauvi's `inline-provider` takes `[[path bytes perm] …]` triples with leading-slash paths (see `providers_test.clj` usage) — verify against the ported `inline.clj` and adjust the literal shape if its constructor differs.

- [ ] **Step 2: Run `clojure -M:test -n aether.vfs.compose-test`** — expect FAIL (no `aether.vfs.compose`).
- [ ] **Step 3: Copy `compose.clj`**, rename table applied.
- [ ] **Step 4: Re-run** — expect PASS.
- [ ] **Step 5: Commit:** `feat: compose roots + store-free compose test`

---

### Task 6: fuse (+ real-mount test)

**Files:**
- Create: `src/aether/vfs/fuse.clj` ← `src/pulsar/vfs/fuse.clj`
- Create: `test/aether/vfs/mount_test.clj` ← `test/pulsar/vfs/mount_test.clj`

**Interfaces:**
- Consumes: `aether.vfs.{error,provider,router,types}`, `aether.vfs.providers.inline` (test).
- Produces: `aether.vfs.fuse/mount` returning a `java.io.Closeable` guard.

- [ ] **Step 1: Copy `mount_test.clj`**, rename table applied (it self-skips when `/dev/fuse` is absent — keep that guard).
- [ ] **Step 2: Run `clojure -M:test -n aether.vfs.mount-test`** — expect FAIL (no `aether.vfs.fuse`).
- [ ] **Step 3: Copy `fuse.clj`**, rename table applied — including `MAUVI_NO_READ_INTO` → `AETHER_VFS_NO_READ_INTO` (line ~28) and `MAUVI_MAX_FUSE_READS` → `AETHER_VFS_MAX_FUSE_READS` (line ~71).
- [ ] **Step 4: Re-run** — expect PASS (this machine has `/dev/fuse`; the test must actually mount, not skip — check the output for the skip message and treat a skip as failure).
- [ ] **Step 5: Commit:** `feat: jnr-fuse mount adapter with FFM zero-copy read path`

---

### Task 7: proton runtime

**Files:**
- Create: `src/aether/vfs/proton.clj` ← the Proton section of mauvi `src/pulsar/steam/launch.clj` (lines 65–144)
- Create: `test/aether/vfs/proton_test.clj` (content below)

**Interfaces:**
- Consumes: nothing from other tasks (pure process/env code).
- Produces: `aether.vfs.proton` with `default-steam-root`, `default-proton-path`, `proton-command`, `launch-proton!`, `drm-failures`, `reached-markers`, `wineserver-path`, `teardown!`.

- [ ] **Step 1: Write `test/aether/vfs/proton_test.clj`:**

```clojure
(ns aether.vfs.proton-test
  (:require [clojure.test :refer [deftest is]]
            [aether.vfs.proton :as proton]))

(deftest proton-command-builds-the-invocation
  (let [cmd (proton/proton-command {:proton "/opt/proton"
                                    :mountpoint "/tmp/mnt"
                                    :exe "SkyrimSE.exe"
                                    :steam-root "/home/u/.local/share/Steam"
                                    :app-id 489830
                                    :compat "/tmp/throwaway-compat"})]
    (is (= "/opt/proton" (:cmd cmd)))
    (is (= ["run" "/tmp/mnt/SkyrimSE.exe"] (:args cmd)))
    (is (= "/tmp/mnt" (:cwd cmd)))
    ;; the throwaway compat dir is used verbatim — never the real prefix
    (is (= "/tmp/throwaway-compat" (get-in cmd [:env "STEAM_COMPAT_DATA_PATH"])))
    (is (= "489830" (get-in cmd [:env "SteamAppId"])))
    (is (= "489830" (get-in cmd [:env "SteamGameId"])))))

(deftest wineserver-lives-beside-proton
  (is (= "/opt/GE-Proton10-34/files/bin/wineserver"
         (proton/wineserver-path "/opt/GE-Proton10-34/proton"))))
```

- [ ] **Step 2: Run `clojure -M:test -n aether.vfs.proton-test`** — expect FAIL.
- [ ] **Step 3: Write `src/aether/vfs/proton.clj`.** Ns docstring: "Run a Windows exe under Proton from a directory (typically an aether-vfs mount): build the invocation, spawn it tracked with logs captured, grep the logs, tear it down." Copy VERBATIM from `pulsar/steam/launch.clj` lines 65–144: `default-steam-root`, `default-proton-path` (env var becomes `AETHER_VFS_PROTON_PATH`), `proton-command`, `launch-proton!`, `drm-fail-re`, `reached-re`, `grep-file`, `grep-dir`, `drm-failures`, `reached-markers`, `wineserver-path`, `sh!`, `teardown!`. Requires: `[clojure.java.io :as io]`; imports: `(java.io File)`. Do NOT bring `locator-path`, `stream-mount!`, `verify-file`, `find-file`, `sha1-hex`, `bytes->hex` (Steam/snapshot-coupled; stay in mauvi).
- [ ] **Step 4: Re-run** — expect PASS.
- [ ] **Step 5: Commit:** `feat: proton launch runtime`

---

### Task 8: README + full verification

**Files:**
- Create: `README.md`

**Interfaces:**
- Consumes: everything; the mount + proton examples must use the real API names from Tasks 2–7.

- [ ] **Step 1: Write `README.md`** with: one-paragraph description (provider-based JVM virtual filesystem, Linux FUSE mount via jnr-fuse with an FFM zero-copy read path, Proton launch runtime; extracted from the mauvi mod manager; a Rust-based Windows counterpart with the same interfaces is planned separately). Sections: **Mount example** (build an `inline-provider`, `fuse/mount` it, read the file back, `.close` the guard — mirror `mount_test.clj`), **Composition** (one paragraph: router/layered/overlay/compose), **Proton example** (`proton-command` → `launch-proton!` → `teardown!`), **Env vars** (the three `AETHER_VFS_*` vars and what they do), **Tests** (`clojure -M:test`; the mount test needs `/dev/fuse`).
- [ ] **Step 2: Run the FULL suite `clojure -M:test`** — expect: all ported namespaces PASS, 0 failures, 0 errors, and the mount test actually mounted (no skip message).
- [ ] **Step 3: Commit:** `docs: README with mount, composition, and proton examples`

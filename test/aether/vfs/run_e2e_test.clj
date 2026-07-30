(ns aether.vfs.run-e2e-test
  "End-to-end proof of the unified entry point aether.vfs/run through REAL
  Windows injection: a target process reads a Provider-served virtual file via
  the injected shim, driven entirely through vfs/run. Covers both native-artifact
  resolution tiers — an explicit :native-dir override and the bundled classpath
  resources (self-staged from the debug build). Windows-only; self-skips (with a
  message) off-Windows or when the Rust artifacts aren't built."
  (:require [clojure.test :refer [deftest is]]
            [clojure.java.io :as io]
            [aether.vfs :as vfs]
            [aether.vfs.providers.inline :as inline]))

(def ^:private windows?
  (.startsWith (.toLowerCase (System/getProperty "os.name")) "windows"))

(def ^:private rd "rust/target/debug/")
(defn- artifact ^java.io.File [n] (io/file (str rd n)))
(def ^:private needed
  ["vfs-injector.exe" "vfs_shim_dll.dll" "vfs_payload.dll" "vfs-fixture-read.exe"])
(defn- artifacts-present? [] (every? #(.exists (artifact %)) needed))

(defn- hello-provider []
  (inline/inline-provider [["/hello.txt" (.getBytes "hello" "UTF-8") 0644]]))

;; the fixture reads C:\GameLayers\runtime\hello.txt (the shim's default root) and
;; exits 0 iff it read the expected 5 bytes.
(def ^:private child-env
  {"VFS_FIXTURE_PATH" "C:\\GameLayers\\runtime\\hello.txt" "VFS_FIXTURE_EXPECT" "5"})

(deftest run-injection-override-tier
  (cond
    (not windows?) (println "skip: run-e2e is Windows-only")
    (not (artifacts-present?))
    (println "skip: build rust artifacts first (cargo build -p vfs-inject -p vfs-shim-dll -p vfs-payload -p vfs-fixture-read)")
    :else
    ;; :native-dir points vfs/run at the debug build's artifacts (tier 1).
    (is (= 0 (vfs/run (hello-provider)
                      {:native-dir (.getPath (io/file rd))
                       :exec [(.getPath (artifact "vfs-fixture-read.exe"))]
                       :env child-env}))
        "vfs/run resolved artifacts via :native-dir, injected, and served the file")))

(deftest run-injection-bundled-tier
  (cond
    (not windows?) (println "skip: run-e2e is Windows-only")
    (not (artifacts-present?))
    (println "skip: build rust artifacts first")
    :else
    ;; Stage the debug artifacts as classpath resources (resources/ is a classpath
    ;; root; gitignored under resources/native/), then run WITHOUT :native-dir so
    ;; resolve! must fall through to the bundled tier (extract to cache). File-joined
    ;; paths per the cross-platform temp-path rule.
    (let [stage (io/file "resources" "native" "windows")]
      (.mkdirs stage)
      (doseq [n ["vfs-injector.exe" "vfs_shim_dll.dll" "vfs_payload.dll"]]
        (io/copy (artifact n) (io/file stage n)))
      (is (= 0 (vfs/run (hello-provider)
                        {:exec [(.getPath (artifact "vfs-fixture-read.exe"))]
                         :env child-env}))
          "vfs/run resolved artifacts from bundled resources (no :native-dir) and injected"))))

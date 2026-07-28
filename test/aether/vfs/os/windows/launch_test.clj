(ns aether.vfs.os.windows.launch-test
  (:require [clojure.test :refer [deftest is]]
            [clojure.java.io :as io]
            [aether.vfs.os.windows.launch :as launch]
            [aether.vfs.providers.inline :as inline]
            [aether.vfs.providers.overlay :as overlay]))

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

(deftest injected-write-and-read-back
  (let [write-artifacts (conj artifacts "vfs-fixture-write.exe")]
    (cond
      (not windows?) (println "skip: launch-test is Windows-only")
      (not (every? #(.exists (artifact %)) write-artifacts))
      (println "skip: build rust artifacts first (incl. vfs-fixture-write)")
      :else
      (let [overrides (str (System/getProperty "java.io.tmpdir") "vfs-m4-launch-" (System/nanoTime))
            _ (.mkdirs (java.io.File. overrides))
            provider (overlay/overlay-provider (inline/inline-provider []) overrides)
            exit (launch/launch provider
                   {:target-exe (.getPath (artifact "vfs-fixture-write.exe"))
                    :injector   (.getPath (artifact "vfs-injector.exe"))
                    :shim-dll   (.getPath (artifact "vfs_shim_dll.dll"))
                    :payload    (.getPath (artifact "vfs_payload.dll"))
                    :child-env  {"VFS_FIXTURE_PATH" "C:\\GameLayers\\runtime\\new.txt"
                                 "VFS_FIXTURE_DATA" "written-through-real-hooks"}})]
        (is (= 0 exit) "injected process created + wrote + read-back a virtual file via the overlay Provider")
        (is (.exists (java.io.File. overrides "new.txt")) "the write copied-up into the overlay overrides dir")))))

(deftest injected-writeset-mkdir-truncate-delete-rename
  (let [writeset-artifacts (conj artifacts "vfs-fixture-writeset.exe")]
    (cond
      (not windows?) (println "skip: launch-test is Windows-only")
      (not (every? #(.exists (artifact %)) writeset-artifacts))
      (println "skip: build rust artifacts first (incl. vfs-fixture-writeset)")
      :else
      ;; File-joined temp overrides dir (java.io.tmpdir has a trailing separator
      ;; on Windows but not on Linux; launch-test is Windows-only but keep the
      ;; safe idiom). Base is empty — every op lands purely in the upper.
      (let [overrides (.getPath (java.io.File. (System/getProperty "java.io.tmpdir")
                                               (str "vfs-m4p2-writeset-" (System/nanoTime))))
            _ (.mkdirs (java.io.File. overrides))
            provider (overlay/overlay-provider (inline/inline-provider []) overrides)
            exit (launch/launch provider
                   {:target-exe (.getPath (artifact "vfs-fixture-writeset.exe"))
                    :injector   (.getPath (artifact "vfs-injector.exe"))
                    :shim-dll   (.getPath (artifact "vfs_shim_dll.dll"))
                    :payload    (.getPath (artifact "vfs_payload.dll"))
                    :child-env  {"VFS_FIXTURE_DIR" "C:\\GameLayers\\runtime"}})]
        (is (= 0 exit)
            "injected process ran mkdir/truncate/delete/rename through the overlay Provider")
        ;; Authoritative proof: the overlay overrides dir on disk reflects each op.
        (is (.isDirectory (java.io.File. overrides "madedir")) "mkdir created a real dir in overrides")
        (is (= 2 (.length (java.io.File. overrides "trunc.bin"))) "truncate shrank the overrides file to 2 bytes")
        (is (not (.exists (java.io.File. overrides "del.txt"))) "delete removed the file from overrides")
        (is (.exists (java.io.File. overrides "ren_b.txt")) "rename produced the target in overrides")
        (is (not (.exists (java.io.File. overrides "ren_a.txt"))) "rename removed the source from overrides")))))

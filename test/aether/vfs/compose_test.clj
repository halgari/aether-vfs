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
    (is (= "ESM-BYTES" (String. ^bytes (p/read-at root (:handle h) 0 16))))
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

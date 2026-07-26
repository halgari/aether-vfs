(ns aether.vfs.providers-test
  "Ports of the inline / passthrough / layered provider tests plus the
  provider-trait read-only defaults."
  (:require [clojure.java.io :as io]
            [clojure.test :refer [deftest is]]
            [aether.vfs.test-util :refer [error-category tmp-dir]]
            [aether.vfs.provider :as p]
            [aether.vfs.providers.inline :as inline]
            [aether.vfs.providers.layered :as layered]
            [aether.vfs.providers.passthrough :as passthrough]
            [aether.vfs.types :as types]))

(defn- mkdirs! [& paths]
  (doseq [path paths]
    (.mkdirs (io/file path))))

(defn- spit-bytes! [path ^bytes b]
  (with-open [out (io/output-stream (io/file path))]
    (.write out b)))

;; ---- provider write defaults ------------------------------------------------

(deftest write-ops-default-to-read-only
  (let [prov (inline/inline-provider [["/f" (.getBytes "x") 0644]])]
    (is (= :read-only (error-category #(p/create prov "/g" 0 0644))))
    (is (= :read-only (error-category #(p/unlink prov "/g"))))
    (is (= :read-only (error-category #(p/rename prov "/g" "/h"))))
    (is (= :read-only (error-category #(p/mkdir prov "/g" 0755))))
    (is (= :read-only (error-category #(p/rmdir prov "/g"))))
    (is (= :read-only (error-category #(p/truncate prov "/g" 0))))))

;; ---- inline ------------------------------------------------------------------

(defn- demo []
  (inline/inline-provider
   [["/game/app.exe" (.getBytes "MZ-binary-bytes") 0755]
    ["/game/data/readme.txt" (.getBytes "hello") 0644]]))

(deftest inline-lookup-file-reports-exact-size-and-perm
  (let [m (p/lookup (demo) "/game/app.exe")]
    (is (= 15 (:size m)))
    (is (= :file (:kind m)))
    (is (= 0755 (:perm m)))
    (is (= :cached (:cache m)))))

(deftest inline-lookup-implicit-dir
  (is (= :dir (:kind (p/lookup (demo) "/game")))))

(deftest inline-lookup-missing-is-not-found
  (is (= :not-found (error-category #(p/lookup (demo) "/nope")))))

(deftest inline-read-returns-byte-slice-at-offset
  (let [d (demo)
        h (p/open-file d "/game/data/readme.txt" types/o-rdonly)]
    (is (= "hello" (String. ^bytes (p/read-at d (:handle h) 0 5))))
    (is (= "ell" (String. ^bytes (p/read-at d (:handle h) 1 3))))
    (is (= "" (String. ^bytes (p/read-at d (:handle h) 100 5))))
    (p/release-handle d (:handle h))))

(deftest inline-readdir-lists-children
  (let [names (set (map :name (p/readdir (demo) "/game")))]
    (is (contains? names "app.exe"))
    (is (contains? names "data"))))

;; ---- passthrough ---------------------------------------------------------------

(defn- backing-fixture []
  (let [dir (tmp-dir)]
    (mkdirs! (str dir "/sub"))
    (spit-bytes! (str dir "/a.txt") (.getBytes "alpha"))
    (spit-bytes! (str dir "/sub/b.txt") (.getBytes "beta"))
    dir))

(deftest passthrough-lookup-and-read-backing-file
  (let [prov (passthrough/passthrough-provider (backing-fixture))
        m (p/lookup prov "/a.txt")]
    (is (= 5 (:size m)))
    (is (= :file (:kind m)))
    (let [h (p/open-file prov "/a.txt" types/o-rdonly)]
      (is (= "alpha" (String. ^bytes (p/read-at prov (:handle h) 0 5))))
      (p/release-handle prov (:handle h)))))

(deftest passthrough-readdir-lists-backing-dir
  (let [names (set (map :name (p/readdir (passthrough/passthrough-provider (backing-fixture)) "/")))]
    (is (contains? names "a.txt"))
    (is (contains? names "sub"))))

(deftest passthrough-lookup-missing-is-not-found
  (is (= :not-found
         (error-category #(p/lookup (passthrough/passthrough-provider (backing-fixture)) "/nope")))))

;; ---- layered ---------------------------------------------------------------------

(defn- layered-fixture []
  (let [top-dir (tmp-dir)
        bot-dir (tmp-dir)]
    (mkdirs! top-dir bot-dir)
    (spit-bytes! (str top-dir "/a.txt") (.getBytes "TOP-A"))
    (spit-bytes! (str top-dir "/shared.txt") (.getBytes "TOP-SHARED"))
    (spit-bytes! (str bot-dir "/b.txt") (.getBytes "BOT-B"))
    (spit-bytes! (str bot-dir "/shared.txt") (.getBytes "BOTTOM"))
    (layered/layered-provider (passthrough/passthrough-provider top-dir)
                              (passthrough/passthrough-provider bot-dir))))

(deftest layered-top-wins-lookup-and-read-on-shared-path
  (let [l (layered-fixture)]
    (is (= 10 (:size (p/lookup l "/shared.txt"))) "TOP-SHARED is 10 bytes, BOTTOM is 6")
    (let [h (p/open-file l "/shared.txt" types/o-rdonly)]
      (is (= "TOP-SHARED" (String. ^bytes (p/read-at l (:handle h) 0 10))))
      (p/release-handle l (:handle h)))))

(deftest layered-falls-through-to-bottom
  (let [l (layered-fixture)]
    (is (= :file (:kind (p/lookup l "/b.txt"))))
    (let [h (p/open-file l "/b.txt" types/o-rdonly)]
      (is (= "BOT-B" (String. ^bytes (p/read-at l (:handle h) 0 5))))
      (p/release-handle l (:handle h)))))

(deftest layered-readdir-unions-both-layers-without-dupes
  (let [names (mapv :name (p/readdir (layered-fixture) "/"))]
    (is (= #{"a.txt" "b.txt" "shared.txt"} (set names)))
    (is (= 3 (count names)) "shared.txt appears once, not twice")))

(deftest layered-missing-in-both-is-not-found
  (is (= :not-found (error-category #(p/lookup (layered-fixture) "/nope")))))

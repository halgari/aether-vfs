(ns aether.vfs.overlay-test
  (:require [clojure.java.io :as io]
            [clojure.test :refer [deftest is]]
            [aether.vfs.test-util :refer [error-category tmp-dir]]
            [aether.vfs.provider :as p]
            [aether.vfs.providers.overlay :as overlay]
            [aether.vfs.providers.passthrough :as passthrough]
            [aether.vfs.types :as types]))

(defn- spit-bytes! [path s]
  (io/make-parents (io/file path))
  (with-open [out (io/output-stream (io/file path))]
    (.write out (.getBytes ^String s))))

(defn- slurp-bytes [path]
  (String. (java.nio.file.Files/readAllBytes (.toPath (io/file path)))))

;; Returns [base-dir upper-dir], base seeded with a.txt + sub/b.txt.
(defn- fixture []
  (let [root (tmp-dir)
        base (str root "/base")
        upper (str root "/upper")]
    (.mkdirs (io/file base "sub"))
    (.mkdirs (io/file upper))
    (spit-bytes! (str base "/a.txt") "ALPHA")
    (spit-bytes! (str base "/sub/b.txt") "BETA")
    [base upper]))

(defn- overlay-of [base upper]
  (overlay/overlay-provider (passthrough/passthrough-provider base) upper))

(deftest lookup-falls-through-to-base
  (let [[base upper] (fixture)
        m (p/lookup (overlay-of base upper) "/a.txt")]
    (is (= 5 (:size m)))
    (is (= :file (:kind m)))))

(deftest upper-shadows-base-on-lookup
  (let [[base upper] (fixture)]
    (spit-bytes! (str upper "/a.txt") "NEWER-AND-LONGER")
    (is (= 16 (:size (p/lookup (overlay-of base upper) "/a.txt"))))))

(deftest whiteout-hides-base-entry
  (let [[base upper] (fixture)]
    (spit-bytes! (str upper "/.wh.a.txt") "")
    (let [ov (overlay-of base upper)
          names (mapv :name (p/readdir ov "/"))]
      (is (= :not-found (error-category #(p/lookup ov "/a.txt"))))
      (is (not-any? #(= "a.txt" %) names) "whiteout should hide a.txt")
      (is (not-any? #(.startsWith ^String % ".wh.") names) "markers must not leak")
      (is (some #(= "sub" %) names)))))

(deftest readdir-merges-upper-and-base
  (let [[base upper] (fixture)]
    (spit-bytes! (str upper "/c.txt") "GAMMA")
    (let [names (set (map :name (p/readdir (overlay-of base upper) "/")))]
      (is (contains? names "a.txt"))
      (is (contains? names "sub"))
      (is (contains? names "c.txt")))))

(deftest read-base-file-through-overlay
  (let [[base upper] (fixture)
        ov (overlay-of base upper)
        o (p/open-file ov "/a.txt" types/o-rdonly)]
    (is (= "ALPHA" (String. ^bytes (p/read-at ov (:handle o) 0 5))))
    (p/release-handle ov (:handle o))))

(deftest create-new-file-lands-in-upper-only
  (let [[base upper] (fixture)
        ov (overlay-of base upper)
        o (p/create ov "/new.txt" types/o-rdwr 0644)]
    (is (= 5 (p/write-at ov (:handle o) 0 (.getBytes "HELLO"))))
    (p/release-handle ov (:handle o))
    (is (= "HELLO" (slurp-bytes (str upper "/new.txt"))))
    (is (not (.exists (io/file base "new.txt"))) "base must not gain the file")))

(deftest writing-base-file-copies-up-and-leaves-base-intact
  (let [[base upper] (fixture)
        ov (overlay-of base upper)
        o (p/open-file ov "/a.txt" types/o-rdwr)]
    (is (= 1 (p/write-at ov (:handle o) 0 (.getBytes "X"))))
    (p/release-handle ov (:handle o))
    (is (= "XLPHA" (slurp-bytes (str upper "/a.txt"))) "copied-up then patched")
    (is (= "ALPHA" (slurp-bytes (str base "/a.txt"))) "base untouched")))

(deftest truncate-copies-up-and-sets-len
  (let [[base upper] (fixture)
        ov (overlay-of base upper)]
    (p/truncate ov "/a.txt" 2)
    (is (= "AL" (slurp-bytes (str upper "/a.txt"))))
    (is (= "ALPHA" (slurp-bytes (str base "/a.txt"))) "base untouched")))

(deftest unlink-base-file-whiteouts-it
  (let [[base upper] (fixture)
        ov (overlay-of base upper)]
    (p/unlink ov "/a.txt")
    (is (= :not-found (error-category #(p/lookup ov "/a.txt"))))
    (is (.exists (io/file upper ".wh.a.txt")) "whiteout marker expected")
    (is (= "ALPHA" (slurp-bytes (str base "/a.txt"))) "base untouched")))

(deftest mkdir-then-rmdir-roundtrip-with-notempty
  (let [[base upper] (fixture)
        ov (overlay-of base upper)]
    (p/mkdir ov "/d" 0755)
    (is (= :dir (:kind (p/lookup ov "/d"))))
    ;; make it non-empty, expect :not-empty on rmdir
    (let [o (p/create ov "/d/f" types/o-rdwr 0644)]
      (p/release-handle ov (:handle o)))
    (is (= :not-empty (error-category #(p/rmdir ov "/d"))))
    (p/unlink ov "/d/f")
    (p/rmdir ov "/d")
    (is (= :not-found (error-category #(p/lookup ov "/d"))))
    (is (= "ALPHA" (slurp-bytes (str base "/a.txt"))) "base untouched")))

(deftest temp-then-rename-over-existing
  (let [[base upper] (fixture)
        ov (overlay-of base upper)
        o (p/create ov "/a.txt.tmp" types/o-rdwr 0644)]
    (p/write-at ov (:handle o) 0 (.getBytes "REPLACED"))
    (p/release-handle ov (:handle o))
    (p/rename ov "/a.txt.tmp" "/a.txt")
    (let [o2 (p/open-file ov "/a.txt" types/o-rdonly)]
      (is (= "REPLACED" (String. ^bytes (p/read-at ov (:handle o2) 0 8))))
      (p/release-handle ov (:handle o2)))
    (is (= "ALPHA" (slurp-bytes (str base "/a.txt"))) "base untouched")
    (is (= :not-found (error-category #(p/lookup ov "/a.txt.tmp"))))))

(deftest rmdir-base-dir-then-mkdir-same-name-is-empty-opaque
  (let [[base upper] (fixture)
        ov (overlay-of base upper)]
    (is (= :not-empty (error-category #(p/rmdir ov "/sub"))) "sub has b.txt")
    (p/unlink ov "/sub/b.txt")
    (p/rmdir ov "/sub")
    (p/mkdir ov "/sub" 0755)
    ;; recreated dir must be empty (base's b.txt must NOT reappear)
    (is (empty? (p/readdir ov "/sub")) "recreated dir should be opaque/empty")
    (is (= "BETA" (slurp-bytes (str base "/sub/b.txt"))) "base untouched")))

(deftest rename-onto-nonempty-base-dir-returns-notempty
  (let [[base upper] (fixture)
        ov (overlay-of base upper)]
    (p/mkdir ov "/src" 0755)
    (let [o (p/create ov "/src/y" types/o-rdwr 0644)]
      (p/release-handle ov (:handle o)))
    ;; /sub exists in base and is non-empty (b.txt) → rename onto it rejected
    (is (= :not-empty (error-category #(p/rename ov "/src" "/sub"))))
    (is (= "BETA" (slurp-bytes (str base "/sub/b.txt"))) "base untouched")))

(deftest wrap-if-requested-nil-returns-base-unchanged
  (let [[base _upper] (fixture)
        got (overlay/wrap-if-requested (passthrough/passthrough-provider base) nil)]
    ;; no upper dir → base passes through; a write op must still be :read-only
    (is (= :read-only (error-category #(p/create got "/x" 0 0644))))))

(deftest wrap-if-requested-some-enables-writes
  (let [[base upper] (fixture)
        got (overlay/wrap-if-requested (passthrough/passthrough-provider base) upper)]
    (p/mkdir got "/d" 0755)
    (is (.isDirectory (io/file upper "d")))))

(deftest rename-dir-onto-emptied-base-dir-is-opaque
  (let [[base upper] (fixture)
        ov (overlay-of base upper)]
    ;; empty out base /sub so the merged view of /sub is empty
    (p/unlink ov "/sub/b.txt")
    ;; build an upper-only source dir with one file
    (p/mkdir ov "/src" 0755)
    (let [o (p/create ov "/src/x" types/o-rdwr 0644)]
      (p/release-handle ov (:handle o)))
    ;; merged /sub is empty so the rename is allowed; base /sub must be masked
    ;; opaque so b.txt does NOT reappear through the renamed dir
    (p/rename ov "/src" "/sub")
    (is (= ["x"] (mapv :name (p/readdir ov "/sub")))
        "renamed dir must not show base's stale b.txt")
    (is (= "BETA" (slurp-bytes (str base "/sub/b.txt"))) "base untouched")))

(deftest rename-dir-with-upper-child-preserves-base-children
  (let [[base upper] (fixture)
        ov (overlay-of base upper)
        ;; give /sub an upper footprint (a new file) so the upper path exists
        o (p/create ov "/sub/c.txt" types/o-rdwr 0644)]
    (p/write-at ov (:handle o) 0 (.getBytes "CEE"))
    (p/release-handle ov (:handle o))
    ;; rename /sub -> /moved: the base child b.txt must survive the move
    (p/rename ov "/sub" "/moved")
    (is (= ["b.txt" "c.txt"] (sort (map :name (p/readdir ov "/moved"))))
        "renamed dir must keep base child b.txt AND upper child c.txt")
    (let [o2 (p/open-file ov "/moved/b.txt" types/o-rdonly)]
      (is (= "BETA" (String. ^bytes (p/read-at ov (:handle o2) 0 4))))
      (p/release-handle ov (:handle o2)))
    ;; source gone, base pristine
    (is (= :not-found (error-category #(p/lookup ov "/sub"))))
    (is (= "BETA" (slurp-bytes (str base "/sub/b.txt"))))))

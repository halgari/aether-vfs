(ns aether.vfs.inode-test
  (:require [clojure.test :refer [deftest is]]
            [aether.vfs.inode :as inode]))

(deftest root-is-ino-1
  (is (= "/" (inode/path-of (inode/table) inode/root-ino))))

(deftest alloc-is-stable-and-reversible
  (let [t (inode/table)
        p "/game/app.exe"
        [t a] (inode/get-or-alloc t p)
        [t b] (inode/get-or-alloc t p)]
    (is (= a b) "same path -> same ino")
    (is (= p (inode/path-of t a)))))

(deftest distinct-paths-get-distinct-inos
  (let [t (inode/table)
        [t a] (inode/get-or-alloc t "/a")
        [_ b] (inode/get-or-alloc t "/b")]
    (is (not= a b))))

(deftest unknown-ino-resolves-to-nil
  (is (nil? (inode/path-of (inode/table) 999))))

(deftest rename-remaps-ino-and-evicts-overwritten-dest
  (let [t (inode/table)
        [t ino-a] (inode/get-or-alloc t "/a")
        ;; plain rename a -> b (b not yet allocated): ino-a now resolves to b
        t (inode/rename-path t "/a" "/b")
        _ (is (= "/b" (inode/path-of t ino-a)))
        ;; fresh source c, then rename c -> b which currently belongs to
        ;; ino-a: c's ino takes over b, the overwritten old dest ino is evicted
        [t ino-c] (inode/get-or-alloc t "/c")
        t (inode/rename-path t "/c" "/b")]
    (is (= "/b" (inode/path-of t ino-c)))
    (is (nil? (inode/path-of t ino-a)))))

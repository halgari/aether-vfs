(ns aether.vfs.types-test
  (:require [clojure.test :refer [deftest is]]
            [aether.vfs.types :as types]))

(deftest root-path-is-slash
  (is (= "/" types/root))
  (is (= "" (types/relative types/root))))

(deftest child-of-root-has-single-slash
  (let [p (types/child types/root "game")]
    (is (= "/game" p))
    (is (= "game" (types/relative p)))))

(deftest nested-child-joins-with-slash
  (let [p (-> types/root (types/child "game") (types/child "app.exe"))]
    (is (= "/game/app.exe" p))
    (is (= "game/app.exe" (types/relative p)))))

(deftest parent-of-root-is-nil
  (is (nil? (types/parent types/root))))

(deftest parent-of-depth1-is-root
  (is (= "/" (types/parent "/a"))))

(deftest parent-of-depth2-is-depth1
  (is (= "/a" (types/parent "/a/b"))))

(deftest parent-of-depth3-is-depth2
  (is (= "/a/b" (types/parent "/a/b/c"))))

(deftest from-wire-roundtrips
  (is (= "/game/app.exe" (types/from-wire "/game/app.exe")))
  (is (= "/" (types/from-wire ""))))

(deftest open-flag-writability
  (is (not (types/writable? types/o-rdonly)))
  (is (types/writable? types/o-wronly))
  (is (types/writable? types/o-rdwr)))

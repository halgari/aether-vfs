(ns aether.vfs.os.windows.shim-config-test
  (:require [clojure.test :refer [deftest is]]
            [clojure.edn :as edn]
            [clojure.java.io :as io]
            [aether.vfs.os.windows.shim-config :as cfg]))

(defn- golden []
  (with-open [r (io/reader (io/resource "protocol-golden.edn"))]
    (into {} (map (juxt :name :bytes)) (:vectors (edn/read (java.io.PushbackReader. r))))))

(defn- hex [^bytes b] (apply str (map #(format "%02x" (bit-and % 0xff)) b)))

(deftest encodes-shim-config-like-rust
  (is (= (:shim-config-root-runtime-empty-snapshot (golden))
         (hex (cfg/encode "C:\\GameLayers\\runtime" (byte-array 0))))))

(deftest empty-tree-snapshot-matches-golden
  (is (= (:empty-tree-snapshot (golden))
         (hex (cfg/empty-tree-snapshot)))))

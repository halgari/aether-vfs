(ns aether.vfs.wire-conformance-test
  (:require [clojure.test :refer [deftest is]]
            [clojure.edn :as edn]
            [clojure.java.io :as io]
            [aether.vfs.wire :as wire]))

(defn- golden []
  (with-open [r (io/reader (io/resource "protocol-golden.edn"))]
    (into {} (map (juxt :name :bytes)) (:vectors (edn/read (java.io.PushbackReader. r))))))

(defn- hex [^bytes b]
  (apply str (map #(format "%02x" (bit-and % 0xff)) b)))

(deftest clojure-encoders-match-rust-golden
  (let [g (golden)]
    (is (= (:open-req-read-skyrim g)      (hex (wire/encode-open-req 1 "Data/Skyrim.esm"))))
    (is (= (:getattr-resp-file-123 g)     (hex (wire/encode-getattr-resp {:found true :is-dir false :size 123 :mtime -7}))))
    (is (= (:read-req-fh7-off10-len4 g)   (hex (wire/encode-read-req {:fh 7 :offset 10 :len 4}))))
    (is (= (:read-resp-abcd g)            (hex (wire/encode-read-resp (.getBytes "abcd")))))
    (is (= (:readdir-resp-two g)          (hex (wire/encode-readdir-resp [{:name "a.esp" :is-dir false :size 10 :mtime 1}
                                                                          {:name "sub"   :is-dir true  :size 0  :mtime 0}]))))
    (is (= (:close-req-99 g)              (hex (wire/encode-close-req 99))))))

(deftest decode-roundtrips
  (is (= {:found true :is-dir false :size 123 :mtime -7}
         (wire/decode-getattr-resp (wire/encode-getattr-resp {:found true :is-dir false :size 123 :mtime -7}))))
  (is (= "abcd" (String. ^bytes (wire/decode-read-resp (wire/encode-read-resp (.getBytes "abcd"))))))
  (is (= [{:name "a.esp" :is-dir false :size 10 :mtime 1}]
         (wire/decode-readdir-resp (wire/encode-readdir-resp [{:name "a.esp" :is-dir false :size 10 :mtime 1}])))))

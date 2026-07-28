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

(deftest server-side-codecs-match-golden
  (let [g (golden)]
    (is (= (:open-resp-fh42-size1000 g)
           (hex (wire/encode-open-resp {:fh 42 :size 1000 :is-dir false}))))
    (is (= (:read-resp-bulk-len5-off65536 g)
           (hex (wire/encode-read-resp-bulk 5 65536))))))

(deftest server-side-decoders-roundtrip
  (is (= {:flags 1 :path "Data/Skyrim.esm"}
         (wire/decode-open-req (wire/encode-open-req 1 "Data/Skyrim.esm"))))
  (is (= {:fh 7 :offset 10 :len 4}
         (wire/decode-read-req (wire/encode-read-req {:fh 7 :offset 10 :len 4}))))
  (is (= "Data/x" (wire/decode-path-req (.getBytes "Data/x" "UTF-8"))))
  (is (= 99 (wire/decode-close-req (wire/encode-close-req 99)))))

(deftest write-codecs-match-golden
  (let [g (golden)]
    (is (= (:write-req-fh7-off10-abc g) (hex (wire/encode-write-req {:fh 7 :offset 10} (.getBytes "abc" "UTF-8")))))
    (is (= (:write-resp-3 g) (hex (wire/encode-write-resp 3))))))

(deftest write-decoders-roundtrip
  (is (= {:fh 7 :offset 10 :data "abc"}
         (let [{:keys [fh offset data]} (wire/decode-write-req (wire/encode-write-req {:fh 7 :offset 10} (.getBytes "abc" "UTF-8")))]
           {:fh fh :offset offset :data (String. ^bytes data "UTF-8")})))
  (is (= 3 (wire/decode-write-resp (wire/encode-write-resp 3)))))

(deftest mkdir-rename-setattr-codecs-match-golden
  (let [g (golden)]
    (is (= (:mkdir-req-mode493-dir g) (hex (wire/encode-mkdir-req 493 "sub/dir"))))
    (is (= (:rename-req-a-b g)        (hex (wire/encode-rename-req "old.txt" "new.txt"))))
    (is (= (:setattr-req-fh5-size100 g) (hex (wire/encode-setattr-req {:fh 5 :size 100}))))))

(deftest mkdir-rename-setattr-decoders-roundtrip
  (is (= {:mode 493 :path "sub/dir"}
         (wire/decode-mkdir-req (wire/encode-mkdir-req 493 "sub/dir"))))
  (is (= {:from "old.txt" :to "new.txt"}
         (wire/decode-rename-req (wire/encode-rename-req "old.txt" "new.txt"))))
  (is (= {:fh 5 :size 100}
         (wire/decode-setattr-req (wire/encode-setattr-req {:fh 5 :size 100})))))

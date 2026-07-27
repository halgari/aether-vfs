(ns aether.vfs.os.windows.arena-test
  (:require [clojure.test :refer [deftest is]]
            [aether.vfs.os.windows.arena :as arena])
  (:import [java.lang.foreign Arena MemorySegment ValueLayout]))

(defn- seg ^MemorySegment [n] (.allocate (Arena/ofAuto) (long n) 8))

(deftest bank-offsets-round-robin
  (let [s (seg 8192)
        a (arena/make s 1024 4096 2)] ; mapping-offset 1024, len 4096, 2 banks -> bank-size 2048
    (is (= 1024 (arena/bank-mapping-offset a 0)))
    (is (= (+ 1024 2048) (arena/bank-mapping-offset a 1)))
    (is (= 1024 (arena/bank-mapping-offset a 2))))) ; slot 2 % 2 banks -> bank 0

(deftest fill-bank-writes-into-arena
  (let [s (seg 8192)
        a (arena/make s 1024 4096 2)
        {:keys [offset len]} (arena/fill-bank a 0 100
                               (fn [^MemorySegment bank]
                                 (.set bank ValueLayout/JAVA_BYTE 0 (byte 65)) ; 'A'
                                 (.set bank ValueLayout/JAVA_BYTE 1 (byte 66)) ; 'B'
                                 2))]
    (is (= 1024 offset))
    (is (= 2 len))
    (is (= 65 (.get s ValueLayout/JAVA_BYTE 1024)))
    (is (= 66 (.get s ValueLayout/JAVA_BYTE 1025)))))

(ns aether.vfs.os.windows.ring-test
  (:require [clojure.test :refer [deftest is]]
            [aether.vfs.os.windows.ring :as ring])
  (:import [java.lang.foreign Arena MemorySegment]))

(defn- heap-seg ^MemorySegment [n]
  ;; 8-aligned native segment usable for atomics, freed with the arena.
  (.allocate (Arena/ofAuto) (long n) 8))

(deftest init-open-roundtrip
  (let [seg (heap-seg 4096)
        geom (ring/init seg 4 256)]
    (is (= 4 (:slot-count geom)))
    (is (= 256 (:payload-cap geom)))
    ;; slot-stride = align8(32 + 256) = 288
    (is (= 288 (:slot-stride geom)))))

(deftest full-slot-cycle
  ;; Drive one slot SUBMITTED->PROCESSING->COMPLETED with the client + server
  ;; halves, entirely in-JVM, verifying the CAS state machine + framing.
  (let [seg (heap-seg 4096)
        geom (ring/init seg 2 256)
        slot (ring/claim-free seg geom)]
    (is (= 0 slot))
    (ring/publish-request seg geom slot 1 7 (.getBytes "ping" "UTF-8")) ; OP_GETATTR=1
    (let [taken (ring/server-take seg geom)]
      (is (= slot taken))
      (let [req (ring/read-request seg geom taken)]
        (is (= 1 (:opcode req)))
        (is (= 7 (:flags req)))
        (is (= "ping" (String. ^bytes (:payload req) "UTF-8"))))
      (ring/server-complete seg geom taken 42 (.getBytes "pong" "UTF-8")))
    (let [{:keys [status payload]} (ring/take-response seg geom slot)]
      (is (= 42 status))
      (is (= "pong" (String. ^bytes payload "UTF-8"))))
    (ring/free-slot seg geom slot)
    (is (= 0 (ring/claim-free seg geom)))))

(deftest server-take-nil-when-idle
  (let [seg (heap-seg 4096)
        geom (ring/init seg 2 256)]
    (is (nil? (ring/server-take seg geom)))))

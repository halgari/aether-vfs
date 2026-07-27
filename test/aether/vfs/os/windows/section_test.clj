(ns aether.vfs.os.windows.section-test
  (:require [clojure.test :refer [deftest is]]
            [aether.vfs.os.windows.section :as section]
            [aether.vfs.os.windows.ring :as ring])
  (:import [java.lang.foreign MemorySegment ValueLayout]))

(def ^:private windows?
  (.startsWith (.toLowerCase (System/getProperty "os.name")) "windows"))

(deftest create-open-alias-same-section
  (if-not windows?
    (println "skip: section-test is Windows-only")
    (let [nm (str "Local\\vfs-m2-test-" (.pid (java.lang.ProcessHandle/current)))
          creator (section/create nm (* 64 1024))]
      (try
        (let [geom (ring/init (:segment creator) 4 256)
              opener (section/open nm (* 64 1024))]
          (try
            ;; The opener sees the MAGIC/geometry the creator wrote into shared pages.
            (is (= 0x56464950 (.get ^MemorySegment (:segment opener) ValueLayout/JAVA_INT 0)))
            (is (= 4 (.get ^MemorySegment (:segment opener) ValueLayout/JAVA_INT 8))) ; slot_count
            (finally (section/close! opener))))
        (finally (section/close! creator))))))

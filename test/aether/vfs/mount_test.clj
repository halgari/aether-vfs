(ns aether.vfs.mount-test
  "Mounts an InlineProvider through the real, in-process FUSE mount helper and
  reads a file back through the kernel. Requires /dev/fuse."
  (:require [clojure.java.io :as io]
            [clojure.test :refer [deftest is]]
            [aether.vfs.test-util :refer [tmp-dir]]
            [aether.vfs.fuse :as fuse]
            [aether.vfs.providers.inline :as inline]))

(deftest read-through-fuse-mount
  (if-not (.exists (io/file "/dev/fuse"))
    (println "skip: /dev/fuse not available")
    (let [dir (tmp-dir)]
      (.mkdirs (io/file dir))
      (let [root (inline/inline-provider [["/hello.txt" (.getBytes "hi") 0644]])
            guard (fuse/mount root dir)]
        (try
          (let [target (io/file dir "hello.txt")
                deadline (+ (System/currentTimeMillis) 5000)
                got (loop []
                      (let [r (try (slurp target) (catch Exception _ nil))]
                        (cond
                          (some? r) r
                          (< (System/currentTimeMillis) deadline) (do (Thread/sleep 20) (recur))
                          :else ::never-readable)))]
            (is (= "hi" got))
            ;; directory listing through the kernel too
            (is (= ["hello.txt"] (mapv #(.getName ^java.io.File %) (.listFiles (io/file dir))))))
          (finally
            (.close ^java.io.Closeable guard)))))))

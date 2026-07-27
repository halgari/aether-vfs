(ns aether.vfs.os.windows.integration-test
  (:require [clojure.test :refer [deftest is]]
            [clojure.java.io :as io]
            [aether.vfs.os.windows.section :as section]
            [aether.vfs.os.windows.ring :as ring]
            [aether.vfs.os.windows.arena :as arena]
            [aether.vfs.os.windows.server :as server]
            [aether.vfs.providers.inline :as inline]))

(def ^:private windows?
  (.startsWith (.toLowerCase (System/getProperty "os.name")) "windows"))

(def ^:private harness-exe
  ;; Built by `cargo build -p vfs-ring-harness` (debug).
  (io/file "rust/target/debug/vfs-ring-harness.exe"))

(deftest cross-process-read-path
  (cond
    (not windows?) (println "skip: integration-test is Windows-only")
    (not (.exists harness-exe)) (println "skip: build rust/target/debug/vfs-ring-harness.exe first")
    :else
    (let [arena-off (* 512 1024)
          size (* 1 1024 1024)
          nm (str "Local\\vfs-m2-int-" (.pid (java.lang.ProcessHandle/current)))
          sec (section/create nm size)
          seg (:segment sec)
          geom (ring/init seg 4 256)
          ;; 2 banks -> 128 KiB bank-size, so the 70000-byte big.bin bulk read
          ;; is not truncated (4 banks would floor to 64 KiB and truncate it).
          a (arena/make seg arena-off (* 256 1024) 2)
          stop? (atom false)
          small (.getBytes "hello" "UTF-8")
          big (byte-array 70000 (byte 88))
          provider (inline/inline-provider [["/hello.txt" small 0644] ["/big.bin" big 0644]])
          server-thread (doto (Thread. #(server/serve seg geom a provider stop?)) (.setDaemon true) (.start))]
      (try
        (let [proc (-> (ProcessBuilder. [(.getPath harness-exe) nm (str size)])
                       (.inheritIO) (.start))
              ok (.waitFor proc)]
          (is (= 0 ok) "harness exited 0 (all read-path assertions passed)"))
        (finally
          (reset! stop? true)
          (section/close! sec))))))

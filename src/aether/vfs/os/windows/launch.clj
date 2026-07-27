(ns aether.vfs.os.windows.launch
  "Windows launcher: creates a JVM ring section, serves a Provider, injects the
  shim into a target (via the generic vfs-injector, dual-layer), and returns the
  target's exit code. Proven by the Part 1 spike: a real process reads
  Provider-served bytes through the injected shim's NtCreateFile/NtReadFile hooks.
  Windows-only (uses os/windows/section FFM)."
  (:require [aether.vfs.os.windows.section :as section]
            [aether.vfs.os.windows.ring :as ring]
            [aether.vfs.os.windows.arena :as arena]
            [aether.vfs.os.windows.server :as server]
            [aether.vfs.os.windows.shim-config :as cfg]
            [clojure.java.io :as io])
  (:import [java.lang.foreign MemorySegment]))

(defn- align8 ^long [^long n] (bit-and (+ n 7) (bit-not 7)))

(defn launch
  "Launch target-exe with the shim injected, serving `provider` over the ring.
  Returns the target's exit code. Windows-only."
  ^long [provider {:keys [target-exe target-args injector shim-dll payload root
                          payload-cap slot-count arena-len ready-timeout-ms child-env]
                   :or {root "C:\\GameLayers\\runtime" payload-cap 65536 slot-count 8
                        arena-len (* 4 1024 1024) target-args [] child-env {}}}]
  (let [stride (align8 (+ 32 (long payload-cap)))
        ring-bytes (+ 40 (* (long slot-count) stride))
        arena-off ring-bytes
        size (+ ring-bytes (long arena-len))
        nm (str "Local\\vfs-m3-" (.pid (java.lang.ProcessHandle/current)) "-" (System/nanoTime))
        sec (section/create nm size)
        seg (:segment sec)
        geom (ring/init seg slot-count payload-cap)
        a (arena/make seg arena-off arena-len slot-count)
        stop? (atom false)
        server-thread (doto (Thread. #(server/serve seg geom a provider stop?))
                        (.setDaemon true) (.start))
        tmp (System/getProperty "java.io.tmpdir")
        cfg-file (str tmp "vfs-m3-" (System/nanoTime) ".cfg")
        ready-file (str tmp "vfs-m3-" (System/nanoTime) ".ready")]
    (try
      (with-open [o (io/output-stream cfg-file)]
        (.write o ^bytes (cfg/encode root (cfg/empty-tree-snapshot))))
      (let [cmd (into [injector target-exe shim-dll payload cfg-file ready-file]
                      (when (seq target-args) (into ["--"] target-args)))
            pb (ProcessBuilder. ^java.util.List cmd)
            ^java.util.Map env (.environment pb)]
        (.put env "VFS_RING_SECTION" nm)
        (.put env "VFS_RING_BYTES" (str size))
        (.put env "VFS_RING_PAYLOAD_CAP" (str payload-cap))
        (.put env "VFS_ARENA_OFFSET" (str arena-off))
        (.put env "VFS_ARENA_LEN" (str arena-len))
        (doseq [[k v] child-env] (.put env (str k) (str v)))
        (.inheritIO pb)
        (let [proc (.start pb)]
          (.waitFor proc)))
      (finally
        (reset! stop? true)
        (try (section/close! sec) (catch Throwable _))
        (try (io/delete-file cfg-file true) (catch Throwable _))
        (try (io/delete-file ready-file true) (catch Throwable _))))))

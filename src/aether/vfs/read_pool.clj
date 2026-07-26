(ns aether.vfs.read-pool
  "Bounded thread-pool for blocking VFS read work. A provider read may block
  on a streamed-chunk fetch, so serving it off the caller's thread keeps a
  slow read from serializing its peers. jnr-fuse already dispatches requests
  on libfuse's multithreaded loop, so the FUSE adapter doesn't need this pool
  — it survives as the utility for any single-threaded consumer needing
  concurrent read batching."
  (:import (java.util.concurrent ExecutorService Executors TimeUnit)))

(defn read-pool ^ExecutorService [n]
  (Executors/newFixedThreadPool (max 1 (long n))))

(defn submit!
  "Run f on the pool. A throw in f is isolated: it kills only this job, never
  a worker."
  [^ExecutorService pool f]
  (.execute pool (fn []
                   (try
                     (f)
                     (catch Throwable _ nil)))))

(defn shutdown!
  "Stop accepting jobs and wait for submitted work to drain."
  [^ExecutorService pool]
  (.shutdown pool)
  (.awaitTermination pool 60 TimeUnit/SECONDS))

(ns aether.vfs.providers.passthrough
  "Serves a real backing directory unchanged. Backs unmatched paths.

  Env-gated I/O trace sink (VFS_TRACE=<file>): every open/read the VFS serves
  is recorded so an external observer can prove that all of a guest's file I/O
  flowed through this provider. Close the provider to flush FINAL lines for
  files a guest held open for its whole session."
  (:require [clojure.java.io :as io]
            [aether.vfs.error :as error]
            [aether.vfs.provider :as p]
            [aether.vfs.providers.fsutil :as fsutil]
            [aether.vfs.types :as types])
  (:import (java.io Closeable PrintWriter)
           (java.nio.channels FileChannel)
           (java.nio.file StandardOpenOption)))

(defn- trace-line! [trace line]
  (when trace
    (locking trace
      (.println ^PrintWriter trace ^String line)
      (.flush ^PrintWriter trace))))

(defrecord PassthroughProvider [backing open next-h trace stats]
  p/Provider
  (lookup [_ path]
    (fsutil/stat-meta (fsutil/real-file backing path)))

  (readdir [_ path]
    (fsutil/list-dir (fsutil/real-file backing path)))

  (open-file [_ path flags]
    (let [real (fsutil/real-file backing path)
          opts (into-array StandardOpenOption
                           (if (types/writable? flags)
                             [StandardOpenOption/READ StandardOpenOption/WRITE]
                             [StandardOpenOption/READ]))
          chan (error/with-io (FileChannel/open (.toPath real) opts))
          handle (swap! next-h inc)]
      (trace-line! trace (str "OPEN " real))
      (swap! open assoc handle {:chan chan :real (str real)})
      {:handle handle :cache :cached}))

  (read-at [_ handle offset size]
    (let [{:keys [chan real]} (or (get @open handle)
                                  (error/raise :invalid-argument "bad handle"))
          out (fsutil/pread chan offset size)]
      (when trace
        (swap! stats update real (fnil (fn [[b r]] [(+ (long b) (alength ^bytes out)) (inc (long r))]) [0 0])))
      out))

  (write-at [_ handle offset data]
    (let [{:keys [chan]} (or (get @open handle)
                             (error/raise :invalid-argument "bad handle"))]
      (fsutil/pwrite chan offset data)))

  (release-handle [_ handle]
    (when-some [{:keys [chan real]} (get @open handle)]
      (swap! open dissoc handle)
      (.close ^FileChannel chan)
      (when trace
        (let [[bytes reads] (get @stats real [0 0])]
          (trace-line! trace (str "CLOSE " real " bytes=" bytes " reads=" reads)))))
    nil)

  Closeable
  (close [_]
    ;; Files the guest still held open at shutdown (e.g. Skyrim keeps its
    ;; masters and .bsa archives open for the whole session) never got a CLOSE
    ;; line — emit their accumulated byte totals now so the manifest accounts
    ;; for every file the VFS served.
    (when trace
      (doseq [[_h {:keys [real]}] @open]
        (let [[bytes reads] (get @stats real [0 0])]
          (trace-line! trace (str "FINAL " real " bytes=" bytes " reads=" reads))))
      (.close ^PrintWriter trace))))

(defn passthrough-provider [backing-dir]
  (->PassthroughProvider backing-dir
                         (atom {})
                         (atom 0)
                         (when-some [path (System/getenv "VFS_TRACE")]
                           (try
                             (PrintWriter. (io/writer (io/file path) :append true))
                             (catch Exception _ nil)))
                         (atom {})))

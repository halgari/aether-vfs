(ns aether.vfs.test-util)

(defonce ^:private tmp-counter (atom 0))

(defn tmp-dir
  "Unique-per-call temp dir path (not created)."
  []
  (str (System/getProperty "java.io.tmpdir")
       "/aether-vfs-" (.pid (java.lang.ProcessHandle/current))
       "-" (swap! tmp-counter inc)))

(defn error-category
  "Runs thunk; returns the :aether.vfs/error category it throws, or nil if it
  returns normally."
  [thunk]
  (try
    (thunk)
    nil
    (catch clojure.lang.ExceptionInfo e
      (:aether.vfs/error (ex-data e)))))

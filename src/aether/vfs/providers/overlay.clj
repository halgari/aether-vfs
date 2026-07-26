(ns aether.vfs.providers.overlay
  "Copy-on-write overlay: an immutable base Provider + a writable upper
  scratch directory. Reads merge upper-over-base; deletes are recorded as
  .wh.<name> whiteout markers; .wh..wh..opq marks an opaque (non-merging)
  directory. The base is never mutated."
  (:require [clojure.java.io :as io]
            [aether.vfs.error :as error]
            [aether.vfs.provider :as p]
            [aether.vfs.providers.fsutil :as fsutil]
            [aether.vfs.types :as types])
  (:import (java.io File RandomAccessFile)
           (java.nio.channels FileChannel)
           (java.nio.file Files StandardCopyOption StandardOpenOption)))

(def ^:private wh-prefix ".wh.")
(def ^:private opaque ".wh..wh..opq")
(def ^:private copy-chunk (bit-shift-left 1 20)) ; 1 MiB copy-up reads

(defn- name-of
  "Last path component, or nil for root."
  [^String path]
  (when-not (= "/" path)
    (subs path (inc (.lastIndexOf path "/")))))

(defn- base-lookup [base path]
  (try
    (p/lookup base path)
    (catch clojure.lang.ExceptionInfo _ nil)))

(defn- base-dir? [base path]
  (= :dir (:kind (base-lookup base path))))

(declare merged-readdir)

(defrecord OverlayProvider [base upper open next-h])

(defn- upper-path ^File [^OverlayProvider ov path]
  (fsutil/real-file (:upper ov) path))

(defn- whiteout-path
  "<upper>/<parent>/.wh.<name> for path, or nil for root."
  ^File [^OverlayProvider ov path]
  (when-some [name (name-of path)]
    (when-some [parent (types/parent path)]
      (io/file (upper-path ov parent) (str wh-prefix name)))))

(defn- whiteout? [ov path]
  (boolean (some-> (whiteout-path ov path) .exists)))

(defn- clear-whiteout! [ov path]
  (some-> (whiteout-path ov path) .delete))

(defn- write-whiteout! [ov path]
  (let [^java.io.File w (or (whiteout-path ov path) (error/raise :invalid-argument "whiteout of root"))]
    (io/make-parents w)
    (error/with-io (.createNewFile w))
    nil))

(defn- store-handle! [ov h cache]
  (let [handle (swap! (:next-h ov) inc)]
    (swap! (:open ov) assoc handle h)
    {:handle handle :cache cache}))

(defn- copy-up!
  "Materialize path into upper by streaming base bytes. No-op if already
  present."
  [ov path]
  (let [up (upper-path ov path)]
    (when-not (.exists up)
      (io/make-parents up)
      (let [o (p/open-file (:base ov) path types/o-rdonly)]
        (try
          (error/with-io
            (with-open [out (io/output-stream up)]
              (loop [off 0]
                (let [chunk ^bytes (p/read-at (:base ov) (:handle o) off copy-chunk)]
                  (when (pos? (alength chunk))
                    (.write out chunk)
                    (recur (+ off (alength chunk))))))))
          (finally
            (try (p/release-handle (:base ov) (:handle o)) (catch Exception _ nil)))))
      (clear-whiteout! ov path))))

(defn- copy-up-dir!
  "Recursively materialize a directory subtree (merged view) into upper."
  [ov path]
  (error/with-io (Files/createDirectories (.toPath (upper-path ov path)) (make-array java.nio.file.attribute.FileAttribute 0)))
  (doseq [{:keys [name kind]} (merged-readdir ov path)]
    (let [child (types/child path name)]
      (case kind
        :dir (copy-up-dir! ov child)
        :file (copy-up! ov child)))))

(defn- merged-readdir [ov path]
  (let [up (upper-path ov path)
        opaque? (.exists (io/file up opaque))
        upper-is-dir (.isDirectory up)
        base-entries (if opaque?
                       []
                       (try
                         (p/readdir (:base ov) path)
                         (catch clojure.lang.ExceptionInfo e
                           (if upper-is-dir
                             [] ; dir exists only in upper; base miss is fine
                             (throw e))))) ; dir exists nowhere
        names (into {} (map (fn [e] [(:name e) (:kind e)])) base-entries)
        names (if-not upper-is-dir
                names
                (let [entries (fsutil/list-dir up)
                      whiteouts (into []
                                      (keep (fn [{:keys [name]}]
                                              (when (and (not= name opaque)
                                                         (clojure.string/starts-with? name wh-prefix))
                                                (subs name (count wh-prefix)))))
                                      entries)
                      uppers (remove (fn [{:keys [name]}]
                                       (or (= name opaque)
                                           (clojure.string/starts-with? name wh-prefix)))
                                     entries)]
                  (as-> names m
                    (apply dissoc m whiteouts)
                    (into m (map (fn [e] [(:name e) (:kind e)])) uppers))))]
    (mapv (fn [[name kind]] {:name name :kind kind}) names)))

(extend-type OverlayProvider
  p/Provider
  (lookup [ov path]
    (when (whiteout? ov path)
      (error/raise :not-found (str path " is whited out")))
    (error/on-not-found
     (fsutil/stat-meta (upper-path ov path))
     (p/lookup (:base ov) path)))

  (readdir [ov path]
    (merged-readdir ov path))

  (open-file [ov path flags]
    (when (whiteout? ov path)
      (error/raise :not-found (str path " is whited out")))
    (let [up (upper-path ov path)]
      (cond
        (types/writable? flags)
        (do (when-not (.exists up)
              (copy-up! ov path))
            (store-handle! ov {:layer :upper
                               :chan (error/with-io
                                       (FileChannel/open (.toPath up)
                                                         (into-array StandardOpenOption
                                                                     [StandardOpenOption/READ StandardOpenOption/WRITE])))}
                           :cached))

        (.exists up)
        (store-handle! ov {:layer :upper
                           :chan (error/with-io
                                   (FileChannel/open (.toPath up)
                                                     (into-array StandardOpenOption
                                                                 [StandardOpenOption/READ])))}
                       :cached)

        :else
        ;; propagate the base provider's cache mode (e.g. :direct-io) so FUSE
        ;; open replies set the correct flags
        (let [o (p/open-file (:base ov) path flags)]
          (store-handle! ov {:layer :base :handle (:handle o)} (:cache o))))))

  (read-at [ov handle offset size]
    (let [h (or (get @(:open ov) handle) (error/raise :invalid-argument "bad handle"))]
      (case (:layer h)
        :upper (fsutil/pread (:chan h) offset size)
        :base (p/read-at (:base ov) (:handle h) offset size))))

  (write-at [ov handle offset data]
    (let [h (or (get @(:open ov) handle) (error/raise :invalid-argument "bad handle"))]
      (case (:layer h)
        :upper (fsutil/pwrite (:chan h) offset data)
        :base (error/raise :read-only "base handles are read-only"))))

  (release-handle [ov handle]
    (when-some [h (get @(:open ov) handle)]
      (swap! (:open ov) dissoc handle)
      (case (:layer h)
        :upper (.close ^FileChannel (:chan h))
        :base (p/release-handle (:base ov) (:handle h))))
    nil)

  p/Writable
  (create-file [ov path _flags mode]
    (let [up (upper-path ov path)]
      (io/make-parents up)
      (let [chan (error/with-io
                   (FileChannel/open (.toPath up)
                                     (into-array StandardOpenOption
                                                 [StandardOpenOption/CREATE
                                                  StandardOpenOption/READ
                                                  StandardOpenOption/WRITE
                                                  StandardOpenOption/TRUNCATE_EXISTING])))]
        (clear-whiteout! ov path)
        (fsutil/set-perms-best-effort! up mode)
        (store-handle! ov {:layer :upper :chan chan} :cached))))

  (truncate! [ov path size]
    (when (whiteout? ov path)
      (error/raise :not-found (str path " is whited out")))
    (let [up (upper-path ov path)]
      (when-not (.exists up)
        (copy-up! ov path))
      (error/with-io
        (with-open [raf (RandomAccessFile. up "rw")]
          (.setLength raf size)))
      nil))

  (unlink! [ov path]
    (let [up (upper-path ov path)
          in-upper (.exists up)
          in-base (and (not (whiteout? ov path)) (some? (base-lookup (:base ov) path)))]
      (when-not (or in-upper in-base)
        (error/raise :not-found path))
      (when in-upper
        (error/with-io (Files/delete (.toPath up))))
      (when in-base
        (write-whiteout! ov path))
      nil))

  (mkdir! [ov path mode]
    (let [up (upper-path ov path)
          was-whiteout (whiteout? ov path)]
      (when (or (.exists up)
                (and (not was-whiteout) (some? (base-lookup (:base ov) path))))
        (error/raise :already-exists path))
      (io/make-parents up)
      (clear-whiteout! ov path)
      (error/with-io (Files/createDirectory (.toPath up) (make-array java.nio.file.attribute.FileAttribute 0)))
      (fsutil/set-perms-best-effort! up mode)
      (when was-whiteout
        ;; recreating a deleted base dir: keep it opaque so base contents
        ;; don't merge
        (error/with-io (.createNewFile (io/file up opaque))))
      nil))

  (rmdir! [ov path]
    (when (seq (merged-readdir ov path))
      (error/raise :not-empty path))
    (let [up (upper-path ov path)
          in-base (and (not (whiteout? ov path)) (base-dir? (:base ov) path))]
      (when (.isDirectory up)
        ;; merged view is empty, so any upper entries are only markers — clear
        ;; them
        (error/with-io
          (doseq [^File f (.listFiles up)]
            (Files/delete (.toPath f)))
          (Files/delete (.toPath up))))
      (when in-base
        (write-whiteout! ov path))
      nil))

  (rename! [ov from to]
    (when (whiteout? ov from)
      (error/raise :not-found from))
    ;; POSIX: renaming onto a non-empty directory is an error.
    (when-some [m (try (p/lookup ov to) (catch clojure.lang.ExceptionInfo _ nil))]
      (when (and (= :dir (:kind m)) (seq (merged-readdir ov to)))
        (error/raise :not-empty to)))
    (let [from-up (upper-path ov from)]
      ;; Materialize the source into upper before moving it. A directory that
      ;; exists in base must ALWAYS be subtree-copied (even when it already
      ;; has an upper footprint such as a created child or a whiteout) —
      ;; otherwise its base-only children would be silently dropped from the
      ;; merged view by the move. copy-up-dir!/copy-up! are idempotent, so
      ;; this is safe to run unconditionally for dirs.
      (cond
        (base-dir? (:base ov) from) (copy-up-dir! ov from)
        (not (.exists from-up)) (copy-up! ov from))
      (let [to-up (upper-path ov to)]
        (io/make-parents to-up)
        (clear-whiteout! ov to)
        (cond
          (.isDirectory to-up) (doseq [^File f (reverse (file-seq to-up))] (.delete f))
          (.exists to-up) (.delete to-up))
        (error/with-io
          (Files/move (.toPath from-up) (.toPath to-up)
                      (make-array StandardCopyOption 0)))
        (when (some? (base-lookup (:base ov) from))
          (write-whiteout! ov from))
        ;; If base has a directory at to, mark upper/to opaque so the base
        ;; directory's stale children don't merge back through the renamed dir.
        (when (and (.isDirectory to-up) (base-dir? (:base ov) to))
          (error/with-io (.createNewFile (io/file to-up opaque))))
        nil))))

(defn overlay-provider [base upper-dir]
  (->OverlayProvider base upper-dir (atom {}) (atom 0)))

(defn wrap-if-requested
  "If upper-dir is non-nil, wrap base in a writable overlay; otherwise return
  base unchanged. The caller owns the upper dir's lifecycle."
  [base upper-dir]
  (if upper-dir
    (overlay-provider base upper-dir)
    base))

(ns aether.vfs.os.linux.launch
  "Linux launcher for the unified aether.vfs/run: mount a Provider at a
  mountpoint (user-space FUSE, no admin), run a target program against it
  (cwd = mountpoint, $AETHER_VFS_MOUNT set), wait, then unmount + clean up a
  temp mountpoint we created. Generalizes the proton.clj pattern."
  (:require [aether.vfs.os.linux.fuse :as fuse]
            [clojure.java.io :as io]
            [clojure.string :as str])
  (:import [java.io Closeable File]))

(defn- fresh-mountpoint ^File []
  (doto (io/file (System/getProperty "java.io.tmpdir") (str "aether-vfs-mnt-" (System/nanoTime)))
    (.mkdirs)))

(defn- mounted-in-proc?
  "True once `canonical` appears as a mountpoint in /proc/self/mounts — the FUSE
  loop registers it there when the mount goes live. Provider-agnostic (works for
  an empty-root provider, unlike a directory-listing probe)."
  [^String canonical]
  (try
    (with-open [r (io/reader "/proc/self/mounts")]
      (boolean (some #(= canonical (second (str/split ^String % #" "))) (line-seq r))))
    (catch Throwable _ false)))

(defn- wait-ready!
  "The non-blocking FUSE mount returns BEFORE the kernel mount is visible, so poll
  until it is actually live (or `timeout-ms` elapses). `(.list mountpoint)` alone
  is not a readiness signal — an empty dir we just created already returns a
  non-null empty array. Ready = mount registered in /proc, OR the provider's root
  entries have appeared."
  [^File mountpoint ^long timeout-ms]
  (let [canonical (.getCanonicalPath mountpoint)
        deadline (+ (System/currentTimeMillis) timeout-ms)]
    (loop []
      (cond
        (mounted-in-proc? canonical) true
        (try (seq (.list mountpoint)) (catch Throwable _ false)) true
        (> (System/currentTimeMillis) deadline) false
        :else (do (Thread/sleep 25) (recur))))))

(defn run
  "Mount `provider` at a mountpoint and run `:exec` inside it. Returns the
  target's exit code. Linux-only (user-space FUSE)."
  ^long [provider {:keys [exec mountpoint env] :or {env {}}}]
  (when-not (seq exec) (throw (ex-info "aether.vfs.os.linux.launch/run: :exec required" {})))
  (let [owned? (nil? mountpoint)
        ^File mp (if mountpoint (io/file mountpoint) (fresh-mountpoint))
        ^Closeable guard (fuse/mount provider (.getPath mp))]
    (try
      (wait-ready! mp 3000)
      (let [pb (ProcessBuilder. ^java.util.List (vec exec))
            e (.environment pb)]
        (.put e "AETHER_VFS_MOUNT" (.getPath mp))
        (doseq [[k v] env] (.put e (str k) (str v)))
        (.directory pb mp)
        (.inheritIO pb)
        (let [proc (.start pb)]
          (try (long (.waitFor proc))
               (finally (when (.isAlive proc) (.destroyForcibly proc))))))
      (finally
        (try (.close guard) (catch Throwable _))
        (when owned? (try (.delete mp) (catch Throwable _)))))))

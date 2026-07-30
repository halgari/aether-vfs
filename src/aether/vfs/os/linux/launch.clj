(ns aether.vfs.os.linux.launch
  "Linux launcher for the unified aether.vfs/run: mount a Provider at a
  mountpoint (user-space FUSE, no admin), run a target program against it
  (cwd = mountpoint, $AETHER_VFS_MOUNT set), wait, then unmount + clean up a
  temp mountpoint we created. Generalizes the proton.clj pattern."
  (:require [aether.vfs.os.linux.fuse :as fuse]
            [clojure.java.io :as io])
  (:import [java.io Closeable File]))

(defn- fresh-mountpoint ^File []
  (doto (io/file (System/getProperty "java.io.tmpdir") (str "aether-vfs-mnt-" (System/nanoTime)))
    (.mkdirs)))

(defn- wait-ready!
  "The non-blocking FUSE mount runs its loop in a background thread; poll until
  the mountpoint answers a listing (or `timeout-ms` elapses)."
  [^File mountpoint ^long timeout-ms]
  (let [deadline (+ (System/currentTimeMillis) timeout-ms)]
    (loop []
      (cond
        (try (some? (.list mountpoint)) (catch Throwable _ false)) true
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

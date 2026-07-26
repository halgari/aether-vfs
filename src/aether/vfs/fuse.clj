(ns aether.vfs.fuse
  "Translates FUSE ops into Provider calls via jnr-fuse and mounts in-process.

  jnr-fuse's high-level API is path-based (libfuse resolves inodes for us), so
  there is no InodeTable in the request path — the table survives as
  aether.vfs.inode for a future low-level adapter. Request concurrency comes
  from libfuse's multithreaded loop; every provider call is guarded so one
  bad request becomes an errno, never a torn-down mount."
  (:require [aether.vfs.error :as error]
            [aether.vfs.provider :as p]
            [aether.vfs.router :as router]
            [aether.vfs.types :as types])
  (:import (java.io Closeable)
           (java.lang.foreign MemorySegment)
           (java.nio.file Paths)
           (java.util.concurrent Semaphore)
           (jnr.ffi Pointer)
           (jnr.posix POSIXFactory)
           (ru.serce.jnrfuse ErrorCodes FuseFillDir FuseStubFS)
           (ru.serce.jnrfuse.struct FileStat FuseFileInfo)))

;; MemorySegment/ofAddress + .reinterpret are FFM "restricted" methods; the
;; JVM may deny them unless started with --enable-native-access=ALL-UNNAMED.
;; Probe once: when denied, reads fall back to the heap read-at path.
(def ^:private ffm-native-access?
  (delay
    (and (nil? (System/getenv "AETHER_VFS_NO_READ_INTO"))   ; escape hatch: force the heap read-at path
         (try
           (.reinterpret (MemorySegment/ofAddress 8) 1)
           true
           (catch Throwable _ false)))))

(defn- pointer->byte-buffer
  "View `size` bytes of a raw native pointer (the kernel's FUSE buffer) as a
  direct ByteBuffer — RocksDB can then write cached chunks straight into it."
  ^java.nio.ByteBuffer [^Pointer p ^long size]
  (-> (MemorySegment/ofAddress (.address p))
      (.reinterpret size)
      (.asByteBuffer)))

(defn- guarded
  "Run (f); success maps through ok (default → 0), fs errors and any throw
  become a negative errno (panic isolation: one bad request never tears down
  the mount)."
  (^long [f] (guarded f (fn [_] 0)))
  (^long [f ok]
   (try
     (long (ok (f)))
     (catch clojure.lang.ExceptionInfo e
       (- (error/errno (or (error/category e) :io))))
     (catch Throwable _
       (- (error/errno :io))))))

(defn- fill-stat! [^FileStat stat meta ^long uid ^long gid]
  (let [dir? (= :dir (:kind meta))]
    (.set (.-st_mode stat) (bit-or (if dir? FileStat/S_IFDIR FileStat/S_IFREG)
                                   (long (:perm meta))))
    (.set (.-st_size stat) (long (:size meta)))
    (.set (.-st_nlink stat) (if dir? 2 1))
    (.set (.-st_uid stat) uid)
    (.set (.-st_gid stat) gid)
    (.set (.-tv_sec (.-st_mtim stat)) (max 0 (long (:mtime-secs meta))))))

;; Bound concurrent read callbacks. libfuse's multithreaded loop spawns a worker
;; per in-flight request; when reads are slow (a network-backed provider streaming
;; from Steam) they pile up faster than they drain, and the accumulated in-flight
;; decode/assemble state exhausts the heap. Acquiring at read entry caps the
;; heap-heavy work — excess libfuse threads block holding almost nothing.
(def ^:private max-concurrent-reads
  (long (or (some-> (System/getenv "AETHER_VFS_MAX_FUSE_READS") Long/parseLong) 32)))

(defn- vfs-filesystem ^FuseStubFS [r]
  (let [posix (POSIXFactory/getNativePOSIX)
        uid (.getuid posix)
        gid (.getgid posix)
        handles (atom {})
        next-h (atom 0)
        ^Semaphore read-sema (Semaphore. (int max-concurrent-reads))
        provider-of (fn [^String path] (router/provider-for r (types/from-wire path)))
        handle-of (fn [^FuseFileInfo fi] (get @handles (.get (.-fh fi))))
        ;; NOTE: some FUSE implementations set FOPEN_KEEP_CACHE / FOPEN_DIRECT_IO
        ;; per-open from the provider's CacheMode. jnr-fuse's FuseFileInfo
        ;; exposes those libfuse2 bitfields only as unsettable Struct$Padding,
        ;; so per-open cache flags are dropped here: the kernel's default FUSE
        ;; caching (page-cache backed, invalidated across opens) applies, which
        ;; is correct for every provider; keep-cache is a perf nicety and
        ;; :direct-io is opt-in and unused.
        open-into! (fn [^FuseFileInfo fi prov opened]
                     (let [fh (long (swap! next-h inc))]
                       (swap! handles assoc fh {:provider prov :handle (:handle opened)
                                                :read-into? (and @ffm-native-access?
                                                                 (satisfies? p/ReadInto prov))})
                       (.set (.-fh fi) fh)
                       0))]
    (proxy [FuseStubFS] []
      (getattr [^String path ^FileStat stat]
        (guarded #(p/lookup (provider-of path) (types/from-wire path))
                 (fn [meta] (fill-stat! stat meta uid gid) 0)))

      (readdir [^String path ^Pointer buf ^FuseFillDir filter _offset ^FuseFileInfo _fi]
        (guarded #(p/readdir (provider-of path) (types/from-wire path))
                 (fn [entries]
                   (.apply filter buf "." nil 0)
                   (.apply filter buf ".." nil 0)
                   (doseq [entry entries]
                     (.apply filter buf ^String (:name entry) nil 0))
                   0)))

      (open [^String path ^FuseFileInfo fi]
        (guarded #(let [prov (provider-of path)]
                    [prov (p/open-file prov (types/from-wire path) (.get (.-flags fi)))])
                 (fn [[prov opened]] (open-into! fi prov opened))))

      (read [^String _path ^Pointer buf size offset ^FuseFileInfo fi]
        (if-some [h (handle-of fi)]
          ;; acquire BEFORE any allocation so blocked libfuse workers hold ~nothing
          (do (.acquire read-sema)
              (try
                (let [want (min (long size) (long Integer/MAX_VALUE))]
                  (if (:read-into? h)
                    ;; zero-heap: the provider writes into the kernel's buffer
                    (guarded #(if (zero? want)
                                0
                                (p/read-into! (:provider h) (:handle h) offset
                                              (pointer->byte-buffer buf want)))
                             long)
                    (guarded #(p/read-at (:provider h) (:handle h) offset want)
                             (fn [^bytes bytes]
                               (.put buf 0 bytes 0 (alength bytes))
                               (alength bytes)))))
                (finally (.release read-sema))))
          (- (ErrorCodes/EBADF))))

      (write [^String _path ^Pointer buf size offset ^FuseFileInfo fi]
        (if-some [h (handle-of fi)]
          (guarded #(let [data (byte-array size)]
                      (.get buf 0 data 0 (int size))
                      (p/write-at (:provider h) (:handle h) offset data))
                   long)
          (- (ErrorCodes/EBADF))))

      (create [^String path mode ^FuseFileInfo fi]
        (guarded #(let [prov (provider-of path)]
                    [prov (p/create prov (types/from-wire path)
                                    (.get (.-flags fi))
                                    (bit-and (long mode) 07777))])
                 (fn [[prov opened]] (open-into! fi prov opened))))

      (unlink [^String path]
        (guarded #(p/unlink (provider-of path) (types/from-wire path))))

      (mkdir [^String path mode]
        (guarded #(p/mkdir (provider-of path) (types/from-wire path) (bit-and (long mode) 07777))))

      (rmdir [^String path]
        (guarded #(p/rmdir (provider-of path) (types/from-wire path))))

      (rename [^String from ^String to]
        (guarded #(p/rename (provider-of from) (types/from-wire from) (types/from-wire to))))

      (truncate [^String path size]
        (guarded #(p/truncate (provider-of path) (types/from-wire path) size)))

      (release [^String _path ^FuseFileInfo fi]
        (let [fh (.get (.-fh fi))]
          (when-some [h (get @handles fh)]
            (swap! handles dissoc fh)
            (guarded #(p/release-handle (:provider h) (:handle h))))
          0)))))

(defn mount-router
  "Mount a router at mountpoint in-process. Returns a Closeable guard;
  closing it unmounts."
  ^Closeable [r mountpoint]
  (let [fs (vfs-filesystem r)]
    (.mount fs (Paths/get (str mountpoint) (make-array String 0)) false false
            (into-array String ["-o" "fsname=aether-vfs"]))
    (reify Closeable
      (close [_] (.umount fs)))))

(defn mount
  "Mount root (as the sole/default provider) at mountpoint in-process —
  equivalent to routing every path to root."
  ^Closeable [root mountpoint]
  (mount-router (router/router root) mountpoint))

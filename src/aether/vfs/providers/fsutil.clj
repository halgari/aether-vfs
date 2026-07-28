(ns aether.vfs.providers.fsutil
  "Shared host-filesystem plumbing for the passthrough and overlay providers."
  (:require [clojure.java.io :as io]
            [aether.vfs.error :as vfs-error]
            [aether.vfs.types :as types])
  (:import (java.io File)
           (java.nio ByteBuffer)
           (java.nio.channels FileChannel)
           (java.nio.file Files LinkOption Path)
           (java.nio.file.attribute BasicFileAttributes FileTime PosixFilePermission PosixFileAttributes)))

(def ^:private ^"[Ljava.nio.file.LinkOption;" no-follow
  (into-array LinkOption [LinkOption/NOFOLLOW_LINKS]))

(def ^:private perm-bits
  {PosixFilePermission/OWNER_READ 0400
   PosixFilePermission/OWNER_WRITE 0200
   PosixFilePermission/OWNER_EXECUTE 0100
   PosixFilePermission/GROUP_READ 040
   PosixFilePermission/GROUP_WRITE 020
   PosixFilePermission/GROUP_EXECUTE 010
   PosixFilePermission/OTHERS_READ 04
   PosixFilePermission/OTHERS_WRITE 02
   PosixFilePermission/OTHERS_EXECUTE 01})

(defn perms->int ^long [perms]
  (reduce (fn [acc [p bit]] (if (contains? perms p) (bit-or (long acc) (long bit)) acc))
          0 perm-bits))

(defn int->perms [mode]
  (into #{}
        (keep (fn [[p bit]] (when (pos? (bit-and (long mode) (long bit))) p)))
        perm-bits))

(defn real-file
  "The backing File for a virtual path under a backing root."
  ^File [backing ^String path]
  (let [rel (types/relative path)]
    (if (= "" rel) (io/file backing) (io/file backing rel))))

(defn stat-meta
  "Meta for a real file (symlink metadata: links are not followed). POSIX
  permissions are used when the host filesystem supports them; on
  filesystems without a POSIX attribute view (e.g. Windows/NTFS, which
  throws UnsupportedOperationException for PosixFileAttributes), fall back
  to BasicFileAttributes with a conservative default perm."
  [^File f]
  (vfs-error/with-io
    (let [path (.toPath f)]
      (try
        (let [attrs ^PosixFileAttributes (Files/readAttributes path PosixFileAttributes no-follow)]
          {:size (.size attrs)
           :kind (if (.isDirectory attrs) :dir :file)
           :perm (perms->int (.permissions attrs))
           :mtime-secs (quot (.toMillis ^FileTime (.lastModifiedTime attrs)) 1000)
           :cache :cached})
        (catch UnsupportedOperationException _
          (let [attrs ^BasicFileAttributes (Files/readAttributes path BasicFileAttributes no-follow)]
            {:size (.size attrs)
             :kind (if (.isDirectory attrs) :dir :file)
             :perm (if (.isDirectory attrs) 0755 0644)
             :mtime-secs (quot (.toMillis ^FileTime (.lastModifiedTime attrs)) 1000)
             :cache :cached}))))))

(defn list-dir
  "DirEntries for a real directory."
  [^File f]
  (vfs-error/with-io
    (with-open [ds (Files/newDirectoryStream (.toPath f))]
      (mapv (fn [^Path child]
              {:name (str (.getFileName child))
               :kind (if (Files/isDirectory child (make-array LinkOption 0)) :dir :file)})
            ds))))

(defn set-perms-best-effort! [^File f mode]
  (try
    (Files/setPosixFilePermissions (.toPath f) (int->perms mode))
    (catch Exception _ nil)))

(defn pread
  "Positional read of up to size bytes at offset (pread semantics)."
  ^bytes [^FileChannel ch ^long offset ^long size]
  (vfs-error/with-io
    (let [buf (ByteBuffer/allocate (int size))]
      (loop [pos offset]
        (let [n (.read ch buf pos)]
          (if (or (neg? n) (not (.hasRemaining buf)))
            (let [out (byte-array (.position buf))]
              (.flip buf)
              (.get buf out)
              out)
            (recur (+ pos n))))))))

(defn pwrite
  "Positional write; returns bytes written."
  ^long [^FileChannel ch ^long offset ^bytes data]
  (vfs-error/with-io
    (.write ch (ByteBuffer/wrap data) offset)))

(ns aether.vfs.provider
  "The software definition of files. Path-based; never returns an fd.

  Read-only providers implement Provider; writable ones also implement
  Writable. The create/unlink/rename/mkdir/rmdir/truncate wrappers default to
  :read-only when the provider is not Writable."
  (:refer-clojure :exclude [read])
  (:require [aether.vfs.error :as error]))

(defprotocol Provider
  (lookup [p path]
    "Metadata for a path (serves both FUSE lookup and getattr). Size MUST be
    exact.")
  (readdir [p path]
    "Directory entries for a directory path (names + kinds; no \".\"/\"..\").")
  (open-file [p path flags]
    "Open a file, returning {:handle h :cache mode}.")
  (read-at [p handle offset size]
    "Read size bytes at offset from an open handle, as a byte array.")
  (write-at [p handle offset data]
    "Write data at offset, returning bytes written.")
  (release-handle [p handle]
    "Release a handle opened by open-file."))

(defprotocol ReadInto
  "Optional zero-copy read: providers that can write straight into a caller-
  supplied buffer implement this; the FUSE adapter hands them a view of the
  kernel's own buffer so cached data never touches the Java heap. Others fall
  back to read-at."
  (read-into! [p handle offset dst]
    "Read up to (.remaining dst) bytes at offset into dst, a DIRECT
    java.nio.ByteBuffer (position advances by the bytes written). Returns
    bytes written; 0 at/past EOF."))

(defprotocol Writable
  (create-file [p path flags mode]
    "Create a new file at path and open it writable; returns {:handle :cache}.")
  (unlink! [p path]
    "Remove a file (or whiteout it over an immutable base).")
  (rename! [p from to])
  (mkdir! [p path mode])
  (rmdir! [p path]
    "Remove an empty directory.")
  (truncate! [p path size]
    "Set a file's length (truncate or extend with zeros)."))

(defn- read-only! [p op]
  (error/raise :read-only (str op " on a read-only provider " (class p))))

(defn create [p path flags mode]
  (if (satisfies? Writable p) (create-file p path flags mode) (read-only! p "create")))

(defn unlink [p path]
  (if (satisfies? Writable p) (unlink! p path) (read-only! p "unlink")))

(defn rename [p from to]
  (if (satisfies? Writable p) (rename! p from to) (read-only! p "rename")))

(defn mkdir [p path mode]
  (if (satisfies? Writable p) (mkdir! p path mode) (read-only! p "mkdir")))

(defn rmdir [p path]
  (if (satisfies? Writable p) (rmdir! p path) (read-only! p "rmdir")))

(defn truncate [p path size]
  (if (satisfies? Writable p) (truncate! p path size) (read-only! p "truncate")))

(ns aether.vfs.types
  "Core value types shared between the FUSE daemon and providers.

  A virtual path is a normalized string from the mount root, always starting
  with '/'. The root is \"/\"; no trailing slash except for root.

  Meta:     {:size long :kind :file|:dir :perm int :mtime-secs long
             :cache :cached|:direct-io}
  DirEntry: {:name String :kind :file|:dir}
  Opened:   {:handle long :cache :cached|:direct-io}"
  (:require [clojure.string :as str]))

(def root "/")

(defn child
  "Join a child component onto a path."
  ^String [^String path ^String name]
  (if (= "/" path)
    (str "/" name)
    (str path "/" name)))

(defn relative
  "The path relative to root with no leading slash (\"\" for root)."
  ^String [^String path]
  (str/replace path #"^/+" ""))

(defn parent
  "The parent path, or nil for root."
  [^String path]
  (when-not (= "/" path)
    (let [idx (.lastIndexOf path "/")]
      (cond
        (zero? idx) "/" ; "/foo" -> "/"
        (pos? idx) (subs path 0 idx)
        :else nil))))

(defn from-wire
  "Reconstruct a path from a wire string (already normalized, '/'-rooted)."
  ^String [^String s]
  (if (str/blank? s) root s))

;; open(2) access-mode flags (Linux values; no libc on the JVM).
(def ^:const o-rdonly 0)
(def ^:const o-wronly 1)
(def ^:const o-rdwr 2)
(def ^:const o-accmode 3)

(defn writable? [flags]
  (not= o-rdonly (bit-and (long flags) o-accmode)))

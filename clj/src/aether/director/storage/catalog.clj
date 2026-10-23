(ns aether.director.storage.catalog
  "The storage catalog: what's in each layer, and the live and cache files,
  as rows in the block store's LMDB environment, so a row and the blocks it
  names commit together.

    layers       name (UTF-8) -> layer id u64
    entries      [layer id u64][folded path] -> v1 | kind u8 | guid 16 | len u64 | mtime i64 | name
    live         live key 16 -> len u64 | mtime i64
    cache_files  cache key 16 -> last access (minutes) u64 | logical bytes u64
    cache_origins cache key 16 -> root u64 | size u64 | mtime i64 | u32 source len | source | vpath
                 (what the key was hashed from: which source to fetch it again from)
    smeta        name -> u64 (next_layer_id)

  (keys big-endian, values little-endian). Paths are vpaths: `/`-separated,
  folded (aether.director.path); the root is the empty path, and an entry's
  name is its last component as it was spelled. Store file ids: a layer
  file's is \"L\" + its GUID, a live file's \"F\" + its key, a cache file's
  \"C\" + a hash of its source."
  (:require [aether.director.store.lmdb :as l])
  (:import (aether.director Codec Lmdb)
           (java.lang.foreign MemorySegment ValueLayout)
           (java.nio ByteBuffer ByteOrder)
           (java.nio.charset StandardCharsets)
           (java.security SecureRandom)))

(set! *warn-on-reflection* true)

(def dbs ["layers" "entries" "live" "cache_files" "cache_origins" "smeta"])

(def ^:const kind-file 1)
(def ^:const kind-dir 2)

(defn- dbi ^long [store db-name] (l/dbi (:env store) db-name))
(defn- utf8 ^bytes [^String s] (.getBytes s StandardCharsets/UTF_8))
(defn- le ^ByteBuffer [n] (.order (ByteBuffer/allocate n) ByteOrder/LITTLE_ENDIAN))

(defn- utf8-str
  "The UTF-8 string of segment `s` from byte `off` on."
  ^String [^MemorySegment s off]
  (String. ^bytes (.toArray (.asSlice s (long off)) ValueLayout/JAVA_BYTE) StandardCharsets/UTF_8))

;; --- ids ---------------------------------------------------------------------------

(def ^:private rng (SecureRandom.))

(defn new-guid ^bytes [] (let [b (byte-array 16)] (.nextBytes ^SecureRandom rng b) b))

(defn- prefixed ^bytes [^long tag ^bytes b]
  (let [out (byte-array (inc (alength b)))]
    (aset out 0 (unchecked-byte tag))
    (System/arraycopy b 0 out 1 (alength b))
    out))

(defn layer-file-id ^bytes [^bytes guid] (prefixed (int \L) guid))
(defn live-file-id ^bytes [^bytes k] (prefixed (int \F) k))
(defn cache-file-id ^bytes [^bytes k] (prefixed (int \C) k))

(defn cache-key
  "The 16-byte key of a cache file: a hash of where its content comes from."
  ^bytes [^String source root ^String vpath size mtime]
  (let [s (utf8 source) p (utf8 vpath)
        bb (le (+ 4 (alength s) 8 4 (alength p) 16))]
    (.putInt bb (alength s)) (.put bb s) (.putLong bb (long root))
    (.putInt bb (alength p)) (.put bb p) (.putLong bb (long size)) (.putLong bb (long mtime))
    (Codec/blake3 (MemorySegment/ofArray (.array bb)) 16)))

;; --- entries --------------------------------------------------------------------------

(defn entry-key ^bytes [layer ^String folded]
  (let [p (utf8 folded) bb (ByteBuffer/allocate (+ 8 (alength p)))]
    (.putLong bb (long layer)) (.put bb p) (.array bb)))

(defn- encode-entry ^bytes [{:keys [dir? ^bytes guid len mtime] nm :name}]
  (let [n (utf8 nm) bb (le (+ 34 (alength n)))]
    (.put bb (byte 1))
    (.put bb (byte (if dir? kind-dir kind-file)))
    (.put bb (or guid (byte-array 16)))
    (.putLong bb (long (or len 0)))
    (.putLong bb (long (or mtime 0)))
    (.put bb n)
    (.array bb)))

(defn- decode-entry [^MemorySegment v]
  (let [kind (.get v ValueLayout/JAVA_BYTE 1)
        dir? (= kind-dir kind)]
    {:dir?  dir?
     :guid  (when-not dir? (.toArray (.asSlice v 2 16) ValueLayout/JAVA_BYTE))
     :len   (l/le-u64 v 18)
     :mtime (l/le-u64 v 26)
     :name  (utf8-str v 34)}))

(defn get-entry
  "Layer `layer`'s entry at folded path `folded`, or nil."
  [store txn layer ^String folded]
  (when-let [v (l/get-seg txn (dbi store "entries") (entry-key layer folded))]
    (decode-entry v)))

(defn put-entry! [store txn layer ^String folded entry]
  (l/put! txn (dbi store "entries") (entry-key layer folded) (encode-entry entry)))

(defn del-entry! [store txn layer ^String folded]
  (l/del! txn (dbi store "entries") (entry-key layer folded)))

(defn children
  "[[folded-path entry] …] directly under folded directory `dir`, skipping
  over subtrees (a path after its child's name and a /, seek past them)."
  [store txn layer ^String dir]
  (let [base   (if (= "" dir) "" (str dir "/"))
        prefix (entry-key layer base)
        more?  (l/prefix? prefix)
        np     (alength prefix)]
    (with-open [c (Lmdb/cursor (long txn) (int (dbi store "entries")))]
      (loop [ok (.seek c prefix) out []]
        (if (and ok (more? (.key c)))
          (let [tail  (utf8-str (.key c) np)
                slash (.indexOf tail "/")]
            (cond
              (= "" tail) (recur (.next c) out)                      ; the directory itself
              (neg? slash) (let [out (conj out [(str base tail) (decode-entry (.val c))])]  ; before the cursor moves
                             (recur (.next c) out))
              ;; deeper: jump past the child's subtree ('0' sorts right after '/')
              :else (recur (.seek c (entry-key layer (str base (subs tail 0 slash) "0"))) out)))
          out)))))

(defn subtree
  "[[folded-path entry] …] of `dir` and everything under it."
  [store txn layer ^String dir]
  (let [self   (when-let [e (get-entry store txn layer dir)] [[dir e]])
        prefix (entry-key layer (if (= "" dir) "" (str dir "/")))
        np     (alength (entry-key layer ""))]
    (into (vec self)
          (l/reduce-range txn (dbi store "entries") prefix (l/prefix? prefix)
                          (fn [acc k v]
                            (let [p (utf8-str k np)]
                              (if (= p dir) acc (conj acc [p (decode-entry v)]))))
                          []))))

;; --- layers ------------------------------------------------------------------------------

(defn- get-smeta [store txn ^String k]
  (when-let [v (l/get-seg txn (dbi store "smeta") (utf8 k))] (l/le-u64 v 0)))

(defn layer-id
  "The id of layer `layer-name`, or nil."
  [store txn ^String layer-name]
  (when-let [v (l/get-seg txn (dbi store "layers") (utf8 layer-name))] (l/le-u64 v 0)))

(defn create-layer!
  "In a write txn: layer `layer-name`'s id, creating it (with its root) if needed."
  [store txn ^String layer-name]
  (or (layer-id store txn layer-name)
      (let [id (long (or (get-smeta store txn "next_layer_id") 1))]
        (l/put! txn (dbi store "smeta") (utf8 "next_layer_id") (.array (.putLong (le 8) (inc id))))
        (l/put! txn (dbi store "layers") (utf8 layer-name) (.array (.putLong (le 8) id)))
        (put-entry! store txn id "" {:dir? true :name ""})
        id)))

(defn layer-names [store txn]
  (l/reduce-range txn (dbi store "layers") nil (constantly true)
                  (fn [acc k _] (conj acc (utf8-str k 0)))
                  []))

(defn drop-layer!
  "In a write txn: removes layer `layer-name`'s row (its entries are the caller's)."
  [store txn ^String layer-name]
  (l/del! txn (dbi store "layers") (utf8 layer-name)))

;; --- live files and cache files -------------------------------------------------------------

(defn- le-pair
  "Two u64s, little-endian."
  ^bytes [a b]
  (.array (doto (le 16) (.putLong (long a)) (.putLong (long b)))))

(defn get-live [store txn ^bytes k]
  (when-let [v (l/get-seg txn (dbi store "live") k)]
    {:len (l/le-u64 v 0) :mtime (l/le-u64 v 8)}))

(defn put-live! [store txn ^bytes k {:keys [len mtime]}]
  (l/put! txn (dbi store "live") k (le-pair len mtime)))

(defn del-live! [store txn ^bytes k] (l/del! txn (dbi store "live") k))

(defn live-keys [store txn]
  (l/reduce-range txn (dbi store "live") nil (constantly true)
                  (fn [acc ^MemorySegment k _] (conj acc (.toArray k ValueLayout/JAVA_BYTE))) []))

(defn- decode-cache-file [^MemorySegment v]
  {:access (l/le-u64 v 0) :bytes (l/le-u64 v 8)})

(defn get-cache-file [store txn ^bytes k]
  (some-> (l/get-seg txn (dbi store "cache_files") k) decode-cache-file))

(defn put-cache-file! [store txn ^bytes k {:keys [access] n :bytes}]
  (l/put! txn (dbi store "cache_files") k (le-pair access n)))

(defn del-cache-file! [store txn ^bytes k] (l/del! txn (dbi store "cache_files") k))

(defn- encode-origin ^bytes [{:keys [^String source root ^String vpath size mtime]}]
  (let [s (utf8 source) p (utf8 vpath) bb (le (+ 28 (alength s) (alength p)))]
    (.putLong bb (long root)) (.putLong bb (long size)) (.putLong bb (long mtime))
    (.putInt bb (alength s)) (.put bb s) (.put bb p)
    (.array bb)))

(defn- decode-origin [^MemorySegment v]
  (let [n (l/le-u32 v 24)]
    {:root (l/le-u64 v 0) :size (l/le-u64 v 8) :mtime (l/le-u64 v 16)
     :source (String. ^bytes (.toArray (.asSlice v 28 n) ValueLayout/JAVA_BYTE) StandardCharsets/UTF_8)
     :vpath (utf8-str v (+ 28 n))}))

(defn get-cache-origin
  "Where cache file `k`'s content comes from, {:source :root :vpath :size
  :mtime} (cache-key's arguments), or nil when it wasn't recorded (a cache
  file made before origins were)."
  [store txn ^bytes k]
  (some-> (l/get-seg txn (dbi store "cache_origins") k) decode-origin))

(defn put-cache-origin! [store txn ^bytes k origin]
  (l/put! txn (dbi store "cache_origins") k (encode-origin origin)))

(defn del-cache-origin! [store txn ^bytes k] (l/del! txn (dbi store "cache_origins") k))

(defn cache-files
  "[[key {:access :bytes}] …], every cache file."
  [store txn]
  (l/reduce-range txn (dbi store "cache_files") nil (constantly true)
                  (fn [acc ^MemorySegment k v]
                    (conj acc [(.toArray k ValueLayout/JAVA_BYTE) (decode-cache-file v)]))
                  []))

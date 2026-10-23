(ns aether.director.provider.cached
  "A pull-through cache over a slow, immutable source (a CDN depot, an
  archive download, a network share): aether-storage's CachedSource.

  Metadata (stat, listings, spelling) is the source's. A file's content is
  cached as the store file \"C\" + a hash of the source key, path, size and
  mtime, so a changed file is a different cache file. Reads go RAM tier,
  then store, then source: a miss fetches a whole unit (`:unit-bytes`,
  rounded up to whole blocks) from the source and stores it; concurrent
  misses on one unit wait for one fetch. The cache_files row (last access,
  size) is what eviction (aether.director.storage/enforce-cache-budget!)
  goes by; the cache_origins row (what the key was hashed from) says which
  source the file can be fetched from again, for the collection
  (aether.director.storage/gc-cache!). It's written with the cache file,
  and, for one made before origins were recorded, when it's next opened."
  (:require [aether.director.path :as path]
            [aether.director.proto :as p]
            [aether.director.provider :as prov]
            [aether.director.storage.catalog :as cat]
            [aether.director.storage.ram :as ram]
            [aether.director.store :as store])
  (:import (aether.director.provider Stat)
           (java.io IOException)
           (java.lang.foreign MemorySegment ValueLayout)
           (java.util Arrays HexFormat)
           (java.util.concurrent CompletableFuture ConcurrentHashMap ExecutionException)
           (java.util.concurrent.atomic AtomicBoolean)
           (java.util.function BiFunction)))

(set! *warn-on-reflection* true)

(defn- minute ^long [] (quot (System/currentTimeMillis) 60000))
(defn- hex ^String [^bytes b] (.formatHex (HexFormat/of) b))

(defn- touch!
  "Records an access of cache file `ck` (at most one commit a minute per file)."
  [{:keys [storage ^ConcurrentHashMap touched]} ^bytes ck size]
  (let [now (minute) k (hex ck)]
    (when (not= now (long (or (.put touched k now) -1)))
      (let [st (:store storage)]
        (store/write! st (fn [txn]
                           (when (store/file-len st txn (cat/cache-file-id ck))
                             (cat/put-cache-file! st txn ck {:access now :bytes size}))))))))

(defn- record-origin!
  "Records where cache file `ck` comes from when its row is missing: a cache
  file made before origins were recorded is adopted at its next open."
  [{:keys [storage]} ^bytes ck origin]
  (let [st (:store storage)]
    (when-not (store/read* st (fn [txn] (cat/get-cache-origin st txn ck)))
      (store/write! st (fn [txn]
                         (when (and (store/file-len st txn (cat/cache-file-id ck))
                                    (not (cat/get-cache-origin st txn ck)))
                           (cat/put-cache-origin! st txn ck origin)))))))

(defn- fetch-unit!
  "Fetches blocks [b0, b1) of the file from the source handle and stores
  them (creating the cache file, and its origin row, at its first fetch).
  Returns the blocks."
  [{:keys [storage ^ConcurrentHashMap origins]} src ^bytes ck ^bytes fid size b0 b1]
  (let [size (long size) b0 (long b0) b1 (long b1)
        st    (:store storage)
        bs    (long (:block-size st))
        start (* b0 bs)
        n     (- (min size (* b1 bs)) start)
        buf   (byte-array n)
        got   (long (prov/read-into src (MemorySegment/ofArray buf) 0 start n))]
    (when (not= got n) (throw (ex-info "the source returned a short read" {:status p/io-error})))
    (when-not (:len (store/stat st fid))
      (store/set-len! st fid size {:also (fn [txn]
                                           (cat/put-cache-file! st txn ck {:access (minute) :bytes size})
                                           (when-let [o (.get origins (hex ck))] (cat/put-cache-origin! st txn ck o)))}))
    (store/write-blocks! st fid b0 buf)
    (vec (for [b (range b0 b1)]
           (let [off (* (- b b0) bs)] (Arrays/copyOfRange buf (int off) (int (min n (+ off bs)))))))))

(defn- block!
  "Block `b`: from the RAM tier, the store, or (fetching its unit) the source."
  ^bytes [{:keys [storage ^ConcurrentHashMap inflight unit-blocks] :as cache} src ^bytes ck ^bytes fid size b]
  (let [size (long size) b (long b)
        st (:store storage)
        r  (:ram storage)
        bs (long (:block-size st))]
    (or (ram/get-block r fid b)
        (let [blen (min bs (- size (* b bs)))
              buf  (byte-array blen)
              res  (store/read! st fid (* b bs) (MemorySegment/ofArray buf))]
          (when (and res (empty? (:missing res)))
            (ram/put-block! r fid b buf)
            buf))
        (let [unit (long unit-blocks)
              b0   (* unit (quot b unit))
              b1   (min (quot (+ size (dec bs)) bs) (+ b0 unit))
              k    (str (hex fid) ":" b0)
              mine (CompletableFuture.)
              ^CompletableFuture f (or (.putIfAbsent inflight k mine) mine)]
          (when (identical? f mine)
            (try (let [blocks (fetch-unit! cache src ck fid size b0 b1)]
                   (doseq [[i blk] (map-indexed vector blocks)] (ram/put-block! r fid (+ b0 i) blk))
                   (.complete mine blocks))
                 (catch Throwable t (.completeExceptionally mine t))
                 (finally (.remove inflight k))))
          (nth (try (.get f) (catch ExecutionException e (throw (.getCause e))))
               (- b b0))))))

(deftype CachedHandle [cache src ^bytes ck ^bytes fid ^long size ^long mtime ^AtomicBoolean closed]
  prov/Handle
  (handle-stat [_] (Stat. false size mtime))
  (immutable? [_] true)
  (read-into [_ dst dst-off pos len]
    (let [bs  (long (:block-size (:store (:storage cache))))
          pos (long pos)
          end (min size (+ pos (long len)))
          ^MemorySegment dst dst]
      (try
        (loop [at pos]
          (if (>= at end)
            (max 0 (- end pos))
            (let [b    (quot at bs)
                  boff (- at (* b bs))
                  n    (min (- bs boff) (- end at))
                  ^bytes blk (block! cache src ck fid size b)]
              (MemorySegment/copy blk (int boff) dst ValueLayout/JAVA_BYTE (+ (long dst-off) (- at pos)) (int n))
              (recur (+ at n)))))
        (catch clojure.lang.ExceptionInfo e (long (or (:status (ex-data e)) p/io-error)))
        (catch IOException _ p/io-error))))
  (write-from [_ _ _ _ _] p/read-only)
  (truncate! [_ _] p/read-only)
  (close! [_]
    (when (.compareAndSet closed false true)
      (prov/close! src)
      (let [^ConcurrentHashMap opens (:opens (:storage cache))]
        (.computeIfPresent opens (hex ck)
                           (reify BiFunction
                             (apply [_ _ n] (let [n (dec (long n))] (when (pos? n) n)))))))))

(defrecord Cached [storage source ^String source-key ^long root ^long unit-blocks
                   ^ConcurrentHashMap inflight ^ConcurrentHashMap touched ^ConcurrentHashMap origins]
  prov/Provider
  (stat [_ vp] (prov/stat source vp))
  (list-dir [_ vp] (prov/list-dir source vp))
  (spelling [_ vp] (prov/spelling source vp))
  (open [this vp flags]
    (if-not (zero? (bit-and (long flags) (bit-or p/open-write p/open-create p/open-trunc p/open-append)))
      p/read-only
      (let [h (prov/open source vp flags)]
        (if (number? h)
          h
          (let [^Stat s (prov/handle-stat h)]
            (if (.-dir? s)
              h
              (let [fvp (path/fold vp)
                    ck  (cat/cache-key source-key root fvp (.-size s) (.-mtime s))
                    fid (cat/cache-file-id ck)
                    origin {:source source-key :root root :vpath fvp :size (.-size s) :mtime (.-mtime s)}
                    first-open? (nil? (.putIfAbsent origins (hex ck) origin))]
                (.merge ^ConcurrentHashMap (:opens storage) (hex ck) 1 +)
                (when (:len (store/stat (:store storage) fid))
                  (touch! this ck (.-size s))
                  (when first-open? (record-origin! this ck origin)))
                (CachedHandle. this h ck fid (.-size s) (.-mtime s) (AtomicBoolean.)))))))))
  (mkdir! [_ _] p/read-only)
  (delete! [_ _] p/read-only)
  (rename! [_ _ _] p/read-only))

(defn cached
  "`source` (a Provider whose files never change), cached in `storage`
  ({:store :ram :opens}). `source-key` names the source (it's part of each
  cache file's key: two sources must not share one). Options: :root (the
  root id it serves, part of the key), :unit-bytes (what a miss fetches,
  default 1 MiB, at most 64 MiB)."
  [storage source ^String source-key & [{:keys [root unit-bytes] :or {root 0 unit-bytes (* 1024 1024)}}]]
  (let [bs (long (:block-size (:store storage)))]
    (->Cached storage source source-key root
              (max 1 (quot (+ (min (long unit-bytes) (* 64 1024 1024)) (dec bs)) bs))
              (ConcurrentHashMap.) (ConcurrentHashMap.) (ConcurrentHashMap.))))

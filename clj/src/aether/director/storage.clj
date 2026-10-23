(ns aether.director.storage
  "Storage: the block store with a catalog and a RAM tier, serving layers
  (writable namespaces, aether.director.provider.layer), cache files (a
  pull-through cache of slow sources, aether.director.provider.cached) and
  live files (layer files frozen under content-derived keys). After
  aether-vfs's aether-storage, with one structural difference: the catalog
  is in the store's own LMDB environment, so every change is one durable
  commit and there's nothing to reconcile at open.

    (def s (storage/open dir))
    (storage/layer s \"saves\")          ; a Provider; with compose/overlay over game content
    (storage/freeze s \"saves\")         ; {vpath live-key}"
  (:require [aether.director.path :as path]
            [aether.director.provider.cached :as cached]
            [aether.director.provider.layer :as layer]
            [aether.director.storage.catalog :as cat]
            [aether.director.storage.ram :as ram]
            [aether.director.store :as store])
  (:import (aether.director Codec)
           (java.nio ByteBuffer ByteOrder)
           (java.util HexFormat)
           (java.util.concurrent ConcurrentHashMap)))

(set! *warn-on-reflection* true)

(def defaults
  {:ram-bytes   (* 256 1024 1024)
   :cache-bytes (* 32 1024 1024 1024)})

(defn open
  "Opens (creating) storage in directory `dir`. Options: the store's
  (aether.director.store/defaults), plus :ram-bytes (the RAM tier) and
  :cache-bytes (the cache files' budget, enforced by enforce-cache-budget!)."
  [dir & [opts]]
  (let [cfg (merge store/defaults defaults {:page-size 8192} opts)
        st  (store/open dir (assoc cfg :extra-dbs cat/dbs))]
    {:store st
     :ram (ram/ram (:ram-bytes cfg) (:block-size st))
     :cfg cfg
     :layers (atom {})
     :opens (ConcurrentHashMap.)}))                       ; cache file key (hex) -> open handles

(defn close
  "Commits open layer files and closes."
  [{:keys [store layers]}]
  (doseq [l (vals @layers)] (layer/commit-open! l))
  (store/close store))

;; --- layers -------------------------------------------------------------------------------

(defn layer
  "The provider of layer `layer-name`, created if it doesn't exist."
  [{:keys [store layers] :as s} ^String layer-name]
  (or (get @layers layer-name)
      (let [id (store/write! store (fn [txn] (cat/create-layer! store txn layer-name)))]
        (-> (swap! layers (fn [m]
                            (if (m layer-name)
                              m
                              (assoc m layer-name (layer/layer (select-keys s [:store :ram]) id layer-name)))))
            (get layer-name)))))

(defn layers
  "Every layer's name."
  [{:keys [store]}]
  (store/read* store (fn [txn] (cat/layer-names store txn))))

(defn delete-layer!
  "Deletes layer `layer-name` and its files. Refused (:layer-in-use) while one is open."
  [{:keys [store layers]} ^String layer-name]
  (when-let [l (get @layers layer-name)]
    (when (pos? (layer/open-files l))
      (throw (ex-info (str "layer " layer-name " has open files") {::error :layer-in-use}))))
  (store/write! store
                (fn [txn]
                  (when-let [id (cat/layer-id store txn layer-name)]
                    (doseq [[p e] (cat/subtree store txn id "")]
                      (when (:guid e) (store/delete-file! store txn (cat/layer-file-id (:guid e))))
                      (cat/del-entry! store txn id p))
                    (cat/drop-layer! store txn layer-name)
                    true)))
  (swap! layers dissoc layer-name)
  nil)

(defn put-files!
  "Writes whole files into layer `layer-name` ([[vpath bytes] …], spelled
  paths; missing directories created), replacing any there, in one commit."
  [{:keys [store] :as s} ^String layer-name files]
  (let [id   (:id (layer s layer-name))
        now  (quot (System/currentTimeMillis) 1000)
        plan (mapv (fn [[vp ^bytes data]] [vp (path/fold vp) (cat/new-guid) data]) files)]
    (store/put-files! store
                      (mapv (fn [[_ _ guid data]] [(cat/layer-file-id guid) data]) plan)
                      (fn [txn]
                        (doseq [[vp folded guid ^bytes data] plan]
                          (doseq [[f nm] (layer/ancestors-of vp)]
                            (when-not (cat/get-entry store txn id f)
                              (cat/put-entry! store txn id f {:dir? true :name nm :mtime now})))
                          (when-let [old (cat/get-entry store txn id folded)]
                            (when (:guid old) (store/delete-file! store txn (cat/layer-file-id (:guid old)))))
                          (cat/put-entry! store txn id folded {:guid guid :len (alength data) :mtime now
                                                               :name (path/file-name vp)}))))
    nil))

;; --- live files --------------------------------------------------------------------------------

(def ^:private live-context "aether-storage 2026-10-13 live key v1")

(defn key-string ^String [^bytes k] (str "live-" (.formatHex (HexFormat/of) k)))
(defn parse-key ^bytes [^String s] (.parseHex (HexFormat/of) (subs s 5)))

(defn- live-key
  "BLAKE3 (derive-key) of the layer, the file's GUID, length and mtime, and
  its content digest: the same file frozen twice gets the same key."
  ^bytes [layer-id ^bytes guid len mtime ^bytes digest]
  (let [bb (.order (ByteBuffer/allocate (+ 8 16 8 8 32)) ByteOrder/LITTLE_ENDIAN)]
    (.putLong bb (long layer-id)) (.put bb guid) (.putLong bb (long len)) (.putLong bb (long mtime)) (.put bb digest)
    (-> (Codec/derive live-context) (.update (.array bb)) (.finish 16))))

(defn freeze
  "Freezes layer `layer-name`'s files (those under `prefix`, default all) into
  live files: immutable copies, sharing their blocks, under keys derived from
  the file and its content. Idempotent: an unchanged file keeps its key.
  Returns {vpath key-string}."
  [{:keys [store] :as s} ^String layer-name & [{:keys [prefix] :or {prefix ""}}]]
  (let [l  (layer s layer-name)
        id (:id l)
        _  (layer/commit-open! l)]
    (store/write! store
                  (fn [txn]
                    (let [files   (for [[p e] (cat/subtree store txn id (path/fold prefix)) :when (:guid e)] [p e])
                          digests (store/content-digests-in store txn (map (fn [[_ e]] (cat/layer-file-id (:guid e))) files))]
                      (into {}
                            (for [[[p e] [len digest]] (map vector files digests) :when digest]
                              (let [k (live-key id (:guid e) len (:mtime e) digest)]
                                (when-not (cat/get-live store txn k)
                                  (store/delete-file! store txn (cat/live-file-id k))     ; an orphan, if any
                                  (cat/put-live! store txn k {:len len :mtime (:mtime e)})
                                  (store/clone-in! store txn [[(cat/layer-file-id (:guid e)) (cat/live-file-id k)]]))
                                [p (key-string k)]))))))))

(defn live-info
  "{:len :mtime} of live file `ks` (a key string), or nil."
  [{:keys [store]} ^String ks]
  (store/read* store (fn [txn] (cat/get-live store txn (parse-key ks)))))

(defn live-keys [{:keys [store]}]
  (store/read* store (fn [txn] (mapv key-string (cat/live-keys store txn)))))

(defn delete-live!
  "Deletes live file `ks` (a key string); false if there was none."
  [{:keys [store]} ^String ks]
  (let [k (parse-key ks)]
    (store/write! store (fn [txn]
                          (when (cat/get-live store txn k)
                            (cat/del-live! store txn k)
                            (store/delete-file! store txn (cat/live-file-id k))
                            true)))))

(defn read-live!
  "Reads live file `ks` (a key string) from `offset` into MemorySegment
  `dst`: as store/read!, through the RAM tier's store."
  [{:keys [store]} ^String ks offset dst]
  (store/read! store (cat/live-file-id (parse-key ks)) (long offset) dst))

;; --- the cache -----------------------------------------------------------------------------------

(defn cached
  "`source` (a Provider whose files never change, slow to read) behind a
  pull-through cache in this storage: see aether.director.provider.cached."
  [s source ^String source-key & [opts]]
  (cached/cached (select-keys s [:store :ram :opens]) source source-key opts))

(defn stored-bytes
  "For each [source-key vpath size mtime] of `items` (the cache files of
  `cached` sources, :root as its option, default 0), the bytes of its cache
  file that are stored (0 when there's none), in one snapshot: a vector, in
  their order. See store/stored-bytes-in."
  [{:keys [store]} items & [{:keys [root] :or {root 0}}]]
  (store/read* store
               (fn [txn]
                 (mapv (fn [[source-key vp size mtime]]
                         (store/stored-bytes-in store txn
                                                (cat/cache-file-id (cat/cache-key source-key root (path/fold vp) size mtime))))
                       items))))

(defn cache-usage
  "{:files n :bytes logical bytes} of the cache files."
  [{:keys [store]}]
  (let [fs (store/read* store (fn [txn] (cat/cache-files store txn)))]
    {:files (count fs) :bytes (reduce + (map (comp :bytes second) fs))}))

(defn- evict!
  "Deletes cache files, least recently used first, until (done? freed) or
  none are left; open ones are kept. Returns the bytes freed."
  [{:keys [store ram ^ConcurrentHashMap opens]} done?]
  (let [fs (sort-by (comp :access second) (store/read* store (fn [txn] (cat/cache-files store txn))))]
    (loop [[[k {n :bytes}] & more] fs freed 0]
      (if (or (nil? k) (done? freed))
        freed
        (if (.containsKey opens (.formatHex (HexFormat/of) ^bytes k))
          (recur more freed)
          (do (store/write! store (fn [txn]
                                    (cat/del-cache-file! store txn k)
                                    (cat/del-cache-origin! store txn k)
                                    (store/delete-file! store txn (cat/cache-file-id k))))
              (ram/invalidate! ram (cat/cache-file-id k))
              (recur more (+ freed (long n)))))))))

(defn enforce-cache-budget!
  "When the cache files hold more than :cache-bytes, deletes the least
  recently used (not open) down to 90% of it, then compacts. Returns the
  bytes freed."
  [{:keys [store cfg] :as s}]
  (let [budget (long (:cache-bytes cfg))
        used   (long (:bytes (cache-usage s)))]
    (if (<= used budget)
      0
      (let [target (- used (long (* 0.9 budget)))
            freed  (evict! s #(>= (long %) target))]
        (store/compact! store {:min-garbage-ratio 0.25})
        freed))))

(defn clear-cache!
  "Deletes every cache file that isn't open. Returns the bytes freed."
  [s]
  (evict! s (constantly false)))

;; --- collecting the cache -----------------------------------------------------------------------

(defn cache-entries
  "Every cache file: [{:key (hex) :bytes :access (minutes) :open? :origin
  {:source :root :vpath :size :mtime}, nil when not recorded}]."
  [{:keys [store ^ConcurrentHashMap opens]}]
  (store/read* store
               (fn [txn]
                 (mapv (fn [[^bytes k {:keys [access] n :bytes}]]
                         (let [hk (.formatHex (HexFormat/of) k)]
                           {:key hk :bytes n :access access
                            :open? (.containsKey opens hk)
                            :origin (cat/get-cache-origin store txn k)}))
                       (cat/cache-files store txn)))))

(defn adopt-origins!
  "Records the origin of each cache file among `origins` ({:source :root
  :vpath :size :mtime}, what a cached provider would key it by; :root
  defaults to 0) that exists without one: a cache file made before origins
  were recorded, which gc-cache! otherwise never collects. Returns how many
  were adopted."
  [{:keys [store]} origins]
  (reduce + 0
          (for [batch (partition-all 1024 origins)]
            (store/write! store
                          (fn [txn]
                            (reduce (fn [n {:keys [source root vpath size mtime] :or {root 0}}]
                                      (let [o {:source source :root root :vpath (path/fold vpath) :size size :mtime mtime}
                                            k (cat/cache-key source root (:vpath o) size mtime)]
                                        (if (and (cat/get-cache-file store txn k) (not (cat/get-cache-origin store txn k)))
                                          (do (cat/put-cache-origin! store txn k o) (inc n))
                                          n)))
                                    0 batch))))))

(defn- verdict
  "Why `entry` is kept, or :collect."
  [{:keys [open? origin]} keep? retrievable? unrecorded?]
  (cond open?                       :open
        (nil? origin)               (if unrecorded? :collect :unknown)
        (keep? origin)              :in-use
        (not (retrievable? origin)) :unretrievable
        :else                       :collect))

(defn gc-cache!
  "Collects the cache: deletes each cache file that the host no longer
  needs and can fetch again, then compacts the packs that frees space in.
  Nothing else is touched: layers (what a program wrote), live files, and
  any cache file that is open, whose origin wasn't recorded (made before
  origins were, and not opened since), that `keep?` (fn [origin]) wants, or
  that `retrievable?` (fn [origin]) can't vouch for, are all kept. Origins
  are {:source :root :vpath :size :mtime}: the source key, root and vpath a
  cached provider was made with. Options: :dry-run? (decide, delete
  nothing); :unrecorded? (collect the files whose origin isn't recorded too:
  for a host that has adopted the origins of everything it uses first
  (adopt-origins!), so they are files nothing uses, though whether they can
  be fetched again can't be checked). Returns {:collected {:files :bytes} :kept {reason {:files
  :bytes}} :compacted {:packs :freed}}, reasons :open :unknown :in-use
  :unretrievable; bytes are logical (before dedup and compression)."
  [{:keys [store ram] :as s} {:keys [keep? retrievable? dry-run? unrecorded?]}]
  (let [entries (cache-entries s)
        decided (group-by #(verdict % keep? retrievable? unrecorded?) entries)
        tally   (fn [es] {:files (count es) :bytes (reduce + 0 (map :bytes es))})
        doomed  (get decided :collect)]
    (when-not dry-run?
      (doseq [batch (partition-all 256 doomed)]
        (store/write! store
                      (fn [txn]
                        (doseq [{:keys [key]} batch
                                :let [k (.parseHex (HexFormat/of) ^String key)]]
                          (cat/del-cache-file! store txn k)
                          (cat/del-cache-origin! store txn k)
                          (store/delete-file! store txn (cat/cache-file-id k)))))
        (doseq [{:keys [key]} batch]
          (ram/invalidate! ram (cat/cache-file-id (.parseHex (HexFormat/of) ^String key))))))
    {:collected (tally doomed)
     :kept      (into {} (for [[why es] (dissoc decided :collect)] [why (tally es)]))
     :compacted (when (and (seq doomed) (not dry-run?))
                  (select-keys (store/compact! store {:min-garbage-ratio 0.05}) [:packs :freed]))}))

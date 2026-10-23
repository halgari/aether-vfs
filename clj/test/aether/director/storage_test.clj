(ns aether.director.storage-test
  "Storage: layers (beyond the conformance suite), freezing into live files,
  and the pull-through cache with eviction."
  (:require [aether.director.path :as path]
            [aether.director.proto :as p]
            [aether.director.provider :as prov]
            [aether.director.provider.compose :as compose]
            [aether.director.provider.tree :as tree]
            [aether.director.storage :as storage]
            [aether.director.storage.catalog :as cat]
            [aether.director.store :as store]
            [clojure.test :refer [deftest is testing]])
  (:import (aether.director.provider Stat)
           (java.lang.foreign MemorySegment ValueLayout)
           (java.nio.file Files)
           (java.util Arrays Random)
           (java.util.concurrent Executors Future)
           (java.util.concurrent.atomic AtomicLong)))

(def bs 4096)

(defn- tmp [] (.toFile (Files/createTempDirectory "aether-storage" (make-array java.nio.file.attribute.FileAttribute 0))))
(defn- open [dir & [opts]] (storage/open dir (merge {:block-size bs :threads 2 :ram-bytes (* 1024 1024)} opts)))
(defmacro ^:private with-storage [[s dir & [opts]] & body]
  `(let [~s (open ~dir ~opts)] (try ~@body (finally (storage/close ~s)))))

(defn- noise ^bytes [seed n] (let [b (byte-array n)] (.nextBytes (Random. seed) b) b))
(def wcreate (bit-or p/open-write p/open-create))

(defn- read-all ^bytes [prov vp]
  (let [h (prov/open prov vp p/open-read)]
    (when-not (number? h)
      (try (let [n (.-size ^Stat (prov/handle-stat h)) b (byte-array n)]
             (prov/read-into h (MemorySegment/ofArray b) 0 0 n)
             b)
           (finally (prov/close! h))))))

(defn- write-at! [h pos ^bytes b] (prov/write-from h (MemorySegment/ofArray b) 0 pos (alength b)))

;; --- layers ----------------------------------------------------------------------------------

(deftest layers-persist
  (let [dir (tmp) big (noise 1 (* 100 bs))]
    (with-storage [s dir]
      (let [l (storage/layer s "saves")
            h (prov/open l "Profiles/Main/Save1.ess" wcreate)]
        (testing "a write of more blocks than a handle keeps commits as it goes"
          (write-at! h 0 big)
          (is (= (alength big) (.-size ^Stat (prov/stat l "profiles/main/save1.ess")))))
        (prov/close! h)))
    (with-storage [s dir]
      (let [l (storage/layer s "saves")]
        (is (= ["saves"] (storage/layers s)))
        (is (Arrays/equals big (read-all l "profiles/main/save1.ess")))
        (is (= "Profiles/Main/Save1.ess" (prov/spelling l "profiles/main/save1.ess")))
        (is (empty? (:problems (store/verify (:store s)))))))))

(deftest open-files
  (with-storage [s (tmp)]
    (let [l (storage/layer s "w")]
      (testing "handles on one file share what's written, before any commit"
        (let [a (prov/open l "f.bin" wcreate) b (prov/open l "F.BIN" p/open-read)]
          (write-at! a 0 (.getBytes "abc"))
          (is (= "abc" (String. (let [buf (byte-array 3)] (prov/read-into b (MemorySegment/ofArray buf) 0 0 3) buf))))
          (prov/close! a) (prov/close! b)))
      (testing "growth reads as zeros, before and after the commit"
        (let [h (prov/open l "g.bin" wcreate)]
          (write-at! h (+ (* 3 bs) 10) (.getBytes "end"))
          (let [buf (byte-array (+ (* 3 bs) 13))]
            (prov/read-into h (MemorySegment/ofArray buf) 0 0 (alength buf))
            (is (every? zero? (take (+ (* 3 bs) 10) buf))))
          (prov/close! h)
          (let [b (read-all l "g.bin")]
            (is (= (+ (* 3 bs) 13) (alength b)))
            (is (every? zero? (take (+ (* 3 bs) 10) b)))
            (is (= "end" (String. (Arrays/copyOfRange b (int (+ (* 3 bs) 10)) (alength b))))))))
      (testing "a file deleted while open stays readable through its handle, then goes"
        (storage/put-files! s "w" [["gone.txt" (.getBytes "still here")]])
        (let [h (prov/open l "gone.txt" p/open-read)]
          (is (= p/ok (prov/delete! l "gone.txt")))
          (is (nil? (prov/stat l "gone.txt")))
          (let [buf (byte-array 10)] (prov/read-into h (MemorySegment/ofArray buf) 0 0 10)
            (is (= "still here" (String. buf))))
          (let [before (count (store/file-ids (:store s)))]
            (prov/close! h)
            (is (= (dec before) (count (store/file-ids (:store s))))))))
      (testing "a directory renamed while a file in it is open: the commit follows the file"
        (let [h (prov/open l "dir/x.txt" wcreate)]
          (write-at! h 0 (.getBytes "one"))
          (is (= p/ok (prov/rename! l "dir" "Moved")))
          (write-at! h 3 (.getBytes "two"))
          (prov/close! h)
          (is (= "onetwo" (String. (read-all l "moved/x.txt"))))
          (is (= 6 (.-size ^Stat (prov/stat l "moved/x.txt"))))))
      (testing "renaming onto a file replaces it; into its own subtree is refused"
        (storage/put-files! s "w" [["r1.txt" (.getBytes "1")] ["r2.txt" (.getBytes "2")]])
        (is (= p/ok (prov/rename! l "r1.txt" "r2.txt")))
        (is (= "1" (String. (read-all l "r2.txt"))))
        (is (= p/bad-request (prov/rename! l "moved" "moved/inner"))))
      (testing "put-files! replaces a file, dropping the old data"
        (storage/put-files! s "w" [["p.txt" (noise 5 bs)]])
        (storage/put-files! s "w" [["p.txt" (.getBytes "new")]])
        (is (= "new" (String. (read-all l "p.txt")))))
      (is (empty? (:problems (store/verify (:store s))))))))

(deftest delete-layer
  (with-storage [s (tmp)]
    (storage/put-files! s "tmp" [["a" (noise 1 bs)] ["d/b" (noise 2 bs)]])
    (let [h (prov/open (storage/layer s "tmp") "a" p/open-read)]
      (is (= :layer-in-use (try (storage/delete-layer! s "tmp") nil
                                (catch clojure.lang.ExceptionInfo e (:aether.director.storage/error (ex-data e))))))
      (prov/close! h))
    (storage/delete-layer! s "tmp")
    (is (= [] (storage/layers s)))
    (is (empty? (store/file-ids (:store s))))))

;; --- freezing ----------------------------------------------------------------------------------

(deftest freeze-into-live-files
  (with-storage [s (tmp)]
    (storage/put-files! s "game" [["Data/a.esp" (noise 1 (* 2 bs))] ["b.ini" (.getBytes "x=1")]])
    (let [k1 (storage/freeze s "game")]
      (is (= #{"data/a.esp" "b.ini"} (set (keys k1))))
      (is (every? #(re-matches #"live-[0-9a-f]{32}" %) (vals k1)))
      (testing "idempotent: an unchanged file keeps its key"
        (is (= k1 (storage/freeze s "game"))))
      (testing "a changed file gets a new key; the old live file is unchanged"
        (let [l (storage/layer s "game") h (prov/open l "b.ini" p/open-write)]
          (write-at! h 0 (.getBytes "x=2")) (prov/close! h))
        (let [k2 (storage/freeze s "game")]
          (is (= (get k1 "data/a.esp") (get k2 "data/a.esp")))
          (is (not= (get k1 "b.ini") (get k2 "b.ini")))
          (let [buf (byte-array 3)]
            (storage/read-live! s (get k1 "b.ini") 0 (MemorySegment/ofArray buf))
            (is (= "x=1" (String. buf))))))
      (testing "a live file outlives its layer file"
        (prov/delete! (storage/layer s "game") "data/a.esp")
        (let [buf (byte-array (* 2 bs))]
          (is (= {:bytes (* 2 bs) :missing []} (storage/read-live! s (get k1 "data/a.esp") 0 (MemorySegment/ofArray buf))))
          (is (Arrays/equals (noise 1 (* 2 bs)) buf))))
      (is (= {:len 3} (select-keys (storage/live-info s (get k1 "b.ini")) [:len])))
      (is (= 3 (count (storage/live-keys s))))
      (is (storage/delete-live! s (get k1 "data/a.esp")))
      (is (nil? (storage/live-info s (get k1 "data/a.esp"))))
      (is (empty? (:problems (store/verify (:store s))))))))

;; --- the cache -----------------------------------------------------------------------------------

(defn- counting
  "`prov` whose handles count the bytes read through them in `reads`."
  [prov ^AtomicLong reads]
  (reify prov/Provider
    (stat [_ vp] (prov/stat prov vp))
    (list-dir [_ vp] (prov/list-dir prov vp))
    (spelling [_ vp] (prov/spelling prov vp))
    (open [_ vp flags]
      (let [h (prov/open prov vp flags)]
        (if (number? h)
          h
          (reify prov/Handle
            (handle-stat [_] (prov/handle-stat h))
            (immutable? [_] true)
            (read-into [_ dst off pos len]
              (let [n (prov/read-into h dst off pos len)] (.addAndGet reads (long n)) n))
            (write-from [_ _ _ _ _] p/read-only)
            (truncate! [_ _] p/read-only)
            (close! [_] (prov/close! h))))))
    (mkdir! [_ _] p/read-only)
    (delete! [_ _] p/read-only)
    (rename! [_ _ _] p/read-only)))

(defn- source [files] (tree/build (for [[k ^bytes v] files] [k (MemorySegment/ofArray v)])))

(deftest pull-through-cache
  (let [dir   (tmp)
        big   (noise 9 (* 20 bs))
        files {"depot/big.bin" big "depot/small.txt" (.getBytes "small")}
        reads (AtomicLong.)]
    (with-storage [s dir]
      (let [c (storage/cached s (counting (source files) reads) "steam:test" {:unit-bytes (* 4 bs)})]
        (testing "metadata is the source's; writes are refused"
          (is (= (alength big) (.-size ^Stat (prov/stat c "depot/big.bin"))))
          (is (= p/read-only (prov/open c "depot/new" wcreate))))
        (testing "a miss fetches its unit; a second read costs the source nothing"
          (let [h (prov/open c "depot/big.bin" p/open-read) buf (byte-array 100)]
            (prov/read-into h (MemorySegment/ofArray buf) 0 (+ bs 10) 100)
            (is (= (* 4 bs) (.get reads)) "one unit")
            (is (Arrays/equals (Arrays/copyOfRange big (int (+ bs 10)) (int (+ bs 110))) buf))
            (prov/read-into h (MemorySegment/ofArray buf) 0 (* 2 bs) 100)
            (is (= (* 4 bs) (.get reads)))
            (prov/close! h)))
        (testing "the whole file, read back, is the source's"
          (is (Arrays/equals big (read-all c "depot/big.bin")))
          (is (= (alength big) (.get reads)) "each unit fetched once"))))
    (testing "the cache survives a reopen"
      (with-storage [s dir]
        (let [c (storage/cached s (counting (source files) reads) "steam:test" {:unit-bytes (* 4 bs)})]
          (.set reads 0)
          (is (Arrays/equals big (read-all c "depot/big.bin")))
          (is (zero? (.get reads)))
          (is (= {:files 1 :bytes (alength big)} (storage/cache-usage s))))))
    (testing "a different source key is a different cache"
      (with-storage [s dir]
        (let [c (storage/cached s (counting (source files) reads) "steam:other")]
          (.set reads 0)
          (read-all c "depot/small.txt")
          (is (= 5 (.get reads))))))))

(deftest concurrent-misses-share-one-fetch
  (with-storage [s (tmp)]
    (let [reads (AtomicLong.)
          big   (noise 3 (* 8 bs))
          c     (storage/cached s (counting (source {"f" big}) reads) "src" {:unit-bytes (* 8 bs)})
          pool  (Executors/newFixedThreadPool 8)
          fs    (vec (for [_ (range 8)] (.submit pool ^Callable #(Arrays/equals big (read-all c "f")))))]
      (is (every? #(.get ^Future %) fs))
      (.shutdown pool)
      (is (= (* 8 bs) (.get reads))))))

(deftest eviction
  (with-storage [s (tmp) {:cache-bytes (* 10 bs)}]
    (let [reads (AtomicLong.)
          files (into {} (for [i (range 4)] [(str "f" i) (noise (+ 50 i) (* 4 bs))]))
          c     (storage/cached s (counting (source files) reads) "src")]
      (doseq [i (range 4)]
        (read-all c (str "f" i))
        (Thread/sleep 5))
      (is (= (* 16 bs) (:bytes (storage/cache-usage s))))
      (let [h (prov/open c "f0" p/open-read)]
        (is (pos? (storage/enforce-cache-budget! s)))
        (is (<= (long (:bytes (storage/cache-usage s))) (* 9 bs)))
        (testing "an open file isn't evicted"
          (.set reads 0)
          (read-all c "f0")
          (is (zero? (.get reads))))
        (prov/close! h))
      (testing "evicted files are fetched again"
        (.set reads 0)
        (is (Arrays/equals ^bytes (files "f1") (read-all c "f1")))
        (is (pos? (.get reads))))
      (storage/clear-cache! s)
      (is (= 0 (:files (storage/cache-usage s))))
      (is (empty? (:problems (store/verify (:store s))))))))

;; --- collecting the cache ------------------------------------------------------------------------

(defn- reasons [r] (into {} (for [[why {:keys [files]}] (:kept r)] [why files])))

(deftest cache-files-know-where-they-came-from
  (with-storage [s (tmp)]
    (let [c (storage/cached s (source {"Data/a.bsa" (noise 1 (* 2 bs))}) "steam 1 [[2 3]]" {:root 4})]
      (read-all c "Data/a.bsa")
      (is (= [{:source "steam 1 [[2 3]]" :root 4 :vpath "data/a.bsa" :size (* 2 bs)
               :mtime (.-mtime ^Stat (prov/stat c "data/a.bsa"))}]
             (map :origin (storage/cache-entries s)))
          "the arguments its key was hashed from, the vpath folded"))))

(deftest a-cache-file-from-before-origins-is-adopted-when-opened
  (with-storage [s (tmp)]
    (let [files {"f" (noise 2 bs)}
          c (storage/cached s (source files) "src")]
      (read-all c "f")
      ;; as a cache file made before origins were recorded
      (let [st (:store s)]
        (store/write! st (fn [txn] (doseq [[k] (cat/cache-files st txn)] (cat/del-cache-origin! st txn k)))))
      (is (nil? (:origin (first (storage/cache-entries s)))))
      (is (= {:unknown 1} (reasons (storage/gc-cache! s {:keep? (constantly false) :retrievable? (constantly true)})))
          "an unknown origin isn't collected")
      (is (= 1 (:files (:collected (storage/gc-cache! s {:keep? (constantly true) :retrievable? (constantly false)
                                                          :unrecorded? true :dry-run? true}))))
          "unless the host asks for it")
      (is (= 0 (storage/adopt-origins! s [{:source "other" :vpath "f" :size bs :mtime 0}])) "no such cache file")
      (let [mtime (.-mtime ^Stat (prov/stat (source files) "f"))]
        (is (= 1 (storage/adopt-origins! s [{:source "src" :vpath "F" :size bs :mtime mtime}]))
            "a host that knows what the launch reads adopts it (the vpath folded)")
        (is (= 0 (storage/adopt-origins! s [{:source "src" :vpath "f" :size bs :mtime mtime}])) "once"))
      (is (= "src" (:source (:origin (first (storage/cache-entries s))))))
      (let [st (:store s)]
        (store/write! st (fn [txn] (doseq [[k] (cat/cache-files st txn)] (cat/del-cache-origin! st txn k)))))
      (read-all (storage/cached s (source files) "src") "f")
      (is (= "src" (:source (:origin (first (storage/cache-entries s))))) "and so does a new provider's open"))))

(deftest gc-collects-only-what-isnt-needed-and-can-be-fetched-again
  (with-storage [s (tmp)]
    (let [files (into {} (for [i (range 4)] [(str "f" i) (noise (+ 10 i) (* 4 bs))]))
          old   (storage/cached s (source files) "game v1")
          new   (storage/cached s (source files) "game v2")
          gone  (storage/cached s (source files) "archive deleted upstream")
          saves (storage/layer s "loadout/saves")]
      (doseq [c [old new gone] f ["f0" "f1"]] (read-all c f))
      (let [h (prov/open saves "a.sav" wcreate)] (write-at! h 0 (noise 7 bs)) (prov/close! h))
      (let [open-h (prov/open old "f1" p/open-read)
            opts   {:keep?        #(= "game v2" (:source %))
                    :retrievable? #(not= "archive deleted upstream" (:source %))}]
        (testing "a dry run decides and deletes nothing"
          (let [r (storage/gc-cache! s (assoc opts :dry-run? true))]
            (is (= {:files 1 :bytes (* 4 bs)} (:collected r)))
            (is (= {:in-use 2 :unretrievable 2 :open 1} (reasons r)))
            (is (= 6 (:files (storage/cache-usage s))))))
        (testing "collected: not kept, fetchable again, not open"
          (let [r (storage/gc-cache! s opts)]
            (is (= 1 (:files (:collected r))))
            (is (= 5 (:files (storage/cache-usage s))))
            (is (= #{["game v1" "f1"] ["game v2" "f0"] ["game v2" "f1"]
                     ["archive deleted upstream" "f0"] ["archive deleted upstream" "f1"]}
                   (set (map (comp (juxt :source :vpath) :origin) (storage/cache-entries s)))))))
        (prov/close! open-h))
      (testing "a layer is never touched, and the collected file is fetched again"
        (is (Arrays/equals (noise 7 bs) (read-all saves "a.sav")))
        (is (Arrays/equals ^bytes (files "f0") (read-all old "f0"))))
      (is (empty? (:problems (store/verify (:store s))))))))

(defn- pack-bytes [s] (reduce + 0 (map :bytes (:packs (store/stats (:store s))))))

(deftest gc-gives-the-space-back
  ;; small packs, so the collected files' blocks sit in sealed ones
  (with-storage [s (tmp) {:max-pack-size (* 16 bs) :zstd-level 1}]
    (let [old   (storage/cached s (source (into {} (for [i (range 8)] [(str "f" i) (noise (+ 100 i) (* 8 bs))]))) "v1")
          new   (storage/cached s (source {"g" (noise 200 (* 8 bs))}) "v2")]
      (doseq [i (range 8)] (read-all old (str "f" i)))
      (read-all new "g")
      (let [before (pack-bytes s)
            r      (storage/gc-cache! s {:keep? #(= "v2" (:source %)) :retrievable? (constantly true)})]
        (is (= 8 (:files (:collected r))))
        (is (pos? (long (:freed (:compacted r)))))
        (is (< (pack-bytes s) (* 0.5 before)) "the packs the collected files filled are gone from disk")
        (is (Arrays/equals (noise 200 (* 8 bs)) (read-all new "g")) "what's kept reads back")
        (is (empty? (:problems (store/verify (:store s)))))))))

;; --- stored bytes ------------------------------------------------------------------------------

(defn- item [c sk vp] (let [^Stat st (prov/stat c (path/fold vp))] [sk vp (.-size st) (.-mtime st)]))

(defn- ranges-total
  "The bytes cached-ranges reports for item [sk vp size mtime] (root 0)."
  [s [sk vp size mtime]]
  (reduce + 0 (map (fn [[a b]] (- (long b) (long a)))
                   (store/cached-ranges (:store s) (cat/cache-file-id (cat/cache-key sk 0 (path/fold vp) size mtime))))))

(defn- read-at [c vp pos n]
  (let [h (prov/open c vp p/open-read)]
    (try (prov/read-into h (MemorySegment/ofArray (byte-array n)) 0 pos n) (finally (prov/close! h)))))

(deftest stored-bytes
  (with-storage [s (tmp)]
    (let [files {"depot/Empty.bin" (byte-array 0)
                 "depot/untouched.bin" (noise 1 (* 3 bs))
                 "depot/part.bin" (noise 2 (* 20 bs))
                 "depot/full.bin" (noise 3 (+ (* 10 bs) 1234))
                 "depot/tail.bin" (noise 4 (+ (* 5 bs) 7))}
          c     (storage/cached s (source files) "steam:test" {:unit-bytes bs})
          items (mapv #(item c "steam:test" %) (sort (keys files)))
          check (fn [why]
                  (testing why
                    (is (= (mapv #(ranges-total s %) items) (storage/stored-bytes s items)))))]
      (check "nothing cached")
      (read-at c "depot/part.bin" 10 1) (read-at c "depot/part.bin" (* 7 bs) (* 3 bs))
      (read-all c "depot/full.bin")
      (read-at c "depot/tail.bin" (* 5 bs) 7)
      (read-all c "depot/empty.bin")
      (check "empty, partly and fully cached, short last blocks")
      (is (= {"depot/Empty.bin" 0 "depot/untouched.bin" 0 "depot/part.bin" (* 4 bs)
              "depot/full.bin" (+ (* 10 bs) 1234) "depot/tail.bin" 7}
             (zipmap (map second items) (storage/stored-bytes s items))))
      (testing "the key is Cached's: folded vpath, source key, size, mtime and root"
        (let [[sk vp size mtime] (item c "steam:test" "depot/full.bin")]
          (is (= [(+ (* 10 bs) 1234) 0 0 0 0]
                 (storage/stored-bytes s [[sk (.toUpperCase ^String vp) size mtime] ["steam:other" vp size mtime]
                                          [sk vp (inc size) mtime] [sk vp size (inc mtime)] [sk "depot/none" size mtime]])))
          (is (= [0] (storage/stored-bytes s [[sk vp size mtime]] {:root 1})))
          (let [c1 (storage/cached s (source files) "steam:test" {:root 1 :unit-bytes bs})]
            (read-all c1 "depot/full.bin")
            (is (= [size] (storage/stored-bytes s [[sk vp size mtime]] {:root 1})))))))))

(deftest stored-bytes-during-a-fill
  (with-storage [s (tmp)]
    (let [files (into {} (for [i (range 40)] [(str "f" i) (noise (+ 100 i) (+ (* 8 bs) (* 100 i)))]))
          c     (storage/cached s (source files) "src" {:unit-bytes bs})
          items (mapv #(item c "src" %) (sort (keys files)))
          sizes (mapv #(nth % 2) items)
          pool  (Executors/newFixedThreadPool 4)
          fill  (vec (for [part (partition-all 10 (sort (keys files)))]
                       (.submit pool ^Callable #(doseq [vp part, b (range 9)] (read-at c vp (* b bs) 1)))))
          seen  (loop [prev (vec (repeat (count items) 0)) n 0]
                  (let [now (storage/stored-bytes s items)]
                    (is (every? true? (map <= prev now sizes)) "never shrinks, never past the size")
                    (if (every? #(.isDone ^Future %) fill) n (recur now (inc n)))))]
      (doseq [^Future f fill] (.get f))
      (.shutdown pool)
      (is (pos? seen))
      (is (= sizes (storage/stored-bytes s items)))
      (is (= (mapv #(ranges-total s %) items) (storage/stored-bytes s items))))))

(deftest stored-bytes-is-fast
  (with-storage [s (tmp) {:map-size (* 1024 1024 1024)}]
    (let [n     50000
          items (vec (for [i (range n)] ["src" (str "dir" (quot i 1000) "/file" i ".dat") (+ 100 (rem i 3000)) 1700000000]))
          st    (:store s)]
      (doseq [part (partition-all 5000 (map-indexed vector items))]
        (store/put-files! st (vec (for [[i [sk vp size mtime]] part :when (even? i)]
                                    [(cat/cache-file-id (cat/cache-key sk 0 (path/fold vp) size mtime)) (noise i size)]))))
      (let [time  (fn [] (let [t0 (System/nanoTime) r (storage/stored-bytes s items)] [r (/ (- (System/nanoTime) t0) 1e6)]))
            [r ms] (time)
            [_ ms2] (time)]
        (println (format "stored-bytes: %d files in one call, %.1f ms (again: %.1f ms)" n ms ms2))
        (is (= (vec (map-indexed (fn [i [_ _ size]] (if (even? i) size 0)) items)) r))
        (is (< ms 1000.0))))))

(deftest the-whole-stack
  ;; a writable layer over a cached, read-only source: the game's content
  ;; fetched on demand, its writes kept in a layer
  (with-storage [s (tmp)]
    (let [reads (AtomicLong.)
          game  (storage/cached s (counting (source {"Data/Skyrim.esm" (noise 1 (* 3 bs)) "SkyrimPrefs.ini" (.getBytes "a=1")}) reads) "steam:489830")
          root  (compose/overlay (storage/layer s "profile") game)]
      (is (Arrays/equals (noise 1 (* 3 bs)) (read-all root "data/skyrim.esm")))
      (let [h (prov/open root "SkyrimPrefs.ini" p/open-write)]
        (write-at! h 2 (.getBytes "2")) (prov/close! h))
      (is (= "a=2" (String. (read-all root "skyrimprefs.ini"))))
      (is (= "a=1" (String. (read-all game "skyrimprefs.ini"))) "the source is untouched"))))

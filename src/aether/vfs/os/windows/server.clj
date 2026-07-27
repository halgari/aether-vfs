(ns aether.vfs.os.windows.server
  "Ring opcode dispatch to an aether Provider + fh table, mirroring the Rust
  dispatch_director read path. Single-threaded spin serve loop."
  (:require [aether.vfs.os.windows.ring :as ring]
            [aether.vfs.os.windows.arena :as arena]
            [aether.vfs.wire :as wire]
            [aether.vfs.provider :as p])
  (:import [java.lang.foreign MemorySegment ValueLayout]))

(def ^:private OP-GETATTR 1) (def ^:private OP-READDIR 2) (def ^:private OP-OPEN 3)
(def ^:private OP-READ 5)    (def ^:private OP-CLOSE 11)
(def ^:private FLAG-READ-BULK 0x1)
(def ^:private BULK-THRESHOLD (* 64 1024))
(def ^:private ST-OK 0) (def ^:private ST-NOT-FOUND -1) (def ^:private ST-BAD-FH -6)
(def ^:private ST-BAD-REQUEST -3)

(defn make-state [arena] (atom {:arena arena :next-fh 1 :open {}}))

(defn- resp [status ^bytes payload] {:status status :payload payload})

(defn- do-getattr [_ provider vpath]
  (if-let [m (p/lookup provider vpath)]
    (resp ST-OK (wire/encode-getattr-resp {:found true :is-dir (= :dir (:kind m))
                                           :size (:size m) :mtime (or (:mtime-secs m) 0)}))
    (resp ST-NOT-FOUND (wire/encode-getattr-resp {:found false :is-dir false :size 0 :mtime 0}))))

(defn- do-readdir [_ provider vpath]
  (let [entries (p/readdir provider vpath)]
    (resp ST-OK (wire/encode-readdir-resp
                  (map (fn [e] {:name (:name e) :is-dir (= :dir (:kind e))
                                :size (or (:size e) 0) :mtime (or (:mtime-secs e) 0)}) entries)))))

(defn- do-open [state provider flags vpath]
  (let [opened (p/open-file provider vpath flags)
        m (p/lookup provider vpath)
        fh (:next-fh @state)]
    (swap! state #(-> % (assoc-in [:open fh] {:provider provider :handle (:handle opened)
                                              :size (:size m)})
                        (assoc :next-fh (inc fh))))
    (resp ST-OK (wire/encode-open-resp {:fh fh :size (:size m) :is-dir (= :dir (:kind m))}))))

(defn- do-read [state _ flags {:keys [fh offset len]}]
  (if-let [rec (get-in @state [:open fh])]
    (let [want (long len)
          bulk? (or (not= 0 (bit-and (long flags) FLAG-READ-BULK)) (> want BULK-THRESHOLD))]
      (if bulk?
        (let [arena (:arena @state)
              {:keys [offset len]} (arena/fill-bank arena fh want
                                     (fn [^MemorySegment bank]
                                       (let [bytes (p/read-at (:provider rec) (:handle rec) offset (min want (:bank-size arena)))]
                                         (MemorySegment/copy (MemorySegment/ofArray bytes) 0 bank 0 (long (alength bytes)))
                                         (alength bytes))))]
          (resp ST-OK (wire/encode-read-resp-bulk len offset)))
        (let [bytes (p/read-at (:provider rec) (:handle rec) offset want)]
          (resp ST-OK (wire/encode-read-resp bytes)))))
    (resp ST-BAD-FH (byte-array 0))))

(defn- do-close [state _ fh]
  (when-let [rec (get-in @state [:open fh])]
    (p/release-handle (:provider rec) (:handle rec)))
  (swap! state update :open dissoc fh)
  (resp ST-OK (byte-array 0)))

(defn dispatch [state provider {:keys [opcode flags payload]}]
  (condp = (long opcode)
    OP-GETATTR (do-getattr state provider (wire/decode-path-req payload))
    OP-READDIR (do-readdir state provider (wire/decode-path-req payload))
    OP-OPEN    (let [{:keys [flags path]} (wire/decode-open-req payload)] (do-open state provider flags path))
    OP-READ    (do-read state provider flags (wire/decode-read-req payload))
    OP-CLOSE   (do-close state provider (wire/decode-close-req payload))
    (resp ST-BAD-REQUEST (byte-array 0))))

(defn serve
  "Spin serve loop: take a submitted slot, dispatch, complete. Stops when @stop? is true."
  [^MemorySegment seg geom arena provider stop?]
  (let [state (make-state arena)]
    (loop []
      (when-not @stop?
        (if-let [slot (ring/server-take seg geom)]
          (let [req (ring/read-request seg geom slot)
                {:keys [status payload]} (dispatch state provider req)]
            (ring/server-complete seg geom slot status payload))
          (Thread/onSpinWait))
        (recur)))))

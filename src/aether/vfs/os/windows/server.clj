(ns aether.vfs.os.windows.server
  "Ring opcode dispatch to an aether Provider + fh table, mirroring the Rust
  dispatch_director read path. Single-threaded spin serve loop."
  (:require [aether.vfs.os.windows.ring :as ring]
            [aether.vfs.os.windows.arena :as arena]
            [aether.vfs.wire :as wire]
            [aether.vfs.error :as error]
            [aether.vfs.provider :as p])
  (:import [java.lang.foreign MemorySegment ValueLayout]))

(def ^:private OP-GETATTR 1) (def ^:private OP-READDIR 2) (def ^:private OP-OPEN 3)
(def ^:private OP-READ 5)    (def ^:private OP-WRITE 6)   (def ^:private OP-CLOSE 11)
(def ^:private OP-HEARTBEAT 13)
(def ^:private OP-SETATTR 7) (def ^:private OP-RENAME 8)  (def ^:private OP-DELETE 9)
(def ^:private OP-MKDIR 10)
(def ^:private FLAG-READ-BULK 0x1)
(def ^:private OPEN-WRITE 2)
(def ^:private DEFAULT-CREATE-MODE 420) ; 0644
(def ^:private BULK-THRESHOLD (* 64 1024))
(def ^:private ST-OK 0) (def ^:private ST-NOT-FOUND -1) (def ^:private ST-BAD-FH -6)
(def ^:private ST-NOT-A-DIRECTORY -2)
;; No dedicated ST_READ_ONLY exists in vfs-protocol (only ST_OK..ST_NO_SPACE,
;; -1..-7); a :read-only raise from a Writable wrapper maps onto ST_BAD_REQUEST,
;; matching the Rust director's existing "OPEN_WRITE unsupported -> BAD_REQUEST"
;; convention rather than inventing a colliding status code here.
(def ^:private ST-BAD-REQUEST -3) (def ^:private ST-IO-ERROR -4)

(defn make-state [arena] (atom {:arena arena :next-fh 1 :open {}}))

(defn- resp [status ^bytes payload] {:status status :payload payload})

(defn- norm-vpath
  "The injected shim classifies opens by root prefix and sends the path relative
  to the virtual root WITHOUT a leading slash (e.g. \"data/x.esp\", \"\" for the
  root); aether Providers expect a '/'-rooted vpath. Normalize so they agree
  (mirrors the Rust director's normalize)."
  ^String [^String s]
  (cond (or (nil? s) (= s "")) "/"
        (.startsWith s "/") s
        :else (str "/" s)))

(defn- do-getattr [_ provider vpath]
  ;; GETATTR always answers ST_OK; a missing path yields found:false (mirrors
  ;; Rust dispatch_director GETATTR — it never uses ST_NOT_FOUND).
  (error/on-not-found
    (let [m (p/lookup provider vpath)]
      (resp ST-OK (wire/encode-getattr-resp {:found true :is-dir (= :dir (:kind m))
                                             :size (:size m) :mtime (or (:mtime-secs m) 0)})))
    (resp ST-OK (wire/encode-getattr-resp {:found false :is-dir false :size 0 :mtime 0}))))

(defn- do-readdir [_ provider vpath]
  ;; Guard with lookup: the inline provider's readdir returns [] for any prefix
  ;; and never raises, so mirror Rust READDIR (ST_NOT_FOUND / ST_NOT_A_DIRECTORY)
  ;; by resolving the path first. lookup raises :not-found for a missing path.
  (error/on-not-found
    (let [m (p/lookup provider vpath)]
      (if (= :dir (:kind m))
        (let [entries (p/readdir provider vpath)]
          (resp ST-OK (wire/encode-readdir-resp
                        (map (fn [e] {:name (:name e) :is-dir (= :dir (:kind e))
                                      :size (or (:size e) 0) :mtime (or (:mtime-secs e) 0)}) entries))))
        (resp ST-NOT-A-DIRECTORY (byte-array 0))))
    (resp ST-NOT-FOUND (byte-array 0))))

(defn- do-open [state provider flags vpath]
  ;; A missing path yields ST_NOT_FOUND (mirrors Rust dispatch_director OPEN).
  ;; OPEN_WRITE (bit 2) routes through the provider's Writable wrapper
  ;; (create-file) to open/create a writable handle; a :read-only raise (base
  ;; provider without a Writable overlay) maps to ST_BAD_REQUEST rather than
  ;; propagating to the total try/catch's generic ST_IO_ERROR.
  (try
    (if (not= 0 (bit-and (long flags) OPEN-WRITE))
      ;; Write-create: a freshly created/truncated file is 0 bytes, so DON'T
      ;; lookup after create — a lookup-after-create tripped a create-vs-lookup
      ;; path quirk in the overlay on Linux (stat of the just-created upper file
      ;; raised :not-found → ST_NOT_FOUND). A :read-only raise still maps to
      ;; ST_BAD_REQUEST via the catch below.
      (let [opened (p/create provider vpath flags DEFAULT-CREATE-MODE)
            fh (:next-fh @state)]
        (swap! state #(-> % (assoc-in [:open fh] {:provider provider :handle (:handle opened) :size 0 :vpath vpath})
                            (assoc :next-fh (inc fh))))
        (resp ST-OK (wire/encode-open-resp {:fh fh :size 0 :is-dir false})))
      (error/on-not-found
        ;; Lookup first: a directory open must NOT go through open-file, which
        ;; opens a byte-stream FileChannel and throws on a directory. Directory
        ;; handles carry no channel (enumeration/attr queries are path- or
        ;; getattr-based); a close on the nil handle is a no-op, and a stray
        ;; byte-read would surface as ST_IO_ERROR (never a wedge) — callers do
        ;; not NtReadFile a directory handle.
        (let [m (p/lookup provider vpath)
              fh (:next-fh @state)]
          (if (= :dir (:kind m))
            (do (swap! state #(-> % (assoc-in [:open fh] {:provider provider :handle nil
                                                          :size 0 :vpath vpath :dir true})
                                    (assoc :next-fh (inc fh))))
                (resp ST-OK (wire/encode-open-resp {:fh fh :size 0 :is-dir true})))
            (let [opened (p/open-file provider vpath flags)]
              (swap! state #(-> % (assoc-in [:open fh] {:provider provider :handle (:handle opened)
                                                        :size (:size m) :vpath vpath})
                                  (assoc :next-fh (inc fh))))
              (resp ST-OK (wire/encode-open-resp {:fh fh :size (:size m) :is-dir false})))))
        (resp ST-NOT-FOUND (byte-array 0))))
    (catch clojure.lang.ExceptionInfo e
      (if (= :read-only (error/category e))
        (resp ST-BAD-REQUEST (byte-array 0))
        (throw e)))))

(defn- do-read [state _ flags {:keys [fh offset len]}]
  (if-let [rec (get-in @state [:open fh])]
    (let [want (long len)
          bulk? (or (not= 0 (bit-and (long flags) FLAG-READ-BULK)) (>= want BULK-THRESHOLD))]
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

(defn- do-write [state _ {:keys [fh offset data]}]
  (if-let [rec (get-in @state [:open fh])]
    (let [n (p/write-at (:provider rec) (:handle rec) offset data)]
      (resp ST-OK (wire/encode-write-resp (int n))))
    (resp ST-BAD-FH (byte-array 0))))

(defn- do-close [state _ fh]
  (if-let [rec (get-in @state [:open fh])]
    (do (p/release-handle (:provider rec) (:handle rec))
        (swap! state update :open dissoc fh)
        (resp ST-OK (byte-array 0)))
    (resp ST-BAD-FH (byte-array 0))))

(defmacro ^:private try-write
  "Run a Writable-overlay mutation, replying ST_OK on success. A :not-found
  raise (e.g. unlink/truncate/rename of a missing path) maps to ST_NOT_FOUND;
  a :read-only raise (provider without a Writable overlay) maps to
  ST_BAD_REQUEST, mirroring do-open's OPEN_WRITE handling. Any other
  ExceptionInfo propagates to dispatch's total try/catch -> ST_IO_ERROR."
  [& body]
  `(try
     ~@body
     (resp ST-OK (byte-array 0))
     (catch clojure.lang.ExceptionInfo e#
       (case (error/category e#)
         :not-found (resp ST-NOT-FOUND (byte-array 0))
         :read-only (resp ST-BAD-REQUEST (byte-array 0))
         (throw e#)))))

(defn- do-delete [_ provider vpath]
  (try-write (p/unlink provider vpath)))

(defn- do-rename [_ provider from to]
  (try-write (p/rename provider from to)))

(defn- do-mkdir [_ provider vpath mode]
  (try-write (p/mkdir provider vpath mode)))

(defn- do-truncate [state _ {:keys [fh size]}]
  (if-let [rec (get-in @state [:open fh])]
    (try-write (p/truncate (:provider rec) (:vpath rec) size))
    (resp ST-BAD-FH (byte-array 0))))

(defn dispatch [state provider {:keys [opcode flags payload]}]
  ;; Total: every request MUST yield a status so the serve loop always reaches
  ;; ring/server-complete for the slot it CAS'd to PROCESSING. Any otherwise-
  ;; unhandled throw becomes ST_IO_ERROR rather than wedging the ring.
  (try
    (condp = (long opcode)
      OP-GETATTR (do-getattr state provider (norm-vpath (wire/decode-path-req payload)))
      OP-READDIR (do-readdir state provider (norm-vpath (wire/decode-path-req payload)))
      OP-OPEN    (let [{:keys [flags path]} (wire/decode-open-req payload)] (do-open state provider flags (norm-vpath path)))
      OP-READ    (do-read state provider flags (wire/decode-read-req payload))
      OP-WRITE   (do-write state provider (wire/decode-write-req payload))
      OP-CLOSE   (do-close state provider (wire/decode-close-req payload))
      OP-DELETE  (do-delete state provider (norm-vpath (wire/decode-path-req payload)))
      OP-RENAME  (let [{:keys [from to]} (wire/decode-rename-req payload)]
                   (do-rename state provider (norm-vpath from) (norm-vpath to)))
      OP-MKDIR   (let [{:keys [mode path]} (wire/decode-mkdir-req payload)]
                   (do-mkdir state provider (norm-vpath path) mode))
      OP-SETATTR (do-truncate state provider (wire/decode-setattr-req payload))
      OP-HEARTBEAT (resp ST-OK (byte-array 0))
      (resp ST-BAD-REQUEST (byte-array 0)))
    (catch Throwable _
      (resp ST-IO-ERROR (byte-array 0)))))

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

(ns aether.vfs.os.windows.ring
  "vfs-ipc ring state machine over a java.lang.foreign.MemorySegment. Mirrors
  rust/crates/vfs-ipc/src/ring.rs byte-for-byte; all offsets/states come from
  aether.vfs.protocol. Scalar fields use native-order JAVA_INT/JAVA_LONG; the
  slot `state` field is the only atomic and is accessed through a VarHandle with
  acquire/release/CAS access modes to match Rust's AtomicU32 orderings."
  (:require [aether.vfs.protocol :as proto])
  (:import [java.lang.foreign MemoryLayout$PathElement MemorySegment ValueLayout]
           [java.lang.invoke MethodHandle VarHandle VarHandle$AccessMode]))

(def ^:private RING-HDR 40)
(def ^:private SLOT-HDR 32)

;; VarHandle over the slot `state` field: coordinates (MemorySegment, long).
;; NOTE (JDK 26 interop): VarHandle access-mode methods (compareAndSet,
;; setRelease, getAcquire, ...) are @PolymorphicSignature natives. javac
;; special-cases the call-site bytecode for these; Clojure's compiler cannot
;; statically resolve them (it has no primitive `int` hint) and falls back to
;; clojure.lang.Reflector, which throws
;; "No matching method ... found taking N args" because Reflector sees the
;; single-Object[]-vararg descriptor, not the polymorphic one. The supported
;; workaround is VarHandle/toMethodHandle(AccessMode), which reifies an
;; ordinary (non-polymorphic) MethodHandle typed to the coordinates/value
;; type; MethodHandle.invokeWithArguments is a normal varargs method Clojure
;; can call directly, and it performs the same box/unbox coercion internally.
(def ^:private ^VarHandle state-vh
  (.varHandle ValueLayout/JAVA_INT (make-array MemoryLayout$PathElement 0)))
(def ^:private ^MethodHandle state-cas-mh (.toMethodHandle state-vh VarHandle$AccessMode/COMPARE_AND_SET))
(def ^:private ^MethodHandle state-set-release-mh (.toMethodHandle state-vh VarHandle$AccessMode/SET_RELEASE))
(def ^:private ^MethodHandle state-get-acquire-mh (.toMethodHandle state-vh VarHandle$AccessMode/GET_ACQUIRE))

(def ^:private ST-FREE 0)
(def ^:private ST-CLAIMED 1)
(def ^:private ST-SUBMITTED 2)
(def ^:private ST-PROCESSING 3)
(def ^:private ST-COMPLETED 4)

(defn- align8 ^long [^long n] (bit-and (+ n 7) (bit-not 7)))

;; --- scalar helpers (native order) ---
(defn- get-i32 ^long [^MemorySegment seg ^long off] (long (.get seg ValueLayout/JAVA_INT off)))
(defn- set-i32 [^MemorySegment seg ^long off v] (.set seg ValueLayout/JAVA_INT off (int v)))
(defn- get-i64 ^long [^MemorySegment seg ^long off] (.get seg ValueLayout/JAVA_LONG off))
(defn- set-i64 [^MemorySegment seg ^long off v] (.set seg ValueLayout/JAVA_LONG off (long v)))

(defn- slot-off ^long [geom ^long slot] (+ RING-HDR (* slot (long (:slot-stride geom)))))
(defn- payload-off ^long [geom ^long slot] (+ (slot-off geom slot) SLOT-HDR))

(defn- cas-state
  "CAS the 32-bit state field at absolute offset `off`. exp/new-v are ints."
  [^MemorySegment seg ^long off exp new-v]
  (boolean (.invokeWithArguments state-cas-mh (object-array [seg off (int exp) (int new-v)]))))

(defn- state-set-release [^MemorySegment seg ^long off v]
  (.invokeWithArguments state-set-release-mh (object-array [seg off (int v)])))

(defn- state-get-acquire ^long [^MemorySegment seg ^long off]
  (long (.invokeWithArguments state-get-acquire-mh (object-array [seg off]))))

(defn init
  "Lay out an empty ring in seg; returns geom {:slot-count :slot-stride :payload-cap}."
  [^MemorySegment seg slot-count payload-cap]
  (let [stride (align8 (+ SLOT-HDR (long payload-cap)))
        rh (:ring-header (deref proto/descriptor))
        f (:fields rh)]
    (set-i32 seg (long (:magic f)) (:magic (deref proto/descriptor)))
    (set-i32 seg (long (:version f)) proto/version)
    (set-i32 seg (long (:slot-count f)) slot-count)
    (set-i32 seg (long (:slot-stride f)) stride)
    (set-i32 seg (long (:payload-cap f)) payload-cap)
    (set-i64 seg (long (:req-seq f)) 0)
    (set-i32 seg (long (:submit-seq f)) 0)
    (let [geom {:slot-count (long slot-count) :slot-stride stride :payload-cap (long payload-cap)}]
      (dotimes [s slot-count]
        (set-i32 seg (slot-off geom s) ST-FREE)) ; SH_STATE offset is 0
      geom)))

(defn- sh [k] (get-in (deref proto/descriptor) [:slot-header :fields k]))

(defn server-take
  "CAS the first SUBMITTED slot to PROCESSING; return its index or nil."
  [^MemorySegment seg geom]
  (loop [s 0]
    (when (< s (long (:slot-count geom)))
      (let [off (+ (slot-off geom s) (long (sh :state)))]
        (if (cas-state seg off ST-SUBMITTED ST-PROCESSING)
          s
          (recur (inc s)))))))

(defn read-request
  [^MemorySegment seg geom ^long slot]
  (let [base (slot-off geom slot)
        len (get-i32 seg (+ base (long (sh :payload-len))))
        payload (byte-array len)]
    (MemorySegment/copy seg (long (payload-off geom slot)) (MemorySegment/ofArray payload) (long 0) (long len))
    {:opcode (get-i32 seg (+ base (long (sh :opcode))))
     :flags  (get-i32 seg (+ base (long (sh :flags))))
     :req-id (get-i64 seg (+ base (long (sh :req-id))))
     :payload payload}))

(defn server-complete
  [^MemorySegment seg geom slot status ^bytes payload]
  (let [base (slot-off geom (long slot))
        n (alength payload)]
    (MemorySegment/copy (MemorySegment/ofArray payload) (long 0) seg (long (payload-off geom slot)) (long n))
    (set-i32 seg (+ base (long (sh :status))) status)
    (set-i32 seg (+ base (long (sh :payload-len))) n)
    (state-set-release seg (+ base (long (sh :state))) ST-COMPLETED)))

;; --- client-side helpers (test harness only; mirror ring.rs) ---
(defn claim-free [^MemorySegment seg geom]
  (loop [s 0]
    (when (< s (long (:slot-count geom)))
      (let [off (+ (slot-off geom s) (long (sh :state)))]
        (if (cas-state seg off ST-FREE ST-CLAIMED)
          s (recur (inc s)))))))

(defn publish-request [^MemorySegment seg geom slot opcode flags ^bytes payload]
  (let [base (slot-off geom (long slot))]
    (set-i32 seg (+ base (long (sh :opcode))) opcode)
    (set-i32 seg (+ base (long (sh :flags))) flags)
    (set-i32 seg (+ base (long (sh :payload-len))) (alength payload))
    (MemorySegment/copy (MemorySegment/ofArray payload) (long 0) seg (long (payload-off geom slot)) (long (alength payload)))
    (state-set-release seg (+ base (long (sh :state))) ST-SUBMITTED)))

(defn take-response [^MemorySegment seg geom ^long slot]
  (let [base (slot-off geom slot)]
    (when (= ST-COMPLETED (state-get-acquire seg (+ base (long (sh :state)))))
      (let [len (get-i32 seg (+ base (long (sh :payload-len))))
            payload (byte-array len)]
        (MemorySegment/copy seg (long (payload-off geom slot)) (MemorySegment/ofArray payload) (long 0) (long len))
        {:status (get-i32 seg (+ base (long (sh :status)))) :payload payload}))))

(defn free-slot [^MemorySegment seg geom ^long slot]
  (state-set-release seg (+ (slot-off geom slot) (long (sh :state))) ST-FREE))

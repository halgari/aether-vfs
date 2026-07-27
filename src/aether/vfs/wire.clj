(ns aether.vfs.wire
  "Wire codec mirroring the Rust vfs-protocol crate byte-for-byte. Conformance
  is enforced against resources/protocol-golden.edn (wire-conformance-test).
  All integers little-endian. Extend under the same golden test — never change
  a byte layout here without changing it in Rust first and regenerating."
  (:import [java.io ByteArrayOutputStream]
           [java.nio ByteBuffer ByteOrder]))

(defn- baos ^ByteArrayOutputStream [] (ByteArrayOutputStream.))
(defn- put-u32! [^ByteArrayOutputStream b v]
  (let [x (long v)] (dotimes [i 4] (.write b (int (bit-and (bit-shift-right x (* 8 i)) 0xff))))))
(defn- put-u64! [^ByteArrayOutputStream b v]
  (let [x (long v)] (dotimes [i 8] (.write b (int (bit-and (bit-shift-right x (* 8 i)) 0xff))))))

(defn encode-open-req ^bytes [flags ^String path]
  (let [b (baos)] (put-u32! b flags) (.write b (.getBytes path "UTF-8")) (.toByteArray b)))

(defn encode-getattr-resp ^bytes [{:keys [found is-dir size mtime]}]
  (let [b (baos)]
    (.write b (int (if found 1 0)))
    (.write b (int (if is-dir 1 0)))
    (put-u64! b size)
    (put-u64! b mtime)
    (.toByteArray b)))

(defn encode-read-req ^bytes [{:keys [fh offset len]}]
  (let [b (baos)] (put-u64! b fh) (put-u64! b offset) (put-u32! b len) (put-u32! b 0) (.toByteArray b)))

(defn encode-read-resp ^bytes [^bytes data]
  (let [b (baos)] (put-u32! b (alength data)) (put-u32! b 0) (.write b data 0 (alength data)) (.toByteArray b)))

(defn encode-readdir-resp ^bytes [entries]
  (let [b (baos)]
    (put-u32! b (count entries))
    (doseq [{:keys [name is-dir size mtime]} entries]
      (let [nb (.getBytes ^String name "UTF-8")]
        (put-u32! b (alength nb))
        (.write b nb 0 (alength nb))
        (.write b (int (if is-dir 1 0)))
        (put-u64! b size)
        (put-u64! b mtime)))
    (.toByteArray b)))

(defn encode-close-req ^bytes [fh]
  (let [b (baos)] (put-u64! b fh) (.toByteArray b)))

;; --- decoders (little-endian views) ---

(defn- buf ^ByteBuffer [^bytes p] (doto (ByteBuffer/wrap p) (.order ByteOrder/LITTLE_ENDIAN)))

(defn decode-getattr-resp [^bytes p]
  (let [bb (buf p)
        found (not (zero? (.get bb)))
        is-dir (not (zero? (.get bb)))]
    {:found found :is-dir is-dir :size (.getLong bb) :mtime (.getLong bb)}))

(defn decode-read-resp ^bytes [^bytes p]
  (let [bb (buf p)
        n (bit-and (long (.getInt bb)) 0xffffffff)]
    (.getInt bb) ; pad
    (let [out (byte-array n)] (.get bb out) out)))

(defn decode-readdir-resp [^bytes p]
  (let [bb (buf p)
        n (.getInt bb)]
    (vec (for [_ (range n)]
           (let [nlen (.getInt bb)
                 nb (byte-array nlen)]
             (.get bb nb)
             {:name (String. nb "UTF-8")
              :is-dir (not (zero? (.get bb)))
              :size (.getLong bb)
              :mtime (.getLong bb)})))))

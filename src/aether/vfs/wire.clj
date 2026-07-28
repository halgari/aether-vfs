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

;; --- server-side codecs (mirrors vfs-protocol; M2) ---

(defn decode-path-req ^String [^bytes p] (String. p "UTF-8"))

(defn decode-open-req [^bytes p]
  (let [bb (buf p)
        flags (bit-and (long (.getInt bb)) 0xffffffff)
        rest (byte-array (.remaining bb))]
    (.get bb rest)
    {:flags flags :path (String. rest "UTF-8")}))

(defn encode-open-resp ^bytes [{:keys [fh size is-dir]}]
  (let [b (baos)]
    (put-u64! b fh) (put-u64! b size) (.write b (int (if is-dir 1 0)))
    (dotimes [_ 7] (.write b 0))
    (.toByteArray b)))

(defn decode-open-resp [^bytes p]
  (let [bb (buf p)]
    {:fh (.getLong bb) :size (.getLong bb) :is-dir (not (zero? (.get bb)))}))

(defn decode-read-req [^bytes p]
  (let [bb (buf p)]
    {:fh (.getLong bb) :offset (.getLong bb) :len (bit-and (long (.getInt bb)) 0xffffffff)}))

(def ^:private READ-RESP-BULK-BIT 0x80000000)
(defn encode-read-resp-bulk ^bytes [bytes-read arena-offset]
  (let [b (baos)]
    (put-u32! b (bit-or (long bytes-read) READ-RESP-BULK-BIT))
    (put-u32! b 0)
    (put-u64! b arena-offset)
    (.toByteArray b)))

(defn decode-read-resp-bulk [^bytes p]
  (let [bb (buf p)
        raw (bit-and (long (.getInt bb)) 0xffffffff)]
    (.getInt bb) ; pad
    [(bit-and raw 0x7fffffff) (.getLong bb)]))

(defn decode-close-req [^bytes p] (.getLong (buf p)))

(defn encode-write-req ^bytes [{:keys [fh offset]} ^bytes data]
  (let [b (baos)] (put-u64! b fh) (put-u64! b offset)
    (put-u32! b (alength data)) (put-u32! b 0)
    (.write b data 0 (alength data)) (.toByteArray b)))

(defn decode-write-req [^bytes p]
  (let [bb (buf p) fh (.getLong bb) offset (.getLong bb)
        len (bit-and (long (.getInt bb)) 0xffffffff)]
    (.getInt bb) ; pad
    (let [d (byte-array len)] (.get bb d) {:fh fh :offset offset :data d})))

(defn encode-write-resp ^bytes [n]
  (let [b (baos)] (put-u32! b n) (put-u32! b 0) (.toByteArray b)))

(defn decode-write-resp [^bytes p] (bit-and (long (.getInt (buf p))) 0xffffffff))

;; --- mkdir / rename / setattr (M4 Part 2: write set) ---

(defn encode-mkdir-req ^bytes [mode ^String path]
  (let [b (baos)] (put-u32! b mode) (.write b (.getBytes path "UTF-8")) (.toByteArray b)))

(defn decode-mkdir-req [^bytes p]
  (let [bb (buf p)
        mode (bit-and (long (.getInt bb)) 0xffffffff)
        rest (byte-array (.remaining bb))]
    (.get bb rest)
    {:mode mode :path (String. rest "UTF-8")}))

(defn encode-rename-req ^bytes [^String from ^String to]
  (let [b (baos)
        fb (.getBytes from "UTF-8")
        tb (.getBytes to "UTF-8")]
    (put-u32! b (alength fb))
    (.write b fb 0 (alength fb))
    (.write b tb 0 (alength tb))
    (.toByteArray b)))

(defn decode-rename-req [^bytes p]
  (let [bb (buf p)
        from-len (bit-and (long (.getInt bb)) 0xffffffff)
        from-b (byte-array from-len)
        _ (.get bb from-b)
        to-b (byte-array (.remaining bb))
        _ (.get bb to-b)]
    {:from (String. from-b "UTF-8") :to (String. to-b "UTF-8")}))

(defn encode-setattr-req ^bytes [{:keys [fh size]}]
  (let [b (baos)] (put-u64! b fh) (put-u64! b size) (.toByteArray b)))

(defn decode-setattr-req [^bytes p]
  (let [bb (buf p)]
    {:fh (.getLong bb) :size (.getLong bb)}))

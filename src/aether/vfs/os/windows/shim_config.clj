(ns aether.vfs.os.windows.shim-config
  "Encodes the VFS_SHIM_CONFIG file the injected shim reads. Mirrors
  vfs-shim::encode_config (encode_config_full with empty overlay + no static
  imports): [u32 root_len][root utf8][u32 overlay_len=0][\"VFS1\"][u32 0][snapshot].
  Pinned byte-for-byte to the Rust golden vector."
  (:import [java.io ByteArrayOutputStream]))

(defn- put-u32! [^ByteArrayOutputStream b v]
  (let [x (long v)] (dotimes [i 4] (.write b (int (bit-and (bit-shift-right x (* 8 i)) 0xff))))))

(defn encode ^bytes [^String root ^bytes snapshot]
  (let [b (ByteArrayOutputStream.)
        rb (.getBytes root "UTF-8")]
    (put-u32! b (alength rb)) (.write b rb 0 (alength rb))
    (put-u32! b 0)                       ; overlay_len = 0
    (.write b (.getBytes "VFS1" "UTF-8") 0 4)
    (put-u32! b 0)                       ; n_static = 0
    (.write b snapshot 0 (alength snapshot))
    (.toByteArray b)))

(defn- hex->bytes ^bytes [^String h]
  (byte-array (for [i (range 0 (count h) 2)]
                (unchecked-byte (Integer/parseInt (subs h i (+ i 2)) 16)))))

(def ^:private empty-tree-snapshot-hex
  ;; vfs_shared::bridge::flatten(vfs_core::build([Layer{0,[]}])) — a valid empty
  ;; tree. VFS_SHIM_CONFIG requires a valid snapshot (an empty byte-array fails
  ;; the shim's Engine::new). Pinned byte-for-byte to the golden vector.
  (str "5353465601000000000000000000000080000000000000000100000030000000"
       "0000000080000000000000008000000000000000000000008000000000000000"
       "0000000000000000800000000000000000000000000000000000000000000000"
       "0000000000000000000000000000000000000000000000000000000000000000"))

(defn empty-tree-snapshot ^bytes [] (hex->bytes empty-tree-snapshot-hex))

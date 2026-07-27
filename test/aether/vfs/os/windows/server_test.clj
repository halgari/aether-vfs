(ns aether.vfs.os.windows.server-test
  (:require [clojure.test :refer [deftest is]]
            [aether.vfs.os.windows.server :as server]
            [aether.vfs.os.windows.arena :as arena]
            [aether.vfs.wire :as wire]
            [aether.vfs.providers.inline :as inline])
  (:import [java.lang.foreign Arena MemorySegment ValueLayout]))

(defn- seg ^MemorySegment [n] (.allocate (Arena/ofAuto) (long n) 8))

(def small (.getBytes "hello" "UTF-8"))
(def big (byte-array 70000 (byte 88))) ; 'X' * 70000 > 64KiB -> bulk

(defn- provider [] (inline/inline-provider [["/hello.txt" small 0644]
                                            ["/big.bin" big 0644]]))

(deftest getattr-and-inline-read
  (let [s (seg (* 1 1024 1024))
        a (arena/make s 0 4096 2) ; arena unused for inline
        st (server/make-state a)
        p (provider)
        ga (server/dispatch st p {:opcode 1 :flags 0 :payload (.getBytes "/hello.txt" "UTF-8")})
        attr (wire/decode-getattr-resp (:payload ga))]
    (is (= 0 (:status ga)))
    (is (= 5 (:size attr)))
    (let [op (server/dispatch st p {:opcode 3 :flags 1 :payload (wire/encode-open-req 1 "/hello.txt")})
          {:keys [fh]} (wire/decode-open-resp (:payload op))
          rd (server/dispatch st p {:opcode 5 :flags 0 :payload (wire/encode-read-req {:fh fh :offset 0 :len 5})})]
      (is (= "hello" (String. ^bytes (wire/decode-read-resp (:payload rd)) "UTF-8"))))))

(deftest bulk-read-lands-in-arena
  (let [s (seg (* 1 1024 1024))
        a (arena/make s (* 512 1024) (* 256 1024) 2) ; arena at 512KiB
        st (server/make-state a)
        p (provider)
        op (server/dispatch st p {:opcode 3 :flags 1 :payload (wire/encode-open-req 1 "/big.bin")})
        {:keys [fh]} (wire/decode-open-resp (:payload op))
        rd (server/dispatch st p {:opcode 5 :flags 1 ; FLAG_READ_BULK
                                  :payload (wire/encode-read-req {:fh fh :offset 0 :len 70000})})
        [n off] (wire/decode-read-resp-bulk (:payload rd))]
    (is (= 70000 n))
    (is (= 88 (.get s ValueLayout/JAVA_BYTE off)))          ; first arena byte is 'X'
    (is (= 88 (.get s ValueLayout/JAVA_BYTE (+ off 69999)))))) ; last byte too

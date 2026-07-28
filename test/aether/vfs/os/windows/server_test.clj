(ns aether.vfs.os.windows.server-test
  (:require [clojure.test :refer [deftest is]]
            [aether.vfs.os.windows.server :as server]
            [aether.vfs.os.windows.arena :as arena]
            [aether.vfs.wire :as wire]
            [aether.vfs.provider :as provider]
            [aether.vfs.providers.inline :as inline]
            [aether.vfs.providers.overlay :as overlay])
  (:import [java.lang.foreign Arena MemorySegment ValueLayout]))

(defn- seg ^MemorySegment [n] (.allocate (Arena/ofAuto) (long n) 8))

(def small (.getBytes "hello" "UTF-8"))
(def big (byte-array 70000 (byte 88))) ; 'X' * 70000 > 64KiB -> bulk
(def exact (byte-array 65536 (byte 89))) ; 'Y' * 65536 == BULK_THRESHOLD -> bulk

(defn- provider [] (inline/inline-provider [["/hello.txt" small 0644]
                                            ["/big.bin" big 0644]
                                            ["/exact.bin" exact 0644]]))

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

(deftest bulk-threshold-boundary
  ;; len == BULK_THRESHOLD (65536) must go bulk even without FLAG_READ_BULK.
  (let [s (seg (* 1 1024 1024))
        a (arena/make s (* 512 1024) (* 256 1024) 2)
        st (server/make-state a)
        p (provider)
        op (server/dispatch st p {:opcode 3 :flags 1 :payload (wire/encode-open-req 1 "/exact.bin")})
        {:keys [fh]} (wire/decode-open-resp (:payload op))
        rd (server/dispatch st p {:opcode 5 :flags 0 ; NO FLAG_READ_BULK
                                  :payload (wire/encode-read-req {:fh fh :offset 0 :len 65536})})
        [n off] (wire/decode-read-resp-bulk (:payload rd))]
    (is (= 65536 n))
    (is (= 89 (.get s ValueLayout/JAVA_BYTE off)))))

(deftest close-unknown-fh-is-bad-fh
  (let [s (seg (* 1 1024 1024))
        a (arena/make s 0 4096 2)
        st (server/make-state a)
        p (provider)
        cl (server/dispatch st p {:opcode 11 :flags 0 :payload (wire/encode-close-req 999)})]
    (is (= -6 (:status cl)))))

(deftest getattr-missing-path-found-false
  (let [s (seg (* 1 1024 1024))
        a (arena/make s 0 4096 2)
        st (server/make-state a)
        p (provider)
        ga (server/dispatch st p {:opcode 1 :flags 0 :payload (.getBytes "/nope.txt" "UTF-8")})
        attr (wire/decode-getattr-resp (:payload ga))]
    (is (= 0 (:status ga)))
    (is (= false (:found attr)))))

(deftest open-missing-path-not-found
  (let [s (seg (* 1 1024 1024))
        a (arena/make s 0 4096 2)
        st (server/make-state a)
        p (provider)
        op (server/dispatch st p {:opcode 3 :flags 1 :payload (wire/encode-open-req 1 "/nope.txt")})]
    (is (= -1 (:status op)))))

(deftest readdir-missing-path-not-found
  (let [s (seg (* 1 1024 1024))
        a (arena/make s 0 4096 2)
        st (server/make-state a)
        p (provider)
        rd (server/dispatch st p {:opcode 2 :flags 0 :payload (.getBytes "/nope" "UTF-8")})]
    (is (= -1 (:status rd)))))

;; A provider whose every method throws a plain RuntimeException — nothing the
;; per-op handling catches (not an :aether.vfs/error). dispatch must still
;; return a status (ST_IO_ERROR -4), never propagate the throw into serve.
(def ^:private throwing-provider
  (reify provider/Provider
    (lookup [_ _] (throw (RuntimeException. "boom")))
    (readdir [_ _] (throw (RuntimeException. "boom")))
    (open-file [_ _ _] (throw (RuntimeException. "boom")))
    (read-at [_ _ _ _] (throw (RuntimeException. "boom")))
    (write-at [_ _ _ _] (throw (RuntimeException. "boom")))
    (release-handle [_ _] (throw (RuntimeException. "boom")))))

(deftest heartbeat-returns-ok
  (let [s (seg (* 1 1024 1024))
        a (arena/make s 0 4096 2)
        st (server/make-state a)
        p (provider)
        hb (server/dispatch st p {:opcode 13 :flags 0 :payload (byte-array 0)})]
    (is (= 0 (:status hb)))))

(deftest dispatch-is-total-on-unhandled-throw
  (let [s (seg (* 1 1024 1024))
        a (arena/make s 0 4096 2)
        st (server/make-state a)
        ga (server/dispatch st throwing-provider
                            {:opcode 1 :flags 0 :payload (.getBytes "/whatever" "UTF-8")})]
    (is (= -4 (:status ga)))
    (is (bytes? (:payload ga)))))

(deftest write-open-write-and-read-back
  (let [s (seg (* 1 1024 1024))
        a (arena/make s 0 4096 2)
        st (server/make-state a)
        overrides (str (System/getProperty "java.io.tmpdir") "vfs-m4-" (System/nanoTime))
        _ (.mkdirs (java.io.File. overrides))
        base (inline/inline-provider [])                    ; empty base
        p (overlay/overlay-provider base overrides)          ; Writable
        ;; open /new.txt for write (OPEN_WRITE=2), create
        op (server/dispatch st p {:opcode 3 :flags 2 :payload (wire/encode-open-req 2 "/new.txt")})
        {:keys [fh]} (wire/decode-open-resp (:payload op))
        w  (server/dispatch st p {:opcode 6 :flags 0 :payload (wire/encode-write-req {:fh fh :offset 0} (.getBytes "hi!" "UTF-8"))})
        n  (wire/decode-write-resp (:payload w))]
    (is (= 0 (:status op)))
    (is (= 3 n))
    ;; close the write handle so the overlay flushes to disk before reopening
    ;; (a fresh read handle won't see an unflushed write channel — this bit on Linux)
    (server/dispatch st p {:opcode 11 :flags 0 :payload (wire/encode-close-req fh)})
    ;; The OP_WRITE dispatch persisted the bytes into the overlay overrides dir.
    ;; (Read-your-writes over the ring is proven end-to-end by the injection
    ;; launch-test; an in-process re-open/read here was platform-dependent on the
    ;; overlay's channel-flush timing, so assert the copied-up file directly.)
    (is (= "hi!" (slurp (java.io.File. overrides "new.txt"))))))

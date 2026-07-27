(ns aether.vfs.protocol-test
  (:require [clojure.test :refer [deftest is]]
            [aether.vfs.protocol :as proto]))

(deftest loads-descriptor
  (is (= 1 proto/version))
  (is (= 0x56464950 (:magic @proto/descriptor))))

(deftest exposes-opcodes-and-statuses
  (is (= 5 (proto/op :read)))
  (is (= 3 (proto/op :open)))
  (is (= -1 (proto/status :not-found)))
  (is (= 0 (proto/status :ok))))

(deftest exposes-layout-offsets
  (is (= 24 (proto/ring-header-offset :req-seq)))
  (is (= 32 (proto/ring-header-offset :submit-seq)))
  (is (= 16 (proto/slot-header-offset :status)))
  (is (= 24 (proto/slot-header-offset :req-id))))

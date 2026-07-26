(ns aether.vfs.error-test
  (:require [clojure.test :refer [deftest is]]
            [aether.vfs.error :as vfs-error]))

(deftest errno-mapping-is-stable
  (is (= 2 (vfs-error/errno :not-found)))    ; ENOENT
  (is (= 5 (vfs-error/errno :io)))           ; EIO
  (is (= 13 (vfs-error/errno :permission-denied))) ; EACCES
  (is (= 22 (vfs-error/errno :invalid-argument))) ; EINVAL
  (is (= 20 (vfs-error/errno :not-a-directory))) ; ENOTDIR
  (is (= 21 (vfs-error/errno :is-a-directory))) ; EISDIR
  (is (= 30 (vfs-error/errno :read-only)))   ; EROFS
  (is (= 17 (vfs-error/errno :already-exists))) ; EEXIST
  (is (= 39 (vfs-error/errno :not-empty)))   ; ENOTEMPTY
  (is (= 5 (vfs-error/errno :something-unknown)) "unknown categories map to EIO"))

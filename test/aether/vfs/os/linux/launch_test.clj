(ns aether.vfs.os.linux.launch-test
  (:require [clojure.test :refer [deftest is]]
            [aether.vfs.os.linux.launch :as launch]
            [aether.vfs.providers.inline :as inline]))

(def ^:private linux?
  (.startsWith (.toLowerCase (System/getProperty "os.name")) "linux"))

(deftest run-executes-target-against-mount
  (if-not linux?
    (println "skip: linux/launch-test is Linux-only")
    (let [provider (inline/inline-provider [["/hello.txt" (.getBytes "hello" "UTF-8") 0644]])
          ;; the target reads the virtual file THROUGH the mount via $AETHER_VFS_MOUNT
          exit (launch/run provider
                 {:exec ["sh" "-c" "test \"$(cat \"$AETHER_VFS_MOUNT/hello.txt\")\" = hello"]})]
      (is (= 0 exit) "target saw the Provider-served file through the FUSE mount"))))

(deftest run-requires-exec
  (is (thrown? clojure.lang.ExceptionInfo (launch/run (inline/inline-provider []) {}))))

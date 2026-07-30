(ns aether.vfs-test
  (:require [clojure.test :refer [deftest is]]
            [aether.vfs :as vfs]))

(deftest os-kind-is-a-known-keyword
  (is (contains? #{:windows :linux :unsupported} (#'vfs/os-kind))))

(deftest to-windows-opts-maps-exec-and-artifacts
  (let [o (#'vfs/to-windows-opts
            {:exec ["game.exe" "--a" "--b"] :env {"K" "V"} :mountpoint "R" :windows {:slot-count 4}}
            {:injector "i" :shim-dll "s" :payload "p"})]
    (is (= "game.exe" (:target-exe o)))
    (is (= ["--a" "--b"] (:target-args o)))
    (is (= {"K" "V"} (:child-env o)))
    (is (= "i" (:injector o)))
    (is (= "R" (:root o)))
    (is (= 4 (:slot-count o))))) ; :windows passthrough merged

(deftest to-linux-opts-passes-exec-env-mount
  (let [o (#'vfs/to-linux-opts {:exec ["sh" "-c" "true"] :env {"K" "V"} :mountpoint "/mnt" :linux {:x 1}})]
    (is (= ["sh" "-c" "true"] (:exec o)))
    (is (= {"K" "V"} (:env o)))
    (is (= "/mnt" (:mountpoint o)))
    (is (= 1 (:x o)))))

(deftest run-requires-exec
  (is (thrown? clojure.lang.ExceptionInfo (vfs/run (reify Object) {}))))

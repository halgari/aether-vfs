(ns aether.vfs.read-pool-test
  (:require [clojure.test :refer [deftest is]]
            [aether.vfs.read-pool :as read-pool]))

(deftest runs-jobs-concurrently-not-serialized
  (let [pool (read-pool/read-pool 4)
        done (atom 0)
        live (atom 0)
        peak (atom 0)
        t0 (System/nanoTime)]
    ;; 8 jobs of 50ms each. Serialized ⇒ ~400ms; 4-wide ⇒ ~100ms.
    (dotimes [_ 8]
      (read-pool/submit! pool
                         (fn []
                           (swap! peak max (swap! live inc))
                           (Thread/sleep 50)
                           (swap! live dec)
                           (swap! done inc))))
    (while (< @done 8)
      (Thread/sleep 5))
    (let [elapsed-ms (quot (- (System/nanoTime) t0) 1000000)]
      (is (> @peak 1) "jobs must overlap (peak concurrency > 1)")
      (is (< elapsed-ms 300) "8x50ms on a 4-wide pool must beat serialized 400ms"))
    (read-pool/shutdown! pool)))

(deftest shutdown-drains-cleanly
  (let [pool (read-pool/read-pool 2)
        done (atom 0)]
    (dotimes [_ 4]
      (read-pool/submit! pool #(swap! done inc)))
    (read-pool/shutdown! pool) ; waits for submitted jobs
    (is (= 4 @done))))

(deftest a-throwing-job-does-not-kill-the-pool
  (let [pool (read-pool/read-pool 1)
        done (atom 0)]
    (read-pool/submit! pool #(throw (RuntimeException. "boom")))
    (read-pool/submit! pool #(swap! done inc))
    (read-pool/shutdown! pool)
    (is (= 1 @done) "the worker survives a throwing job")))

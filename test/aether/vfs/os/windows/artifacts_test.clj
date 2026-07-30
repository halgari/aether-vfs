(ns aether.vfs.os.windows.artifacts-test
  (:require [clojure.test :refer [deftest is]]
            [clojure.java.io :as io]
            [aether.vfs.os.windows.artifacts :as art]))

(defn- temp-dir ^java.io.File []
  (let [d (io/file (System/getProperty "java.io.tmpdir") (str "art-test-" (System/nanoTime)))]
    (.mkdirs d) d))

(deftest native-dir-override-wins
  (let [dir (temp-dir)]
    (doseq [n (vals art/artifact-names)] (spit (io/file dir n) "x"))
    (let [{:keys [injector shim-dll payload]} (art/resolve! {:native-dir (.getPath dir)})]
      (is (= (.getPath (io/file dir (:injector art/artifact-names))) injector))
      (is (.exists (io/file shim-dll)))
      (is (.exists (io/file payload))))))

(deftest native-dir-incomplete-falls-through-to-error
  ;; a dir missing one artifact is not a valid override tier → ex-info. Neutralize
  ;; the bundled tier hermetically: resources/native/windows may be populated on
  ;; this machine (the jar build / run-e2e stage it), so stub io/resource → nil
  ;; rather than assuming an empty classpath.
  (let [dir (temp-dir)]
    (spit (io/file dir (:injector art/artifact-names)) "x") ; only 1 of 3
    (with-redefs [clojure.java.io/resource (constantly nil)]
      (is (thrown? clojure.lang.ExceptionInfo (art/resolve! {:native-dir (.getPath dir)}))))))

(deftest extract-bundled-copies-resource-and-size-skips
  (let [cache (temp-dir)
        f1 (art/extract-bundled! "native/test" "dummy.bin" cache)]
    (is (some? f1))
    (is (.exists f1))
    (let [len (.length f1)
          f2 (art/extract-bundled! "native/test" "dummy.bin" cache)] ; second call: size-skip
      (is (= len (.length f2))))))

(deftest extract-bundled-missing-resource-nil
  (is (nil? (art/extract-bundled! "native/test" "does-not-exist.bin" (temp-dir)))))

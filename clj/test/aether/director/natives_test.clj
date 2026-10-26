(ns aether.director.natives-test
  (:require [aether.director.natives :as natives]
            [clojure.java.io :as io]
            [clojure.test :refer [deftest is testing]]))

(defn- pe
  "A minimal PE header with optional-header magic `magic` (0x10b PE32, 0x20b PE32+)."
  ^bytes [magic]
  (let [b (byte-array 512)
        bb (doto (java.nio.ByteBuffer/wrap b) (.order java.nio.ByteOrder/LITTLE_ENDIAN))]
    (.putShort bb 0 (unchecked-short 0x5A4D))
    (.putInt bb 0x3c 0x80)
    (.putInt bb 0x80 0x4550)
    (.putShort bb (+ 0x80 24) (unchecked-short magic))
    b))

(defn- tmp-dir [] (.toFile (java.nio.file.Files/createTempDirectory "natives" (make-array java.nio.file.attribute.FileAttribute 0))))

(defn- file [dir n ^bytes content] (let [f (io/file dir n)] (io/copy content f) f))

(deftest an-images-bitness
  (let [d (tmp-dir)]
    (is (= 32 (natives/image-bits (file d "x86.exe" (pe 0x10b)))))
    (is (= 64 (natives/image-bits (file d "x64.exe" (pe 0x20b)))))
    (is (nil? (natives/image-bits (file d "text.exe" (.getBytes "not a program")))))
    (is (nil? (natives/image-bits (io/file d "missing.exe"))))))

(deftest the-natives-in-the-images-bitness
  (let [d (tmp-dir)
        x86 (file d "x86.exe" (pe 0x10b))
        x64 (file d "x64.exe" (pe 0x20b))]
    (doseq [n ["vfs-injector.exe" "vfs_shim_dll.dll"]] (file d n (byte-array 1)))
    (is (= {:injector (str (io/file d "vfs-injector.exe")) :shim (str (io/file d "vfs_shim_dll.dll"))}
           (natives/for-image d x64)))
    (testing "a 32-bit image needs the 32-bit pair, and says so"
      (is (thrown-with-msg? clojure.lang.ExceptionInfo #"vfs-injector32.exe not found in .*a 32-bit program"
                            (natives/for-image d x86)))
      (doseq [n ["vfs-injector32.exe" "vfs_shim_dll32.dll"]] (file d n (byte-array 1)))
      (is (= {:injector (str (io/file d "vfs-injector32.exe")) :shim (str (io/file d "vfs_shim_dll32.dll"))}
             (natives/for-image d x86))))
    (is (= (str (io/file d "vfs-injector.exe")) (:injector (natives/for-image d (io/file d "unknown.exe"))))
        "an image that can't be read is taken for 64-bit")))

(ns aether.director.natives
  "The Windows programs a launch runs: the injector, and the shim DLL it
  loads into the target, in the target's bitness. A 64-bit image gets
  vfs-injector.exe and vfs_shim_dll.dll; a 32-bit one (an i386 PE32, run
  under WOW64) vfs-injector32.exe and vfs_shim_dll32.dll, the i686 builds:
  a 64-bit shim can't load into a 32-bit process. The shim then injects the
  children it can (its own bitness) with itself.")

(def names
  "The natives' file names by image bitness."
  {64 {:injector "vfs-injector.exe" :shim "vfs_shim_dll.dll"}
   32 {:injector "vfs-injector32.exe" :shim "vfs_shim_dll32.dll"}})

(defn image-bits
  "32 or 64 for the PE image at `file` (its optional header's magic: 0x10b
  PE32, 0x20b PE32+); nil when it isn't one, or can't be read."
  [file]
  (try
    (with-open [in (java.io.FileInputStream. (str file))]
      (let [buf (byte-array 4096)
            n (.read in buf)
            bb (doto (java.nio.ByteBuffer/wrap buf 0 (max 0 n)) (.order java.nio.ByteOrder/LITTLE_ENDIAN))
            u16 #(bit-and (.getShort bb (int %)) 0xffff)]
        (when (and (> n 0x40) (= 0x5A4D (u16 0)))
          (let [pe (.getInt bb 0x3c)]
            (when (and (< 0 pe (- n 26)) (= 0x4550 (.getInt bb (int pe))))
              (case (u16 (+ pe 24)) 0x10b 32 0x20b 64 nil))))))
    (catch java.io.IOException _ nil)))

(defn for-image
  "{:injector :shim}: the natives in directory `natives` for the image at
  `image` (64-bit when its bitness can't be told). Throws when they aren't
  there, naming the one missing and why it's wanted."
  [natives image]
  (let [bits (or (image-bits image) 64)
        files (update-vals (names bits) #(java.io.File. (str natives) ^String %))]
    (doseq [[_ ^java.io.File f] files]
      (when-not (.isFile f)
        (throw (ex-info (str (.getName f) " not found in " natives
                             (when (= 32 bits) (str ": " image " is a 32-bit program, which needs the 32-bit natives")))
                        {:file (str f) :bits bits}))))
    (update-vals files str)))

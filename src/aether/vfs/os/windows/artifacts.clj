(ns aether.vfs.os.windows.artifacts
  "Resolve the three Windows native artifacts the injected-launch path needs.
  Override order: :native-dir opt -> AETHER_VFS_NATIVE_DIR env -> bundled
  classpath resources (native/windows/<name>) extracted to a cache dir. The
  extraction mechanism itself is OS-neutral (so it is unit-tested on any OS)."
  (:require [clojure.java.io :as io])
  (:import [java.io File]))

(def artifact-names
  "Logical artifact key -> on-disk filename."
  {:injector "vfs-injector.exe"
   :shim-dll "vfs_shim_dll.dll"
   :payload  "vfs_payload.dll"})

(defn- from-dir
  "If `dir` holds ALL three artifacts, return the {:injector.. :shim-dll.. :payload..}
  path map; else nil (an incomplete dir is not a valid tier)."
  [^String dir]
  (when dir
    (let [paths (into {} (map (fn [[k n]] [k (io/file dir n)])) artifact-names)]
      (when (every? #(.exists ^File %) (vals paths))
        (into {} (map (fn [[k ^File f]] [k (.getPath f)])) paths)))))

(defn- cache-dir ^File []
  (io/file (System/getProperty "java.io.tmpdir") "aether-vfs-native"))

(defn extract-bundled!
  "Copy classpath resource <subdir>/<name> to <cache>/<name>, unless a same-size
  copy already exists. Returns the extracted File, or nil if the resource is
  absent from the classpath. Marks the file readable+executable."
  (^File [name cache] (extract-bundled! "native/windows" name cache))
  (^File [subdir name ^File cache]
   (when-some [url (io/resource (str subdir "/" name))]
     (.mkdirs cache)
     (let [dest (io/file cache name)
           bytes (with-open [s (.openStream url)] (.readAllBytes s))]
       (when (or (not (.exists dest)) (not= (alength bytes) (.length dest)))
         (io/copy bytes dest))
       (.setReadable dest true)
       (.setExecutable dest true)
       dest))))

(defn resolve!
  "=> {:injector p :shim-dll p :payload p}. :native-dir opt / AETHER_VFS_NATIVE_DIR
  env win (only if the dir holds all three); else bundled resources extracted to
  the cache dir. Throws ex-info if nothing resolves to a complete set."
  [{:keys [native-dir]}]
  (or (from-dir native-dir)
      (from-dir (System/getenv "AETHER_VFS_NATIVE_DIR"))
      (let [cache (cache-dir)
            resolved (into {} (map (fn [[k n]]
                                     [k (some-> (extract-bundled! n cache) .getPath)]))
                           artifact-names)]
        (when (every? some? (vals resolved)) resolved))
      (throw (ex-info "Cannot resolve Windows native artifacts (injector/shim/payload)"
                      {:tried [:native-dir :env :bundled]
                       :native-dir native-dir
                       :names (vals artifact-names)}))))

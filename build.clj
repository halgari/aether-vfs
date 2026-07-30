(ns build
  (:require [clojure.java.io :as io]
            [clojure.tools.build.api :as b]))

(def lib 'com.halgari/aether-vfs)
(def version (or (System/getenv "AETHER_VFS_VERSION") "0.1.0-SNAPSHOT"))
(def class-dir "target/classes")
(def jar-file (format "target/%s-%s.jar" (name lib) version))
(def stage-dir "resources/native/windows")
(def artifacts ["vfs-injector.exe" "vfs_shim_dll.dll" "vfs_payload.dll"])

(defn stage-native
  "Build the Windows Rust artifacts (release) and copy them into resources so the
  jar bundles them. Best-effort: warns (does not fail) when cargo or the built
  artifacts are unavailable, so a non-Windows jar build still succeeds."
  [_]
  (let [{:keys [exit]} (try
                         (b/process {:command-args ["cargo" "build" "--release"
                                                    "-p" "vfs-inject" "-p" "vfs-shim-dll"
                                                    "-p" "vfs-payload"]
                                     :dir "rust"})
                         (catch Throwable _ {:exit 1}))]
    (when-not (zero? exit)
      (println "WARN: cargo build --release unavailable/failed; staging whatever exists")))
  (.mkdirs (io/file stage-dir))
  (doseq [n artifacts]
    (let [src (io/file "rust/target/release" n)]
      (if (.exists src)
        (do (b/copy-file {:src (str src) :target (str (io/file stage-dir n))})
            (println "staged" n))
        (println "WARN: missing" (str src) "- skipped")))))

(defn jar
  "Stage natives then build an importable source jar (src + resources)."
  [_]
  (stage-native nil)
  (b/write-pom {:class-dir class-dir :lib lib :version version
                :basis (b/create-basis {:project "deps.edn"}) :src-dirs ["src"]})
  (b/copy-dir {:src-dirs ["src" "resources"] :target-dir class-dir})
  (b/jar {:class-dir class-dir :jar-file jar-file})
  (println "wrote" jar-file))

(defn clean [_] (b/delete {:path "target"}))

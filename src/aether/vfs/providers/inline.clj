(ns aether.vfs.providers.inline
  "In-memory byte provider: serves a fixed {path → bytes} map from RAM (no
  store, no cache, no disk). Used for the load-order Plugins.txt overlay, unit
  tests, and the exec-through-FUSE proof. Read-only; wrap in an
  OverlayProvider for writes."
  (:require [clojure.string :as str]
            [aether.vfs.error :as error]
            [aether.vfs.provider :as p])
  (:import (java.util Arrays)))

(defn- dir? [files ^String path]
  (or (= "/" path)
      (let [prefix (str path "/")]
        (boolean (some #(str/starts-with? % prefix) (keys files))))))

(defrecord InlineProvider [files open next-h]
  p/Provider
  (lookup [_ path]
    (if-some [node (get files path)]
      {:size (alength ^bytes (:bytes node))
       :kind :file
       :perm (:perm node)
       :mtime-secs 0
       :cache :cached}
      (if (dir? files path)
        {:size 0 :kind :dir :perm 0755 :mtime-secs 0 :cache :cached}
        (error/raise :not-found (str path " not in inline map")))))

  (readdir [_ path]
    (let [prefix (if (= "/" path) "/" (str path "/"))]
      (first
       (reduce (fn [[out seen] ^String key]
                 (if-not (str/starts-with? key prefix)
                   [out seen]
                   (let [rest-path (subs key (count prefix))
                         head (first (str/split rest-path #"/"))]
                     (if (or (empty? head) (contains? seen head))
                       [out seen]
                       [(conj out {:name head
                                   :kind (if (contains? files (str prefix head)) :file :dir)})
                        (conj seen head)]))))
               [[] #{}]
               (keys files)))))

  (open-file [_ path _flags]
    (when-not (contains? files path)
      (error/raise :not-found (str path " not in inline map")))
    (let [handle (swap! next-h inc)]
      (swap! open assoc handle path)
      {:handle handle :cache :cached}))

  (read-at [_ handle offset size]
    (let [path (or (get @open handle) (error/raise :invalid-argument "bad handle"))
          node (or (get files path) (error/raise :not-found path))
          bytes ^bytes (:bytes node)
          start (min (long offset) (alength bytes))
          end (min (+ start (long size)) (alength bytes))]
      (Arrays/copyOfRange bytes start end)))

  (write-at [_ _handle _offset _data]
    (error/raise :permission-denied "InlineProvider is read-only"))

  (release-handle [_ handle]
    (swap! open dissoc handle)
    nil))

(defn inline-provider
  "entries: [[virtual-path bytes perm] …]. Parent dirs are implicit."
  [entries]
  (->InlineProvider (into {} (map (fn [[path bytes perm]] [path {:bytes bytes :perm perm}])) entries)
                    (atom {})
                    (atom 0)))

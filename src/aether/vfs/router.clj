(ns aether.vfs.router
  "Maps virtual paths to providers via glob patterns; first matching route
  wins, otherwise the default provider."
  (:require [aether.vfs.error :as error])
  (:import (java.nio.file FileSystems Paths)
           (java.util.regex PatternSyntaxException)))

(defn- matcher [pattern]
  (try
    (.getPathMatcher (FileSystems/getDefault) (str "glob:" pattern))
    (catch PatternSyntaxException e
      (error/raise :invalid-argument (str "bad glob " pattern ": " (.getMessage e))))))

(defn router
  "A router over [[pattern provider] …] routes with a default provider.
  Patterns match the full virtual path, e.g. \"/game/**\" or \"/game/*.exe\"."
  ([default] (router default []))
  ([default routes]
   {:default default
    :routes (mapv (fn [[pattern provider]] [(matcher pattern) provider]) routes)}))

(defn provider-for [r ^String path]
  (let [p (Paths/get path (make-array String 0))]
    (or (some (fn [[m provider]]
                (when (.matches ^java.nio.file.PathMatcher m p)
                  provider))
              (:routes r))
        (:default r))))

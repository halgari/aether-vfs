(ns aether.vfs.router-test
  (:require [clojure.test :refer [deftest is]]
            [aether.vfs.provider :as p]
            [aether.vfs.router :as router]))

(defn- tag [name]
  (reify p/Provider
    (lookup [_ _path] (aether.vfs.error/raise :io "tag"))
    (readdir [_ _path] (aether.vfs.error/raise :io "tag"))
    (open-file [_ _path _flags] (aether.vfs.error/raise :io "tag"))
    (read-at [_ _handle _offset _size] (.getBytes ^String name))
    (write-at [_ _handle _offset _data] (aether.vfs.error/raise :io "tag"))
    (release-handle [_ _handle] nil)))

(defn- tag-of [provider]
  (String. ^bytes (p/read-at provider 0 0 0)))

(deftest matched-path-routes-to-provider
  (let [r (router/router (tag "default") [["/game/**" (tag "game")]])]
    (is (= "game" (tag-of (router/provider-for r "/game/a.dat"))))))

(deftest unmatched-path-routes-to-default
  (let [r (router/router (tag "default") [["/game/**" (tag "game")]])]
    (is (= "default" (tag-of (router/provider-for r "/windows/system32"))))))

(deftest first-matching-route-wins
  (let [r (router/router (tag "default")
                         [["/game/*.exe" (tag "exe")]
                          ["/game/**" (tag "game")]])]
    (is (= "exe" (tag-of (router/provider-for r "/game/app.exe"))))))

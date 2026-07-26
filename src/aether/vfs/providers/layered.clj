(ns aether.vfs.providers.layered
  "Stacks two providers with top-wins precedence: top shadows bottom.
  Lookups/opens try top first, falling through to bottom on :not-found;
  readdir unions both (top wins on name collisions). Handles opened by this
  provider are namespaced so read/write/release route back to the layer that
  opened them. Read-focused: in the mount tree it always sits UNDER the
  write-handling OverlayProvider, so writes are never exercised, but they
  route to the owning layer for completeness."
  (:require [aether.vfs.error :as error]
            [aether.vfs.error :as vfs-error]
            [aether.vfs.provider :as p]))

(defn- routed [top bottom layer]
  (case layer :top top :bottom bottom))

(defrecord LayeredProvider [top bottom open next-h]
  p/Provider
  (lookup [_ path]
    (vfs-error/on-not-found (p/lookup top path) (p/lookup bottom path)))

  (readdir [_ path]
    (let [t (try
              (p/readdir top path)
              (catch clojure.lang.ExceptionInfo e
                (if (= :not-found (error/category e)) ::not-found (throw e))))]
      (if (= ::not-found t)
        (p/readdir bottom path)
        (let [b (try (p/readdir bottom path) (catch clojure.lang.ExceptionInfo _ nil))
              seen (into #{} (map :name) t)]
          (into (vec t) (remove #(contains? seen (:name %))) (or b []))))))

  (open-file [_ path flags]
    (let [[layer inner] (vfs-error/on-not-found
                         [:top (p/open-file top path flags)]
                         [:bottom (p/open-file bottom path flags)])
          handle (swap! next-h inc)]
      (swap! open assoc handle [layer (:handle inner)])
      {:handle handle :cache (:cache inner)}))

  (read-at [_ handle offset size]
    (let [[layer inner] (or (get @open handle) (error/raise :invalid-argument "bad handle"))]
      (p/read-at (routed top bottom layer) inner offset size)))

  (write-at [_ handle offset data]
    (let [[layer inner] (or (get @open handle) (error/raise :invalid-argument "bad handle"))]
      (p/write-at (routed top bottom layer) inner offset data)))

  (release-handle [_ handle]
    (when-some [[layer inner] (get @open handle)]
      (swap! open dissoc handle)
      (p/release-handle (routed top bottom layer) inner))
    nil))

(defn layered-provider [top bottom]
  (->LayeredProvider top bottom (atom {}) (atom 0)))

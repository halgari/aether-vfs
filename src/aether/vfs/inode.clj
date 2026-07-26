(ns aether.vfs.inode
  "Bidirectional inode ↔ virtual-path table. Pure functions over a table map."
  (:require [aether.vfs.types :as types]))

(def ^:const root-ino 1)

(defn table []
  {:fwd {root-ino types/root}
   :rev {types/root root-ino}
   :next (inc root-ino)})

(defn get-or-alloc
  "The existing inode for path, or a newly allocated stable one. Returns
  [table' ino]."
  [t path]
  (if-some [ino (get (:rev t) path)]
    [t ino]
    (let [ino (:next t)]
      [(-> t
           (update :fwd assoc ino path)
           (update :rev assoc path ino)
           (update :next inc))
       ino])))

(defn path-of [t ino]
  (get (:fwd t) ino))

(defn rename-path
  "Remap the inode at from to point to to. Must be applied after a successful
  rename so getattr(ino) resolves the new path. If the destination already had
  an inode, it is evicted (the rename overwrote that file; POSIX says the old
  inode is simply gone)."
  [t from to]
  (if-some [ino (get (:rev t) from)]
    (let [old-dst (get (:rev t) to)]
      (cond-> t
        true (update :rev dissoc from)
        old-dst (update :fwd dissoc old-dst)
        true (update :fwd assoc ino to)
        true (update :rev assoc to ino)))
    t))

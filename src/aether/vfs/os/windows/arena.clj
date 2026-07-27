(ns aether.vfs.os.windows.arena
  "Bulk data arena mirror of vfs-ipc::DataArena. Banks are sized per ring slot;
  fill-bank hands the provider a MemorySegment slice of the bank to write into
  directly (zero-copy read destination)."
  (:import [java.lang.foreign MemorySegment]))

(defn make [^MemorySegment seg mapping-offset arena-len banks]
  (let [banks (max 1 (long banks))
        bank-size (max 1 (quot (long arena-len) banks))]
    {:seg seg :mapping-offset (long mapping-offset) :bank-size bank-size :banks banks}))

(defn bank-mapping-offset ^long [arena ^long slot]
  (+ (:mapping-offset arena) (* (mod slot (:banks arena)) (:bank-size arena))))

(defn fill-bank
  "Give f a MemorySegment slice (the bank, capped at max-len and bank-size) to
  write into; return {:offset mapping-offset :len bytes-written}."
  [arena ^long slot ^long max-len f]
  (let [off (bank-mapping-offset arena slot)
        cap (min max-len (:bank-size arena))
        ^MemorySegment slice (.asSlice ^MemorySegment (:seg arena) off (long cap))
        n (long (f slice))]
    {:offset off :len (min n cap)}))

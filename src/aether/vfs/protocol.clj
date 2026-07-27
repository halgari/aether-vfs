(ns aether.vfs.protocol
  "Loads the generated protocol descriptor (single source of truth: the Rust
  vfs-ipc/vfs-protocol crates). Nothing in this library hardcodes a wire or
  ring-layout magic number — it reads them from here. Regenerate the descriptor
  with bin/regen-protocol after any Rust protocol change."
  (:require [clojure.edn :as edn]
            [clojure.java.io :as io]))

(def ^:private resource-name "protocol-descriptor.edn")

(def descriptor
  (delay
    (with-open [r (io/reader (or (io/resource resource-name)
                                 (throw (ex-info "protocol-descriptor.edn not on classpath"
                                                 {:resource resource-name}))))]
      (edn/read (java.io.PushbackReader. r)))))

(def version (:version @descriptor))

(defn op ^long [k] (long (get-in @descriptor [:opcodes k])))
(defn status ^long [k] (long (get-in @descriptor [:statuses k])))
(defn ring-header-offset ^long [k] (long (get-in @descriptor [:ring-header :fields k])))
(defn slot-header-offset ^long [k] (long (get-in @descriptor [:slot-header :fields k])))

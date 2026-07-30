(ns aether.vfs
  "Unified cross-platform entry point. `run` executes a target program inside a
  Provider-backed virtual filesystem, scoped to the program's lifetime:
  FUSE mount on Linux, injected shim on Windows. See the M5 design spec.

  Opts: :exec [cmd & args] (required); :mountpoint (Linux mount dir / Windows
  virtual root); :native-dir (Windows artifacts override, else bundled);
  :env {k v}; :windows {..}/:linux {..} OS-specific passthrough."
  (:require [clojure.string :as str]))

(defn- os-kind []
  (let [n (str/lower-case (System/getProperty "os.name"))]
    (cond (str/starts-with? n "windows") :windows
          (str/starts-with? n "linux")   :linux
          :else :unsupported)))

(defn- to-windows-opts [{:keys [exec mountpoint env windows]} artifacts]
  (merge {:target-exe  (first exec)
          :target-args (vec (rest exec))
          :injector    (:injector artifacts)
          :shim-dll    (:shim-dll artifacts)
          :payload     (:payload artifacts)
          :child-env   (or env {})}
         (when mountpoint {:root mountpoint})
         windows))

(defn- to-linux-opts [{:keys [exec mountpoint env linux]}]
  (merge {:exec exec :mountpoint mountpoint :env (or env {})} linux))

(defn run
  "Run :exec inside a VFS serving `provider`; block until it exits; tear the VFS
  down. Returns the target's exit code (long). Dispatches on the host OS;
  throws ex-info on an unsupported OS.

  NOTE (Windows): the effective virtual root is currently the injected shim's
  default (C:\\GameLayers\\runtime); :mountpoint is passed through but the shim
  root is not yet reconfigurable (tracked follow-up)."
  ^long [provider {:keys [exec native-dir] :as opts}]
  (when-not (seq exec)
    (throw (ex-info "aether.vfs/run: :exec (command vector) is required" {:opts opts})))
  (case (os-kind)
    :windows (let [resolve! (requiring-resolve 'aether.vfs.os.windows.artifacts/resolve!)
                   launch   (requiring-resolve 'aether.vfs.os.windows.launch/launch)]
               (launch provider (to-windows-opts opts (resolve! {:native-dir native-dir}))))
    :linux   (let [lrun (requiring-resolve 'aether.vfs.os.linux.launch/run)]
               (lrun provider (to-linux-opts opts)))
    (throw (ex-info (str "aether.vfs/run is unsupported on this OS: "
                         (System/getProperty "os.name"))
                    {:os (System/getProperty "os.name")}))))

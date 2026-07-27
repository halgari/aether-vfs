(ns aether.vfs.os.linux.proton
  "Run a Windows exe under Proton from a directory (typically an aether-vfs
  mount): build the invocation, spawn it tracked with logs captured, grep the
  logs, tear it down."
  (:require [clojure.java.io :as io])
  (:import (java.io File)))

(defn default-steam-root ^String []
  (str (System/getProperty "user.home") "/.local/share/Steam"))

(defn default-proton-path ^String [steam-root]
  (or (System/getenv "AETHER_VFS_PROTON_PATH")
      (str steam-root "/compatibilitytools.d/GE-Proton10-34/proton")))

(defn proton-command
  "The `proton run <mount>/<exe>` invocation. STEAM_COMPAT_DATA_PATH MUST be the
  caller's throwaway compat dir (never the user's real prefix)."
  [{:keys [proton mountpoint exe steam-root app-id compat]}]
  {:cmd proton
   :args ["run" (str mountpoint "/" exe)]
   :cwd mountpoint
   :env {"STEAM_COMPAT_DATA_PATH" compat
         "STEAM_COMPAT_CLIENT_INSTALL_PATH" steam-root
         "SteamAppId" (str app-id)
         "SteamGameId" (str app-id)}})

(defn launch-proton!
  "Spawn a proton-command as a tracked child, PROTON_LOG on, stdout+stderr →
  <logdir>/run.log. Returns the java.lang.Process."
  ^Process [{:keys [cmd args cwd env]} {:keys [logdir]}]
  (let [pb (ProcessBuilder. ^java.util.List (into [cmd] args))
        e (.environment pb)]
    (doseq [[k v] env] (.put e k v))
    (.put e "PROTON_LOG" "1")
    (.put e "PROTON_LOG_DIR" logdir)
    ;; PROTON_LOG=1 defaults WINEDEBUG to a verbose trace that grows the log to
    ;; GBs in seconds; silence wine trace — the DXVK/Vulkan renderer markers we
    ;; grep for come from DXVK's own logging, unaffected by WINEDEBUG.
    (.put e "WINEDEBUG" "-all")
    (.directory pb (File. ^String cwd))
    (.redirectErrorStream pb true)
    (.redirectOutput pb (File. (str logdir "/run.log")))
    (.start pb)))

(def ^:private drm-fail-re
  #"(?i)steam is not running|must be running|not owned|invalid app|steamstub.*fail|drm.*fail")
(def ^:private reached-re
  #"(?i)dxvk|vulkan|d3d11|createdevice|adapter|skyrim\.esm|renderer|wined3d|vkd3d")

(defn- grep-file
  "Matching lines in one file, streamed line-by-line (PROTON_LOG trace logs grow
  to hundreds of MB — never slurp them whole) and capped."
  [^File f re]
  (with-open [r (io/reader f)]
    (into [] (comp (filter #(re-find re %)) (take 25)) (line-seq r))))

(defn- grep-dir [^String logdir re]
  (->> (.listFiles (File. logdir))
       (mapcat (fn [^File f] (try (grep-file f re) (catch Exception _ []))))
       distinct vec))

(defn drm-failures [logdir] (grep-dir logdir drm-fail-re))
(defn reached-markers [logdir] (grep-dir logdir reached-re))

(defn wineserver-path
  "wineserver lives beside the resolved proton binary at
  <proton-dir>/files/bin/wineserver — derived from the ACTUAL proton path so
  teardown targets the same build that was launched."
  ^String [^String proton]
  (str (.getParent (File. proton)) "/files/bin/wineserver"))

(defn- sh! [& args]
  (try (.waitFor (.start (doto (ProcessBuilder. ^java.util.List (vec args))
                           (.redirectErrorStream true)
                           (.redirectOutput (java.lang.ProcessBuilder$Redirect/to (File. "/dev/null"))))))
       (catch Exception _ nil)))

(defn teardown!
  "Kill OUR streamed game and wineserver ONLY — the exe running at our mount is a
  wine grandchild that outlives the proton wrapper and keeps hammering the mount
  otherwise. pkill by the mount path (never other games), then wineserver -k in
  our throwaway prefix."
  [{:keys [mountpoint exe compat proton]}]
  (sh! "pkill" "-9" "-f" (str mountpoint "/" exe))
  (sh! "bash" "-lc" (str "WINEPREFIX='" compat "/pfx' '" (wineserver-path proton) "' -k")))

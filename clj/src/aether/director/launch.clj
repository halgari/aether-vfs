(ns aether.director.launch
  "Launching a program under the director: the host half of vfs-embed's
  Session::serve + Session::launch.

  The game sees an empty directory, the managed root, as its folder; the shim
  inside it turns every path under that root into a ring request. The shim
  finds the ring through its environment and the session's state directory:

    shim.cfg   VFSC v2: the root and a static-import table
               (vfs-protocol/src/shimcfg.rs)
    fuse.cfg   the ring's coordinates as key=value lines (written for tools;
               the shim reads its environment instead)

  The ring is a named section, or a file (ring/start's :transport); the env
  that names it comes from aether.director.env.

  Starting the process stays native: rust/'s vfs-injector creates it with the
  shim, waits for the shim's ready file, and exits with the target's code.
  The target inherits the injector's environment, and the injector inherits
  ours, so the child's environment is built explicitly here, never by
  changing this process's own.

  session and start! dispatch on the platform (aether.director.platform:
  opts' :platform, else this process's). start! returns a Launch, which a
  host waits on, polls or stops; launch is start! then await-exit (wait is Object.wait, so not a protocol method name).

    :platform/windows  the injector runs directly, with host paths and the
                       section or file ring's env (today's launch, unchanged)
    :platform/linux    the injector runs under GE-Proton's wine, in a prefix
                       the session is linked into (aether.director.proton.*);
                       the pure composition is proton.launch/plan, and every
                       effect not built yet throws {:aether/unimplemented …}"
  (:require [aether.director.env :as env]
            [aether.director.natives :as natives]
            [aether.director.platform :as platform]
            [aether.director.proton.launch :as plaunch]
            [aether.director.proton.layout :as layout]
            [aether.director.proton.prefix :as prefix]
            [aether.director.proton.runtime :as runtime]
            [aether.director.proton.steam :as steam]
            [aether.director.proton.wine :as wine]
            [clojure.java.io :as io]
            [clojure.string :as str])
  (:import (java.io ByteArrayOutputStream File)
           (java.lang ProcessBuilder$Redirect)
           (java.nio ByteBuffer ByteOrder)
           (java.nio.charset StandardCharsets)
           (java.security MessageDigest)
           (java.util List Map)))

(set! *warn-on-reflection* true)

(defn shim-config
  "shim.cfg bytes, version 2 (vfs-protocol/src/shimcfg.rs):
  \"VFSC\" u32 2, u32 len root, u32 n, then n x (u32 len dll, u32 len backing).
  `static-imports` are [dll-name backing-path] pairs. Nothing may follow."
  (^bytes [^String root] (shim-config root []))
  (^bytes [^String root static-imports]
   (let [out  (ByteArrayOutputStream.)
         u32  (fn [n] (.write out (.array (.putInt (.order (ByteBuffer/allocate 4) ByteOrder/LITTLE_ENDIAN) (int n)))))
         str! (fn [^String s] (let [b (.getBytes s StandardCharsets/UTF_8)] (u32 (alength b)) (.write out b)))]
     (.write out (.getBytes "VFSC" StandardCharsets/US_ASCII))
     (u32 2)
     (str! root)
     (u32 (count static-imports))
     (doseq [[dll backing] static-imports] (str! dll) (str! backing))
     (.toByteArray out))))

(defn- canonical ^String [f] (.getCanonicalPath (io/file f)))

(defn fuse-cfg
  "fuse.cfg's text for `server` and the managed root `root`: a section names
  itself and its events, a file its path as the shim opens it."
  ^String [server ^String root]
  (let [{:keys [cap map-bytes arena-offset arena-len]} server
        geometry (str "\npayload_cap=" cap "\nring_bytes=" map-bytes
                      "\narena_offset=" arena-offset "\narena_len=" arena-len)]
    (case (:transport server)
      :section (str "section=" (:name server) "\nroot=" root geometry
                    "\nserver_ev=" (:server-event server) "\nclient_ev=" (:client-event server) "\n")
      :file    (str "ring_path=" (:ring-path server) "\nroot=" root geometry "\n"))))

(defn session-env
  "The shim's env for `server`: the transport's (aether.director.env/transport-env),
  fuse.cfg's path, the managed root, the roots beyond root 0 ({id path}), and
  the registry switch when the server serves one."
  [server {:keys [root fuse roots]}]
  (cond-> (merge (env/transport-env server)
                 {env/fuse-cfg      fuse
                  env/virtual-dir   root
                  ;; set even when empty, as it always was
                  env/virtual-roots (or (env/encode-roots roots) "")})
    (:registry? server) (assoc env/registry "1")))

(defmulti session
  "Creates the session's directories under `dir`: root/ (the managed root,
  empty) and state/, and writes shim.cfg and fuse.cfg for `server` (from
  ring/start). Returns the session map start! takes. `roots` declares the
  roots beyond root 0 to the shim: {id path-the-program-uses}. When the
  server serves a registry, the shim is told to install its registry hooks.
  Dispatches on (platform/current opts)."
  {:arglists '([server dir & [{:keys [roots platform]}]])}
  (fn [_server _dir & [opts]] (platform/current opts)))

(defmethod session :platform/windows
  [server dir & [{:keys [roots]}]]
  (let [[root state] (for [d ["root" "state"]] (doto (io/file dir d) (.mkdirs)))
        root-s  (canonical root)
        shim    (io/file state "shim.cfg")
        fuse    (io/file state "fuse.cfg")]
    (io/copy (shim-config root-s) shim)
    (spit fuse (fuse-cfg server root-s))
    {:root root-s :state (canonical state)
     :shim-config (canonical shim) :ready (str (canonical state) File/separator "ready.flag")
     :env (session-env server {:root root-s :fuse (canonical fuse) :roots roots})}))

(defn launch-env
  "The child's env for a launch under `session`, as {:remove names :set [[k
  v] …]} for apply-env!: every transport name the launch doesn't set is
  removed (an earlier session's would otherwise reach the child), and so is
  each of `remove-env` (in any spelling); then the session's env, the
  injector's, `natives-dir` in front of `host-path` on PATH, the staged
  `launch-image`, and last the caller's `env`, which wins."
  [session {:keys [env cwd launch-image remove-env]} ^String natives-dir host-path]
  (let [sets (-> (vec (:env session))
                 (conj [env/inject-cwd (if cwd (str (io/file (:root session) (str cwd))) (:root session))]
                       ;; Remote-thread injection, always: the shim is loaded into the
                       ;; suspended process by full path. (The injector's alternative patches
                       ;; the image's import table in memory, which a self-verifying SteamStub
                       ;; wrapper such as SkyrimSE.exe refuses: "Application load error
                       ;; 3:0000065432".)
                       [env/force-injection "1"]
                       ;; The shim's own DLLs (vcruntime140 in a bundle) resolve from the
                       ;; natives directory: a staged EXE's folder doesn't hold them.
                       ["PATH" (str natives-dir File/pathSeparator host-path)])
                 (cond-> launch-image (conj [env/launch-image (str launch-image)]))
                 (into env))
        set-names (set (map first sets))]
    ;; transport names only: on Windows the host may set the injector's own
    {:remove (into (env/stale set-names env/transport) (remove set-names) remove-env)
     :set    sets}))

(defn apply-env!
  "Applies launch-env's result to `m`, a ProcessBuilder's environment."
  ^Map [^Map m delta]
  (env/apply! m delta))

;; --- a started launch -------------------------------------------------------------

(defprotocol Launch
  "A started program, as start! returns it: stateful, so a host can poll and
  stop it while another thread waits."
  (await-exit [l] "Blocks until the program has ended; its exit code (the injector's own failures are 3).")
  (alive? [l] "Whether it is still running.")
  (stop! [l] "Stops it and everything it started. Idempotent."))

(defmulti start!
  "Starts `opts`' program under `session` and returns its Launch, without
  waiting. Dispatches on (platform/current opts)."
  {:arglists '([session opts])}
  (fn [_session opts] (platform/current opts)))

(defn launch
  "Runs a program under `session` (start!, see the platform's method for
  `opts`), awaits its exit, and returns its exit code."
  [session opts]
  (await-exit (start! session opts)))

;; --- Windows ----------------------------------------------------------------------

(defrecord WindowsLaunch [^Process process]
  Launch
  (await-exit [_] (.waitFor process))
  (alive? [_] (.isAlive process))
  (stop! [_]
    ;; the target is the injector's child: kill it before the injector
    (run! #(.destroyForcibly ^ProcessHandle %) (.toList (.descendants process)))
    (.destroyForcibly process)
    nil))

(defn windows-command
  "What the Windows start! spawns for `opts` under `session`: {:argv
  :dir :env}, :env being launch-env's delta. `host-path` is this process's
  PATH. Throws for a native missing from `natives`."
  [session {:keys [natives exe args] :as opts} host-path]
  (let [{:keys [injector shim]} (natives/for-image natives exe)]
    (doseq [f [injector shim]]
      (when-not (.isFile (io/file f))
        (throw (ex-info (str (.getName (io/file f)) " not found in " natives) {:file (io/file f)}))))
    {:argv (vec (concat [(canonical (io/file injector)) (canonical exe)
                         (canonical (io/file shim))
                         (:shim-config session) (:ready session)]
                        (when (seq args) (cons "--" args))))
     :dir  (:root session)
     :env  (launch-env session opts (canonical natives) host-path)}))

(defmethod start! :platform/windows
  ;; Runs `exe` (a real file) with `args` under `session`, via the injector at
  ;; `natives` (a directory holding vfs-injector.exe and vfs_shim_dll.dll), with
  ;; the managed root as its working directory. `env` adds variables for the
  ;; target; `launch-image` (a staged EXE) makes that EXE's folder an alias of
  ;; the root. `remove-env` names inherited variables the target must not see
  ;; (matched without case). Output goes to ours.
  [session opts]
  (let [{:keys [argv dir env]} (windows-command session opts (System/getenv "PATH"))
        pb (doto (ProcessBuilder. ^List argv)
             (.directory (io/file dir))
             (.redirectOutput ProcessBuilder$Redirect/INHERIT)
             (.redirectError ProcessBuilder$Redirect/INHERIT))]
    (io/delete-file (:ready session) true)
    (apply-env! (.environment pb) env)
    (->WindowsLaunch (.start pb))))

;; --- Linux ------------------------------------------------------------------------

(defmethod session :platform/linux
  ;; As the Windows method, for a launch under Wine: shim.cfg names root 0 by
  ;; its C: location (:root-location, default proton.launch/default-root-location,
  ;; inside the session's link), and `roots` are {id C:-location}. `server`
  ;; must be a file ring named in C: form (ring/start :shim-path); its geometry
  ;; is kept for proton.launch/plan. No env: the plan builds the child's.
  [server dir & [{:keys [roots root-location]}]]
  (let [ring-path (:ring-path server)]
    (when-not (and (= :file (:transport server)) ring-path (prefix/windows-absolute? ring-path))
      (throw (ex-info (str "a Wine launch needs a file ring named in C: form (ring/start :shim-path), not "
                           (pr-str (or ring-path (:transport server))))
                      {::plaunch/error :ring-not-wine :server (select-keys server [:transport :ring-path])}))))
  (let [[root state] (for [d ["root" "state"]] (doto (io/file dir d) (.mkdirs)))
        root0 (or root-location plaunch/default-root-location)
        shim  (io/file state "shim.cfg")
        fuse  (io/file state "fuse.cfg")]
    (io/copy (shim-config root0) shim)
    (spit fuse (fuse-cfg server root0))
    {:dir (canonical dir) :root (canonical root) :state (canonical state)
     :shim-config (canonical shim) :ready (str (canonical state) "/ready.flag")
     :root-location root0 :roots (or roots {})
     :server (select-keys server [:transport :ring-path :host-path :map-bytes :cap :arena-offset :arena-len
                                  :registry?])}))

(defn- session-id
  "The anonymous prefix's name for a session: its state dir, hashed (as
  wine_session_id, session/proton/mod.rs:374-379; only stable for itself)."
  [^String state]
  (let [h (.digest (MessageDigest/getInstance "SHA-256") (.getBytes state StandardCharsets/UTF_8))]
    (apply str "session-" (for [i (range 8)] (format "%02x" (bit-and (aget h i) 0xff))))))

(defn resolve-proton
  "The runtime, prefix and Steam decision for a Linux launch, from `opts`'
  :proton {:home :runtime :prefix-name :prefix-init :helper? :steam-state-dir}
  (home: layout/home of `env`; runtime: the newest installed; prefix name:
  the session's own; prefix-init: :wineboot) and the launch's :env. Reads
  the runtime's version file and the Steam client's pid file, nothing else."
  [session {:keys [proton] :as opts} env]
  (let [{:keys [home runtime prefix-name prefix-init helper? steam-state-dir]
         :or {prefix-init :wineboot helper? true}} proton
        home    (or home (layout/home env)
                    (throw (ex-info "no aether-vfs home: set VFS_HOME, XDG_DATA_HOME or HOME" {})))
        runtime (or runtime (runtime/newest home)
                    (throw (ex-info (str "no GE-Proton runtime under " (layout/runtimes home)
                                         ": install one (spec task 13's CLI)") {:home home})))
        _       (runtime/verify-ge runtime)
        state   (or steam-state-dir (steam/state-dir (get env "HOME")))]
    {:home    home
     :runtime runtime
     :prefix  (prefix/prefix-dir home (or prefix-name (session-id (:state session))) prefix-init)
     :prefix-init prefix-init
     :steam   (steam/steam-side {:prefix-init prefix-init :helper? helper? :env (:env opts) :state-dir state
                                 :client-pid (when state (steam/running-client state))})}))

(defrecord LinuxLaunch [^Process process plan quiet requested ended cleanup]
  ;; liveness is the prefix's, not wine's: a launcher that starts the game
  ;; and exits (skse64_loader.exe -> SkyrimSE.exe) keeps the launch alive
  ;; until wineserver -w returns (session/proton/handle.rs)
  Launch
  (await-exit [_]
    (let [code (.waitFor process)]
      (.waitFor ^Process (swap! quiet #(or % (wine/wineserver-wait (:runtime plan) (:prefix plan)))))
      (when (compare-and-set! ended false true) (cleanup))
      (if @requested
        code
        (let [report (let [f (io/file (plaunch/injector-error-path (get-in plan [:files :ready])))]
                       (when (.isFile f) (slurp f)))
              {:keys [exit error] :as r} (plaunch/finish plan code report)]
          (if error (throw (ex-info error r)) exit)))))
  (alive? [_]
    (or (.isAlive process)
        (and (not @ended)
             (.isAlive ^Process (swap! quiet #(or % (wine/wineserver-wait (:runtime plan) (:prefix plan))))))))
  (stop! [_]
    (when-not @ended
      (reset! requested true)
      (wine/stop-wineserver! (:runtime plan) (:prefix plan)))
    nil))

(defmethod start! :platform/linux
  ;; `opts`: as proton.launch/plan's, plus :proton (resolve-proton) and
  ;; :log-file (wine's output; default ours). Sets the prefix up under its
  ;; lock, links the session and the roots into it, and spawns wine; the
  ;; Launch releases the lock and removes the root links when it ends.
  [session opts]
  (let [env     (into {} (System/getenv))
        p       (resolve-proton session opts env)
        release (wine/lock! (:prefix p))]
    (try
      (let [init   (:prefix-init p)
            name   (get-in opts [:proton :prefix-name] (session-id (:state session)))
            _      (wine/ensure! (:home p) (:runtime p) name init)
            root0  (:root-location session)
            locs   (cons root0 (vals (:roots session)))
            plan   (plaunch/plan session opts
                                 (assoc p :listing (wine/listing (:prefix p) locs)
                                          :manifest (wine/read-manifest (:prefix p)))
                                 {:env env :cwd (System/getProperty "user.dir")})
            plan   (assoc plan :files {:ready (:ready session)})]
        (wine/link-session! (:session-link plan))
        (doseq [step (:links plan)] (wine/link! (:prefix p) step))
        (plaunch/check-geometry (assoc (:geometry plan) :ring-host-path (:ring-host-path plan))
                                (let [f (io/file (:ring-host-path plan))] (when (.isFile f) (.length f))))
        (run! #(io/delete-file % true) (:reports plan))
        (io/delete-file (:ready session) true)
        (let [proc (wine/spawn! plan (:log-file opts))]
          (->LinuxLaunch proc plan (atom nil) (atom false) (atom false)
                         (fn []
                           (doseq [step (:links plan)]
                             (try (wine/unlink! (:prefix p) step) (catch Exception _)))
                           (release)))))
      (catch Throwable t (release) (throw t)))))

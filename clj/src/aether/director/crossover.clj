(ns aether.director.crossover
  "Launching under CrossOver on macOS: the :platform/macos methods of
  aether.director.launch's session and start! (vfs-proton/src/crossover.rs
  and vfs-embed's shared-prefix launch, on the macos-portability branch).

  The Wine half is the Linux one: the injector's argv, the file ring named in
  C: form, the session linked into the prefix at C:\\vfs-session
  (proton.launch, proton.prefix). CrossOver differs where its install does:

  - Installed by the user. Nothing is downloaded and nothing is gated on its
    version; an install is verified by its parts (install).
  - bin/wine is a Perl front end, not Wine's loader. It picks a bottle before
    anything else, overwriting WINEPREFIX, so the bottle is named with
    `--bottle <absolute path>`; `--no-convert` stops it rewriting arguments
    that name host files (Wine resolves those through Z: either way); CX_ROOT
    lets it find its own modules.
  - The bottle is shared. A Steam-DRM'd game must run in the same prefix as a
    signed-in Windows Steam client (CrossOver has no bridge to the native
    one), so a launch goes into an existing bottle where one runs (default:
    <VFS_HOME>/sessions/steam/prefix) and neither sets it up nor stops it.
    The launch is its own wine process: stopping it ends that and what it
    started, never the wineserver, which Steam would go down with. Steam's
    ActiveProcess key is left alone too: no VFS_INJECT_STEAM_HELPER.
  - Roots beyond 0 are virtualized at their C: locations by the shim alone,
    as on Windows; nothing is linked over a folder the bottle already has.

  CrossOver's Unix side is x86_64, so on Apple Silicon Wine runs under
  Rosetta while the director is native; the ring is a file both map, which
  is coherent across the two."
  (:require [aether.director.env :as henv]
            [aether.director.launch :as launch]
            [aether.director.proton.env :as penv]
            [aether.director.proton.launch :as plaunch]
            [aether.director.proton.layout :as layout]
            [aether.director.proton.prefix :as prefix]
            [clojure.java.io :as io]
            [clojure.string :as str])
  (:import (java.lang ProcessBuilder$Redirect)
           (java.nio.file Files LinkOption)
           (java.nio.file.attribute FileAttribute)
           (java.util List)))

(set! *warn-on-reflection* true)

(def app-env
  "Names the CrossOver install: the bundle (…/CrossOver.app) or its
  Contents/SharedSupport/CrossOver (vfs-env CROSSOVER_APP)."
  "VFS_CROSSOVER_APP")

(def ^:private shared-support "Contents/SharedSupport/CrossOver")

(def default-prefix-name
  "The bottle a launch shares when none is named: <VFS_HOME>/sessions/<name>/prefix,
  the one Windows Steam is installed and signed in to."
  "steam")

(defn- fail [kind msg data]
  (throw (ex-info msg (assoc data ::error kind))))

(defn verify
  "The directory holding bin/wine for `app`, a CrossOver bundle or its
  Contents/SharedSupport/CrossOver, when the parts a launch uses are there;
  else throws ::error :crossover naming the first one missing."
  [app]
  (let [app   (io/file app)
        _     (when-not (.exists app)
                (fail :crossover (str "no CrossOver at " app) {:app (str app)}))
        inner (io/file app shared-support)
        dir   (if (.isDirectory inner) inner app)]
    (doseq [part ["bin/wine" "bin/wineserver" "lib/wine/x86_64-windows"]
            :let [f (io/file dir part)]]
      (when-not (.exists f)
        (fail :crossover (str "incomplete CrossOver install: " f " is missing") {:missing (str f)})))
    (.getAbsolutePath dir)))

(defn install
  "The CrossOver install a launch uses: `app` when given, else $VFS_CROSSOVER_APP
  (only that one, so a wrong value is an error rather than a silent
  fallback), else the first of /Applications/CrossOver.app and
  ~/Applications/CrossOver.app that verifies."
  [app env]
  (if-let [a (or app (not-empty (get env app-env)))]
    (verify a)
    (let [candidates (cons "/Applications/CrossOver.app"
                           (when-let [h (not-empty (get env "HOME"))] [(str h "/Applications/CrossOver.app")]))]
      (or (some #(try (verify %) (catch clojure.lang.ExceptionInfo _ nil)) candidates)
          (verify (first candidates))))))

(defn wine
  "CrossOver's `wine` front end in the install `dir`."
  [dir]
  (layout/join dir "bin" "wine"))

(defn bottle-args
  "What puts a `wine` run in the bottle at `prefix`, before the program. The
  path is made absolute: a relative one would name a bottle in CrossOver's
  own list."
  [prefix]
  ["--bottle" (.getAbsolutePath (io/file (str prefix))) "--no-convert"])

(defn resolve-host
  "A macOS launch's {:install :prefix}, from `opts`' :crossover {:app :prefix
  :prefix-name} and the environment `env`: the install (install), and the
  bottle, by default <home>/sessions/<prefix-name or steam>/prefix. The bottle
  must already be set up (drive_c/windows/system32): a launch shares it and
  never creates one."
  [{:keys [crossover]} env]
  (let [{:keys [app prefix-name] bottle :prefix} crossover
        home   (layout/home env)
        bottle (or bottle
                   (when home (layout/join (layout/try-session-dir home (or prefix-name default-prefix-name)) "prefix"))
                   (fail :no-bottle "no aether-vfs home to find the bottle in: set VFS_HOME or HOME" {}))]
    (when-not (prefix/initialised? bottle #(.isDirectory (io/file (str %))))
      (fail :no-bottle (str "no CrossOver bottle at " bottle ": a launch shares an existing one, where a "
                            "signed-in Windows Steam runs; name another with :crossover {:prefix dir}")
            {:prefix bottle}))
    {:install (install app env) :prefix bottle}))

(defn reserved?
  "Whether the caller's env may not set `name`: what the launch sets itself."
  [^String name]
  (or (penv/reserved? name) (.equalsIgnoreCase "CX_ROOT" name)))

(defn launch-env
  "The child's environment, as a sorted map: CX_ROOT; WINEDLLOVERRIDES
  (penv/base-dll-overrides, merged per DLL with the caller's); WINEDEBUG (the
  host's, else -all); the shim's ring geometry, root 0 and the other roots,
  and the registry switch; the injector's cwd, the staged image, and
  remote-thread injection, as on Windows (launch/launch-env says why); then
  the caller's `env` apart from reserved names. No VFS_INJECT_STEAM_HELPER: the
  bottle's own Steam is left alone."
  [{:keys [install server root0 roots cwd image env host-env]}]
  (let [{:keys [ring-path map-bytes cap arena-offset arena-len registry?]} server]
    (as-> (sorted-map "CX_ROOT"              (str install)
                      "WINEDLLOVERRIDES"     (penv/merge-dll-overrides penv/base-dll-overrides (get env "WINEDLLOVERRIDES"))
                      "WINEDEBUG"            (or (get host-env "WINEDEBUG") penv/default-winedebug)
                      henv/ring-path         (str ring-path)
                      henv/ring-bytes        (str map-bytes)
                      henv/ring-payload-cap  (str cap)
                      henv/arena-offset      (str arena-offset)
                      henv/arena-len         (str arena-len)
                      henv/virtual-dir       (str root0)
                      henv/inject-cwd        (str cwd)
                      henv/force-injection   "1") e
      (cond-> e
        (seq roots) (assoc henv/virtual-roots (henv/encode-roots roots))
        registry?   (assoc henv/registry "1")
        image       (assoc henv/launch-image image))
      (reduce-kv (fn [e k v] (if (or (reserved? k) (= "WINEDLLOVERRIDES" k)) e (assoc e k v)))
                 e (or env {})))))

(defn- links-only?
  "Whether `dir` is a folder holding nothing but symlinks: an earlier
  launch's links (vfs-embed linked the session's parts one by one)."
  [^java.io.File dir]
  (and (.isDirectory dir)
       (every? #(Files/isSymbolicLink (.toPath ^java.io.File %)) (.listFiles dir))))

(defn link-session!
  "Points <prefix>/drive_c/vfs-session at the session `dir`, so its state/,
  stage/ and root/ have C: names. A link an earlier session left is
  replaced, as is a folder of nothing but links (the links themselves are
  removed, never what they point at); anything else there is refused."
  [prefix dir]
  (let [link   (.toPath (io/file (str prefix) "drive_c" "vfs-session"))
        target (.toPath (io/file (str dir)))]
    (cond
      (Files/isSymbolicLink link) (Files/delete link)
      (links-only? (.toFile link)) (do (run! #(Files/delete (.toPath ^java.io.File %)) (.listFiles (.toFile link)))
                                       (Files/delete link))
      (Files/exists link (make-array LinkOption 0))
      (fail :bad-location (str link " exists in the bottle and isn't a link aether-vfs made; remove it to launch")
            {:link (str link)}))
    (Files/createSymbolicLink link target (make-array FileAttribute 0))
    (str link)))

;; --- the bottle's user folders ------------------------------------------------------

(defn isolate-user-folders!
  "Replaces the links CrossOver makes from the bottle's user folders
  (drive_c/users/<user>/Documents, Downloads, Music, Pictures, Videos) to the
  Mac's own with folders inside the bottle. Wine goes through them as it
  starts (the shell folders, Steam, the game), and through the links that is
  the Mac's Documents, Downloads, Music, Pictures and Movies: folders macOS
  guards, so each one asks the user to allow the app. The game's own folders
  there (My Games) are the shim's roots, not these. Returns the paths whose
  links were replaced."
  [prefix]
  (let [bottle (.toPath (.getCanonicalFile (io/file (str prefix))))]
    (vec (for [^java.io.File user (or (.listFiles (io/file (str prefix) "drive_c" "users")) [])
               :when (.isDirectory user)
               ^java.io.File f (or (.listFiles user) [])
               :let [p (.toPath f)]
               :when (and (Files/isSymbolicLink p)
                          ;; both canonical: /var is /private/var, say
                          (not (.startsWith (.toPath (.getCanonicalFile (.toFile (.resolveSibling p (Files/readSymbolicLink p)))))
                                            bottle)))]
           (do (Files/delete p)
               (Files/createDirectory p (make-array FileAttribute 0))
               (str p))))))

(defn prune-drives!
  "Removes the bottle's drives but C: (the bottle) and Z: (/, which the
  injector's own paths go through): Y:, CrossOver's link to the Mac's home
  folder, and the drive Wine adds for every disk image or volume mounted.
  Through them Wine (Steam's library scan, a file dialog) reaches the
  Mac's guarded folders and volumes, each asking the user to allow the app.
  Wine adds a volume's drive again when one is mounted, so this runs before
  each launch. Returns the drives removed."
  [prefix]
  (vec (for [^java.io.File f (or (.listFiles (io/file (str prefix) "dosdevices")) [])
             :let [n (str/lower-case (.getName f))]
             :when (and (re-matches #"[a-y]::?" n) (not (str/starts-with? n "c:"))
                        (Files/isSymbolicLink (.toPath f)))]
         (do (Files/delete (.toPath f)) (.getName f)))))

;; --- Steam in the bottle ------------------------------------------------------------
;;
;; A Steam game's DRM (SteamStub) asks the Steam client running in its prefix
;; whether the account owns it. With none running, it starts one with
;; steam://run/<app>, and that Steam, which has no copy of the game, offers to
;; install it: the game never starts, and must not be installed that way. So
;; a Steam game starts only once the bottle's own Steam is running and signed
;; in, started here (silently, with no run link) when it isn't.

(def steam-exe "C:\\Program Files (x86)\\Steam\\steam.exe")

(def steam-sign-in-ms
  "How long the bottle's Steam may take to start and sign in."
  120000)

(defn- steam-dir ^java.io.File [prefix]
  (io/file (str prefix) "drive_c" "Program Files (x86)" "Steam"))

(defn- wine-process
  "A ProcessBuilder for `args` (a program and its arguments, Windows paths)
  in the bottle at `prefix`, through the install's wine."
  ^ProcessBuilder [install prefix args]
  (let [pb (ProcessBuilder. ^List (into [(wine install)] (concat (bottle-args prefix) args)))]
    (.put (.environment pb) "CX_ROOT" (str install))
    pb))

(defn steam-running?
  "Whether the bottle's Steam client is running: the bottle's own process
  list (tasklist: not another bottle's, nor the Mac's Steam) has steam.exe."
  [install prefix]
  (let [p (.start (doto (wine-process install prefix ["C:\\windows\\system32\\tasklist.exe"])
                    (.redirectError ProcessBuilder$Redirect/DISCARD)))
        out (slurp (.getInputStream p))]
    (.waitFor p)
    (boolean (re-find #"(?im)^steam\.exe\s" out))))

(defn- log-tail
  "The last 64 KB of Steam's connection log, or nil."
  [prefix]
  (let [f (io/file (steam-dir prefix) "logs" "connection_log.txt")]
    (when (.isFile f)
      (with-open [r (java.io.RandomAccessFile. f "r")]
        (let [n (min (.length r) 65536) b (byte-array n)]
          (.seek r (- (.length r) n))
          (.readFully r b)
          (String. b java.nio.charset.StandardCharsets/UTF_8))))))

(def ^:private log-line #"(?m)^\[(\d{4}-\d\d-\d\d \d\d:\d\d:\d\d)\] \[([A-Za-z ]+),[^\n]*$")

(defn- log-ms [ts]
  (-> (java.time.LocalDateTime/parse ts (java.time.format.DateTimeFormatter/ofPattern "yyyy-MM-dd HH:mm:ss"))
      (.atZone (java.time.ZoneId/systemDefault)) .toInstant .toEpochMilli))

(defn signed-in?
  "Whether Steam's connection log says it's signed in: its last state is
  Logged On, as of `since` (ms) or later when given. The log's times are
  the Mac's local time, to the second."
  ([prefix] (signed-in? prefix nil))
  ([prefix since]
   (when-let [[_ ts state] (last (re-seq log-line (or (log-tail prefix) "")))]
     (and (= "Logged On" state) (or (nil? since) (>= (log-ms ts) (long since)))))))

(defn ensure-steam!
  "Returns once the bottle's Steam is running and signed in, starting it
  (steam.exe -silent: no run link, so no install offer) when it isn't, its
  output to `log`. Throws ::error :no-steam when the bottle has no Steam,
  :steam-not-signed-in when it doesn't sign in within steam-sign-in-ms:
  either way the game isn't started."
  [install prefix log]
  (when-not (.isFile (io/file (steam-dir prefix) "steam.exe"))
    (fail :no-steam (str "Steam isn't installed in the bottle " prefix ": a Steam game needs its "
                         "Windows client running there, signed in to the account that owns the game")
          {:prefix (str prefix)}))
  (let [t0 (System/currentTimeMillis)
        running (steam-running? install prefix)]
    (if (and running (signed-in? prefix))
      :running
      (do (when-not running
            (.start (doto (wine-process install prefix [steam-exe "-silent"])
                      (.redirectErrorStream true)
                      (.redirectOutput (ProcessBuilder$Redirect/appendTo (io/file (str log)))))))
          ;; the log's seconds: a sign-in in the second it started counts
          (loop []
            (cond
              (signed-in? prefix (- t0 1000)) :started
              (> (- (System/currentTimeMillis) t0) (long steam-sign-in-ms))
              (fail :steam-not-signed-in
                    (str "Steam in the bottle didn't sign in within " (quot (long steam-sign-in-ms) 1000)
                         " seconds, so the game wasn't started: without Steam it would ask Steam to install it. "
                         "Open Steam in the bottle, sign in, and play again.")
                    {:prefix (str prefix)})
              :else (do (Thread/sleep 1000) (recur))))))))

;; --- the session and the launch -----------------------------------------------------

(defmethod launch/session :platform/macos
  ;; The Linux session: shim.cfg names root 0 by its C: location inside the
  ;; session's link, and the server must be a file ring named in C: form.
  [server dir & [opts]]
  ((get-method launch/session :platform/linux) server dir opts))

(defn- processes
  "Every process as {:pid :pgid :command}, from ps: Wine rewrites a Windows
  process's argv, which ps shows and ProcessHandle (macOS) doesn't."
  []
  (let [p (.start (ProcessBuilder. ^"[Ljava.lang.String;" (into-array String ["ps" "-axww" "-o" "pid=,pgid=,command="])))
        out (slurp (.getInputStream p))]
    (.waitFor p)
    (for [line (str/split-lines out)
          :let [[_ pid pgid command] (re-matches #"\s*(\d+)\s+(\d+)\s+(.*)" line)]
          :when pid]
      {:pid (parse-long pid) :pgid (parse-long pgid) :command command})))

(def ^:private own-pgid
  (delay (let [me (.pid (ProcessHandle/current))]
           (some #(when (= me (:pid %)) (:pgid %)) (processes)))))

(defn launch-group
  "The process group of the launch whose ready file is `ready`, or nil when
  none runs. Wine hands its processes to launchd, so the injector and the
  game are no descendants of ours; the injector leads a group of its own,
  which the game joins. It's the process whose command line is `injector`
  (its own path, as the launch ran it) and names this session's ready file:
  not wine's front end, whose command line names both too but which is in
  our own group, and never our own group, whatever matches."
  [injector ready]
  (some #(when (and (str/starts-with? (:command %) (str injector))
                    (str/includes? (:command %) (str ready))
                    (not= (:pgid %) @own-pgid))
           (:pgid %))
        (processes)))

(defn- group-alive? [pgid]
  (boolean (and pgid (some #(= pgid (:pgid %)) (processes)))))

(defrecord CrossOverLaunch [^Process process injector ready group]
  launch/Launch
  (await-exit [_]
    ;; the front end, then the injector's group: the front end needn't wait
    ;; for what the injector started
    (let [code   (.waitFor process)
          _      (loop [] (when (group-alive? (or @group (reset! group (launch-group injector ready))))
                            (Thread/sleep 500)
                            (recur)))
          report (let [f (io/file (plaunch/injector-error-path ready))] (when (.isFile f) (slurp f)))]
      (when (and (#{2 3} code) report)
        (binding [*out* *err*]
          (println "aether-vfs: vfs-injector did not run the target:" (plaunch/describe-injector-error report))))
      code))
  (alive? [_] (or (.isAlive process) (group-alive? (or @group (reset! group (launch-group injector ready))))))
  (stop! [_]
    ;; the wine front end and the injector's group (the game, and what it
    ;; started), never the bottle's wineserver or Steam: they run there too
    (when-let [g (or @group (launch-group injector ready))]
      (.waitFor (.start (ProcessBuilder. ^"[Ljava.lang.String;" (into-array String ["kill" "-KILL" (str "-" g)])))))
    (run! #(.destroyForcibly ^ProcessHandle %) (.toList (.descendants process)))
    (.destroyForcibly process)
    nil))

(defn command
  "What start! spawns for `opts` under `session`: {:argv :dir :env :log
  :link}, :env being launch/apply-env!'s delta. Throws for a native missing
  from :natives."
  [{:keys [dir root state shim-config ready root-location roots server]}
   {:keys [natives exe launch-image args env remove-env cwd]}
   {:keys [install prefix]} host-env]
  (doseq [n ["vfs-injector.exe" "vfs_shim_dll.dll"]]
    (when-not (.isFile (io/file (str natives) n))
      (throw (ex-info (str n " not found in " natives) {:file (str (io/file (str natives) n))}))))
  (penv/check-extra-env (into {} (remove (fn [[k]] (= "WINEDLLOVERRIDES" k))) env))
  (let [natives (.getAbsolutePath (io/file (str natives)))
        target  (plaunch/wine-path dir (str (or exe launch-image)))
        ;; a host builds C: locations with its own path joins: Wine's separator only
        roots   (into {} (for [[id at] roots] [id (str/replace (str at) "/" "\\")]))
        child   (launch-env {:install install :server server :root0 root-location :roots roots
                             :cwd (prefix/wine-cwd cwd root-location target)
                             :image (when launch-image (plaunch/wine-path dir (str launch-image)))
                             :env env :host-env host-env})
        ;; the injector's positional contract, as the Linux launch builds it
        [_ argv] (plaunch/command-line {:runtime install
                                        :injector (layout/join natives "vfs-injector.exe")
                                        :target target
                                        :shim-dll (layout/join natives "vfs_shim_dll.dll")
                                        :config-file shim-config :ready-file ready :args args})]
    {:argv (into [(wine install)] (concat (bottle-args prefix) argv))
     :dir  root
     :env  {:remove (into (henv/stale child) (remove #(contains? child %)) remove-env)
            :set    (vec child)}
     :log  (layout/join state "wine.log")}))

(defmethod launch/start! :platform/macos
  ;; Runs `exe` (or the staged `launch-image`) under `session` in the shared
  ;; bottle (resolve-host, from opts' :crossover), through CrossOver's wine
  ;; and the injector at `natives`. `env` adds variables for the target;
  ;; `remove-env` names inherited ones it must not see. Wine's output goes to
  ;; <session>/state/wine.log.
  [session opts]
  (let [host-env (into {} (System/getenv))
        host     (resolve-host opts host-env)
        {:keys [argv dir env log]} (command session opts host host-env)
        server   (:server session)]
    (plaunch/check-geometry {:ring-bytes (:map-bytes server) :arena-offset (:arena-offset server)
                             :arena-len (:arena-len server) :ring-host-path (:host-path server)}
                            (let [f (io/file (str (:host-path server)))] (when (.isFile f) (.length f))))
    (run! #(io/delete-file % true) (conj (plaunch/stale-reports (:ready session)) (:ready session)))
    (isolate-user-folders! (:prefix host))
    (prune-drives! (:prefix host))
    ;; a Steam game (its app id in the env, as a host sets it) only with the
    ;; bottle's Steam running and signed in: never one the game starts
    (when (get (:env opts) "SteamAppId")
      (ensure-steam! (:install host) (:prefix host) log))
    (link-session! (:prefix host) (:dir session))
    (let [pb (doto (ProcessBuilder. ^List argv)
               (.directory (io/file (str dir)))
               (.redirectErrorStream true)
               (.redirectOutput (ProcessBuilder$Redirect/appendTo (io/file (str log)))))]
      (launch/apply-env! (.environment pb) env)
      ;; argv: wine --bottle <prefix> --no-convert <injector> …
      (->CrossOverLaunch (.start pb) (nth argv 4) (:ready session) (atom nil)))))

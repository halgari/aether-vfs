(ns aether.director.proton.env
  "A Wine launch's environment (vfs-proton/src/launch/env.rs): Wine's own
  variables, the runtime's library paths, and exactly the VFS_* names the shim
  and the injector read in file-backed mode.

  Pure. A launch is a map:

    :runtime       the GE-Proton runtime dir (Linux path; made absolute)
    :prefix        the Wine prefix (WINEPREFIX)
    :ring-path     the ring file as Wine sees it (C:\\…)
    :ring-bytes :arena-offset :arena-len :payload-cap   the ring's geometry
    :virtual-dir   root 0 as Wine sees it (C:\\…)
    :virtual-roots the roots beyond 0: [[id C:\\…] …], or a map {id C:\\…}
    :registry?     a registry layer is served
    :cwd           the target's working directory as it sees it (C:\\…)
    :ready-timeout-secs  how long the injector waits for the shim
    :steam         nil (untouched), or proton.steam/steam-side's decision:
                   {:mode :untouched|:off|:helper :app-id n :client dir}
    :extra-env     the host's own variables for the child

  and the host is {:env {name value} :cwd dir}: this process's environment
  (cleaned of an AppImage's additions here, as hostenv.rs's var_os does) and
  working directory. Linux paths are strings throughout.

  The handshake names and tables are aether.director.env's; the Steam
  constants are proton.steam's. This is the one builder of the env."
  (:require [aether.director.env :as henv]
            [aether.director.proton.hostenv :as hostenv]
            [aether.director.proton.layout :as layout]
            [aether.director.proton.runtime :as runtime]
            [aether.director.proton.steam :as steam]
            [clojure.string :as str]))

(set! *warn-on-reflection* true)

(defn- ascii-lower
  "`s` with only A-Z lower-cased, as Rust's to_ascii_lowercase: Wine compares
  names ASCII case-insensitively, and the locale's rules would fold more (the
  Kelvin sign to k)."
  ^String [^String s]
  (let [cs (.toCharArray s)]
    (dotimes [i (alength cs)]
      (let [c (aget cs i)] (when (<= (int \A) (int c) (int \Z)) (aset cs i (char (+ (int c) 32))))))
    (String. cs)))

(def base-dll-overrides
  "WINEDLLOVERRIDES every launch carries: Mono's and Gecko's install prompts
  would otherwise block a launch on a fresh prefix (env.rs:12)."
  "mscoree=d;mshtml=d")

(def default-winedebug "WINEDEBUG unless the host sets its own (env.rs:15)." "-all")

(def wine-arch
  "WINEARCH every launch carries: Wine's WoW64 mode, which runs a 32-bit
  program's code on the runtime's 64-bit Unix side. Without it a 32-bit
  program (Oblivion, xEdit) runs on 32-bit Wine over the host's own i386
  libraries, which Steam's runtime container would replace and a direct
  launch can't: they may be missing (a Flatpak has none) or broken (Oblivion
  draws no menu over Mesa's i386 Vulkan driver), where WoW64 needs none.
  64-bit programs run the same either way."
  "wow64")

(def proton-graphics-overrides
  "What Proton itself sets to run a game on DXVK and vkd3d-proton, for a host
  launching in a Proton-made prefix that wants the same: a direct `wine` run
  doesn't apply Proton's per-launch overrides (PROTON_GRAPHICS_OVERRIDES,
  vfs-proton/src/prefix/init.rs:68-69)."
  "d3d11=n;d3d10core=n;d3d9=n;dxgi=n;d3d8=n;d3d12=n;d3d12core=n")

(def proxy-dll-names
  "DLL names a mod commonly ships as a proxy beside the game (PROXY_DLL_NAMES,
  vfs-director/src/stage.rs:110-145). Wine prefers its builtin for most of
  them, so a staged one loads only when WINEDLLOVERRIDES names it n,b (see
  Session::staged_proxies, vfs-embed/src/session/stage.rs:114-129)."
  ["d3d11.dll" "dxgi.dll" "dinput8.dll" "version.dll" "winmm.dll" "winhttp.dll" "dbghelp.dll"
   "xinput1_4.dll" "hid.dll" "dwmapi.dll" "uxtheme.dll" "wintrust.dll" "userenv.dll"
   "xinput1_3.dll" "x3daudio1_7.dll" "d3d8.dll" "d3d9.dll" "d3d10.dll" "d3d10_1.dll" "d3d10core.dll"
   "d3d12.dll" "ddraw.dll" "opengl32.dll" "dsound.dll" "dinput.dll" "xinput1_1.dll" "xinput1_2.dll"
   "xinput9_1_0.dll" "wininet.dll" "d3dcompiler_46e.dll"])

(defn reserved?
  "Whether `name` is one a launch sets or clears itself, and so one extra-env
  may not: WINEPREFIX, PROTONPATH and every handshake name. ASCII
  case-insensitive, because Wine's environment is: vfs_virtual_dir reaches
  the shim as VFS_VIRTUAL_DIR (is_reserved_env, env.rs:191-196)."
  [^String name]
  ;; ASCII folding, not equalsIgnoreCase (henv/handshake?), which would also
  ;; fold the Kelvin sign to k
  (let [n (ascii-lower name)]
    (boolean (some #(= n (ascii-lower %)) (list* "WINEPREFIX" "PROTONPATH" (henv/handshake-names))))))

(defn check-extra-env
  "nil, or an ex-info (::error :reserved-env, :name) for the first reserved
  name in `extra-env` (check_extra_env, env.rs:199-204)."
  [extra-env]
  (when-let [k (first (filter reserved? (sort (keys extra-env))))]
    (throw (ex-info (str k " is set by the launch itself") {::error :reserved-env :name k}))))

(defn merge-dll-overrides
  "`base` and `extra` as one WINEDLLOVERRIDES value, extra winning per DLL.
  Entries are ;-separated names=mode, where names may list DLLs with , and
  mode may itself hold a , (n,b); each DLL becomes its own dll=mode entry, in
  first-seen order, matched ASCII case-insensitively. An entry without = is
  kept verbatim. Wine gives no order between two entries for one DLL, so
  exactly one is left (merge_dll_overrides, env.rs:213-238)."
  [base extra]
  (let [put     (fn [[order m] name entry]
                  (let [k (ascii-lower name)]
                    [(if (contains? m k) order (conj order k)) (assoc m k entry)]))
        entries (for [src [base extra]
                      e (map str/trim (str/split (str src) #";"))
                      :when (seq e)
                      :let [[names mode] (str/split e #"=" 2)]
                      name (if mode (remove empty? (map str/trim (str/split names #","))) [e])]
                  [name (if mode (str name "=" (str/trim mode)) e)])
        [order m] (reduce (fn [acc [name entry]] (put acc name entry)) [[] {}] entries)]
    (str/join ";" (map m order))))

(defn proxy-overrides
  "WINEDLLOVERRIDES entries that make each of `dlls` (file names, as staged:
  dinput8.dll) load native first, n,b: the builtin otherwise wins under Wine
  (vfs-embed/src/session/stage.rs:114-122). A host merges this into its
  extra-env's WINEDLLOVERRIDES."
  [dlls]
  (str/join ";" (for [d dlls] (str (str/replace (ascii-lower d) #"\.dll$" "") "=n,b"))))

(defn absolute
  "`p`, a Linux path, made absolute against `cwd` (unchanged if already, or
  with no cwd). Not canonicalized: the dir needn't exist yet, and resolving
  links would rename a runtime a user linked in (absolute,
  launch/mod.rs:506-514)."
  [^String p cwd]
  (if (or (.startsWith p "/") (nil? cwd)) p (layout/join cwd p)))

(defn- caller-then-host
  "A search path's inherited part: extra-env's value in front of the host's,
  either alone when the other is unset or empty (caller_then_host,
  env.rs:172-182)."
  [extra host-env name]
  (let [x (get extra name) h (hostenv/inherited host-env name)]
    (cond (and x (seq h)) (str x ":" h)
          x x
          :else h)))

(defn launch-env
  "The child's environment for `launch` on `host` (see the ns doc), as a sorted
  map (launch_env, env.rs:54-168). With no host :cwd, relative paths are made
  absolute against this process's (absolute, launch/mod.rs:506-514).

  - WINEPREFIX; PROTONPATH, absolute, because a relative one silently means
    stock UMU-Proton; runtime-lib-env, with extra-env's LD_LIBRARY_PATH and
    WINEDLLPATH after the runtime's dirs and in front of the host's;
    WINEDLLOVERRIDES (base-dll-overrides [+ steam.exe=b], merged per DLL with
    extra-env's), WINEDEBUG, and WINEARCH (wine-arch).
  - The shim's VFS_RING_PATH (C: form), VFS_RING_BYTES, VFS_RING_PAYLOAD_CAP,
    VFS_ARENA_OFFSET, VFS_ARENA_LEN, VFS_VIRTUAL_DIR, VFS_VIRTUAL_ROOTS (when
    there are roots) and VFS_REGISTRY=1 (with a registry). Deliberately never
    VFS_RING_SECTION, VFS_SERVER_EV or VFS_CLIENT_EV: there is no section, and a
    Wine event can't wake a Linux director.
  - The injector's VFS_INJECT_CWD and VFS_READY_TIMEOUT_SECS (at least 1, and
    over extra-env's).
  - Steam, by `:steam`'s :mode. :helper: SteamAppId, SteamGameId,
    STEAM_COMPAT_CLIENT_INSTALL_PATH (absolute) and VFS_INJECT_STEAM_HELPER;
    :off: VFS_INJECT_STEAM_HELPER=off only; :untouched or nil: nothing.
  - extra-env over all that, apart from reserved names (check-extra-env
    refuses those before a spawn; here they are skipped).

  What the launch leaves out, it clears: (henv/stale env) names the
  inherited transport and inject names to remove. That includes
  VFS_REGISTRY, so a host that has it set can't turn the registry hooks on
  for a launch with no registry (stale_env, env.rs:245-247).

  NVAPI's additions (env.rs:82-93) are not here yet: spec task 14."
  [{:keys [runtime prefix ring-path ring-bytes arena-offset arena-len payload-cap virtual-dir
           virtual-roots registry? cwd ready-timeout-secs steam extra-env]}
   {host-env :env host-cwd :cwd}]
  (let [host-env  (or host-env {})
        host-cwd  (or host-cwd (System/getProperty "user.dir"))
        rt        (absolute runtime host-cwd)
        mode      (:mode steam)
        overrides (cond-> base-dll-overrides (= :helper mode) (merge-dll-overrides steam/helper-override))
        roots     (henv/encode-roots virtual-roots)]
    (as-> (sorted-map "WINEPREFIX"           (str prefix)
                      "PROTONPATH"           rt
                      "WINEDLLOVERRIDES"     overrides
                      "WINEDEBUG"            default-winedebug
                      "WINEARCH"             wine-arch
                      henv/ring-path         (str ring-path)
                      henv/ring-bytes        (str ring-bytes)
                      henv/ring-payload-cap  (str payload-cap)
                      henv/arena-offset      (str arena-offset)
                      henv/arena-len         (str arena-len)
                      henv/virtual-dir       (str virtual-dir)) env
      (merge env (runtime/runtime-lib-env rt
                                          (caller-then-host extra-env host-env "LD_LIBRARY_PATH")
                                          (hostenv/inherited host-env "ORIG_LD_LIBRARY_PATH")
                                          (caller-then-host extra-env host-env "WINEDLLPATH")))
      (cond-> env
        roots     (assoc henv/virtual-roots roots)
        registry? (assoc henv/registry "1")
        cwd       (assoc henv/inject-cwd (str cwd)))
      (case mode
        :off    (assoc env henv/inject-steam-helper "off")
        :helper (assoc env
                       "SteamAppId" (str (:app-id steam))
                       "SteamGameId" (str (:app-id steam))
                       "STEAM_COMPAT_CLIENT_INSTALL_PATH" (absolute (str (:client steam)) host-cwd)
                       henv/inject-steam-helper steam/helper-command)
        (:untouched nil) env)
      (reduce-kv (fn [env k v]
                   (cond (reserved? k) env
                         (#{"LD_LIBRARY_PATH" "WINEDLLPATH"} k) env
                         (= "WINEDLLOVERRIDES" k) (assoc env k (merge-dll-overrides overrides v))
                         :else (assoc env k v)))
                 env (or extra-env {}))
      (cond-> env
        ready-timeout-secs (assoc "VFS_READY_TIMEOUT_SECS" (str (max 1 (long ready-timeout-secs))))))))

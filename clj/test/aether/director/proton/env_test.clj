(ns aether.director.proton.env-test
  "vfs-proton/src/launch/env.rs's tests (env.rs:249-610): a Wine launch's
  environment, built from plain data."
  (:require [aether.director.env :as henv]
            [aether.director.proton.env :as env]
            [aether.director.proton.steam :as steam]
            [clojure.string :as str]
            [clojure.test :refer [deftest is testing]]))

(defn- sample
  "launch/mod.rs:574-597's sample(), Linux-shaped."
  []
  {:runtime      "/ge/GE-Proton11-6-x86_64"
   :prefix       "/ge/probe-prefix"
   :ring-path    "C:\\probe\\ring.bin"
   :ring-bytes   33751040
   :arena-offset 65536
   :arena-len    33554432
   :payload-cap  1048576
   :virtual-dir  "C:\\probe\\managed"})

(def ^:private host {:env {} :cwd "/home/me"})

(defn- launch-env
  ([l] (env/launch-env l host))
  ([l h] (env/launch-env l h)))

(defn- lib [l sub] (str (:runtime l) "/files/lib/" sub))

(deftest launch-env-puts-the-runtime-libs-first-on-ld-library-path
  (let [l (sample) e (launch-env l)]
    (is (str/starts-with? (e "LD_LIBRARY_PATH") (str (lib l "x86_64-linux-gnu") ":" (lib l "i386-linux-gnu"))))
    (is (str/starts-with? (e "WINEDLLPATH") (str (lib l "vkd3d") ":" (lib l "wine"))))
    (is (contains? e "ORIG_LD_LIBRARY_PATH"))
    (is (not (contains? (launch-env l {:env {"ORIG_LD_LIBRARY_PATH" "/o"}}) "ORIG_LD_LIBRARY_PATH"))
        "the host's own is left to be inherited")))

(deftest an-extra-env-ld-library-path-goes-after-the-runtime-dirs
  (let [l      (assoc (sample) :extra-env {"LD_LIBRARY_PATH" "/mine/lib"})
        rt     (str (lib l "x86_64-linux-gnu") ":" (lib l "i386-linux-gnu"))]
    (is (= (str rt ":/mine/lib") ((launch-env l) "LD_LIBRARY_PATH")))
    (is (= (str rt ":/mine/lib:/host/lib")
           ((launch-env l {:env {"LD_LIBRARY_PATH" "/host/lib"}}) "LD_LIBRARY_PATH"))
        "in front of the host's")
    (is (= "/mine/lib:/host/lib" ((launch-env l {:env {"LD_LIBRARY_PATH" "/host/lib"}}) "ORIG_LD_LIBRARY_PATH"))
        "ORIG_ is the inherited part, as env.rs:69-74 passes it")
    (is (= (str rt ":/host/lib") ((launch-env (sample) {:env {"LD_LIBRARY_PATH" "/host/lib"}}) "LD_LIBRARY_PATH")))))

(deftest the-hosts-search-paths-lose-an-appimages-entries
  (let [h {:env {"APPDIR" "/tmp/.mount_X" "LD_LIBRARY_PATH" "/tmp/.mount_X/usr/lib:/usr/lib"}}]
    (is (str/ends-with? ((launch-env (sample) h) "LD_LIBRARY_PATH") "i386-linux-gnu:/usr/lib"))))

(deftest extra-roots-travel-in-the-windows-format
  (let [l (assoc (sample) :virtual-roots [[1 "C:\\users\\steamuser\\Saves"] [2 "C:\\x"]])]
    (is (= "1=C:\\users\\steamuser\\Saves;2=C:\\x" ((launch-env l) "VFS_VIRTUAL_ROOTS"))))
  (is (= "1=C:\\a;2=C:\\b" ((launch-env (assoc (sample) :virtual-roots {2 "C:\\b" 1 "C:\\a"})) "VFS_VIRTUAL_ROOTS"))
      "a map goes in id order")
  (is (= "2=C:\\b;1=C:\\a" ((launch-env (assoc (sample) :virtual-roots [[2 "C:\\b"] [1 "C:\\a"]])) "VFS_VIRTUAL_ROOTS"))
      "pairs keep their order (henv/encode-roots, as Rust's)"))

(deftest an-inherited-registry-flag-is-cleared-unless-the-launch-sets-it
  (is (some #{"VFS_REGISTRY"} (henv/stale (launch-env (sample)))))
  (is (not (some #{"VFS_REGISTRY"} (henv/stale (launch-env (assoc (sample) :registry? true)))))))

(deftest every-transport-name-the-launch-does-not-set-is-cleared
  (let [e (launch-env (sample)) stale (set (henv/stale e))]
    (doseq [n (concat henv/transport henv/inject)]
      (is (not= (contains? e n) (contains? stale n)) (str n " must be exactly one of set or cleared")))
    (is (stale "VFS_FUSE_CFG") "VFS_FUSE_CFG was missing from the old list")))

(deftest registry-flag-is-set-only-for-a-registry-launch
  (is (not (contains? (launch-env (sample)) "VFS_REGISTRY")))
  (is (= "1" ((launch-env (assoc (sample) :registry? true)) "VFS_REGISTRY"))))

(deftest no-extra-roots-means-no-root-map
  (is (not (contains? (launch-env (sample)) "VFS_VIRTUAL_ROOTS"))))

(deftest env-carries-the-real-ring-size-not-a-default
  (let [e (launch-env (sample))]
    (is (= "33751040" (e "VFS_RING_BYTES")))
    (is (= "33554432" (e "VFS_ARENA_LEN")))
    (is (= "65536" (e "VFS_ARENA_OFFSET")))
    (is (= "1048576" (e "VFS_RING_PAYLOAD_CAP")))))

(deftest protonpath-is-absolute-because-a-relative-one-silently-means-stock
  (is (= "/ge/GE-Proton11-6-x86_64" ((launch-env (sample)) "PROTONPATH"))))

(deftest a-relative-runtime-is-absolutized-rather-than-passed-through
  (let [l (assoc (sample) :runtime "runtimes/GE-Proton11-6-x86_64")
        e (launch-env l)]
    (is (= "/home/me/runtimes/GE-Proton11-6-x86_64" (e "PROTONPATH")))
    (is (str/starts-with? (e "LD_LIBRARY_PATH") "/home/me/runtimes/GE-Proton11-6-x86_64/files/lib/"))
    (is (= "/runtimes/x" (env/absolute "runtimes/x" "/")))
    (is (= (str (System/getProperty "user.dir") "/runtimes/x")
           ((launch-env (assoc l :runtime "runtimes/x") {:env {}}) "PROTONPATH"))
        "with no host cwd, this process's, as launch/mod.rs:506-514")
    (is (= "/a/b" (env/absolute "/a/b" "/home/me")))))

(deftest the-ring-is-named-by-its-wine-path-and-no-section-is-offered
  (let [e (launch-env (sample))]
    (is (= "C:\\probe\\ring.bin" (e "VFS_RING_PATH")) "the shim maps the ring inside Wine: the C: form")
    (doseq [n ["VFS_RING_SECTION" "VFS_SERVER_EV" "VFS_CLIENT_EV"]]
      (is (not (contains? e n)) n))))

(deftest the-managed-root-is-always-set-because-the-shim-has-no-default-for-it
  (let [e (launch-env (sample))]
    (is (= "C:\\probe\\managed" (e "VFS_VIRTUAL_DIR")))
    (is (= "/ge/probe-prefix" (e "WINEPREFIX")))
    (is (= "mscoree=d;mshtml=d" (e "WINEDLLOVERRIDES")))
    (is (= "-all" (e "WINEDEBUG")))
    (is (= "wow64" (e "WINEARCH")) "32-bit programs run in WoW64 mode, on no host i386 libraries")
    (is (not (contains? e "VFS_INJECT_CWD")))
    (is (not (contains? e "VFS_READY_TIMEOUT_SECS")))))

(deftest the-cwd-and-ready-timeout-reach-the-injector
  (let [e (launch-env (assoc (sample) :cwd "C:\\probe" :ready-timeout-secs 0
                             :extra-env {"VFS_READY_TIMEOUT_SECS" "9"}))]
    (is (= "C:\\probe" (e "VFS_INJECT_CWD")))
    (is (= "1" (e "VFS_READY_TIMEOUT_SECS")) "at least 1, and the launch's beats extra-env's")))

(deftest dll-overrides-merge-with-the-caller-winning-per-dll
  (is (= "mscoree=d;mshtml=d" (env/merge-dll-overrides env/base-dll-overrides "")))
  (is (= "mscoree=d;mshtml=d;d3dx9_42=n,b" (env/merge-dll-overrides env/base-dll-overrides "d3dx9_42=n,b"))
      "a mode with a comma is one mode")
  (is (= "mscoree=d;MSHTML=n;dxgi=n;d3d11=n"
         (env/merge-dll-overrides env/base-dll-overrides "MSHTML=n;dxgi,d3d11=n; ;"))
      "the caller's entry replaces ours in place; a group splits per DLL")
  (is (= "a=d;winemenubuilder" (env/merge-dll-overrides "a=d" "winemenubuilder")))
  (is (= "k=d;K=n" (env/merge-dll-overrides "k=d" "K=n"))
      "ASCII case folding only, as eq_ignore_ascii_case"))

(deftest extra-env-reaches-the-child-env-with-overrides-merged
  (let [e (launch-env (assoc (sample) :extra-env {"WINEDLLOVERRIDES" "d3dx9_42=n,b"
                                                  "WINEDEBUG"        "+loaddll"
                                                  "SteamAppId"       "489830"}))]
    (is (= "mscoree=d;mshtml=d;d3dx9_42=n,b" (e "WINEDLLOVERRIDES")))
    (is (= "+loaddll" (e "WINEDEBUG")))
    (is (= "489830" (e "SteamAppId")))))

(deftest reserved-names-are-refused-case-insensitively
  (doseq [n ["WINEPREFIX" "protonpath" "VFS_VIRTUAL_DIR" "vfs_virtual_dir" "VFS_REGISTRY"
             "VFS_INJECT_STEAM_HELPER" "VFS_FORCE_INJECTION" "VFS_SHIM_READY"]]
    (is (env/reserved? n) n))
  (doseq [n ["WINEDLLOVERRIDES" "WINEDEBUG" "SteamAppId" "VFS_BENCH" "LD_LIBRARY_PATH"
             "VFS_ROOT_BACKINGS"]]
    (is (not (env/reserved? n)) n))
  (is (nil? (env/check-extra-env {"WINEDEBUG" "-all"})))
  (let [e (try (env/check-extra-env {"WINEDEBUG" "" "vfs_ring_path" "x"}) nil
               (catch clojure.lang.ExceptionInfo e (ex-data e)))]
    (is (= :reserved-env (::env/error e)))
    (is (= "vfs_ring_path" (:name e))))
  (is (= "/ge/probe-prefix" ((launch-env (assoc (sample) :extra-env {"WINEPREFIX" "/evil"})) "WINEPREFIX"))
      "launch-env never lets one through, though check-extra-env is what refuses it"))

(defn- steam-sample []
  (assoc (sample) :prefix "/ge/compat/pfx" :steam {:mode :helper :app-id 489830 :client "/ge/Steam"}))

(deftest a-steam-launch-carries-the-helper-and-what-steams-launcher-sets
  (let [e (launch-env (steam-sample))]
    (doseq [n ["SteamAppId" "SteamGameId"]] (is (= "489830" (e n)) n))
    (is (= "/ge/Steam" (e "STEAM_COMPAT_CLIENT_INSTALL_PATH")))
    (is (= "C:\\windows\\system32\\steam.exe C:\\windows\\system32\\rundll32.exe" (e "VFS_INJECT_STEAM_HELPER"))
        "the helper sets nothing up unless it is given a program to run")
    (is (= "mscoree=d;mshtml=d;steam.exe=b" (e "WINEDLLOVERRIDES")))
    (doseq [n ["SteamClientLaunch" "SteamEnv" "STEAM_COMPAT_APP_ID" "STEAM_COMPAT_DATA_PATH"
               "SteamOverlayGameId" "SteamUser"]]
      (is (not (contains? e n)) n)))
  (is (= "/home/me/Steam" ((launch-env (assoc-in (steam-sample) [:steam :client] "Steam"))
                           "STEAM_COMPAT_CLIENT_INSTALL_PATH"))
      "absolutized"))

(deftest with-the-helper-off-the-injector-is-only-asked-to-clear-the-stale-pid
  (let [e (launch-env (assoc (sample) :steam {:mode :off :notes []}))]
    (is (= "off" (e "VFS_INJECT_STEAM_HELPER")))
    (is (= env/base-dll-overrides (e "WINEDLLOVERRIDES")))
    (doseq [n ["SteamAppId" "SteamGameId" "STEAM_COMPAT_CLIENT_INSTALL_PATH"]]
      (is (not (contains? e n)) n))))

(deftest without-steam-the-environment-is-what-it-was
  (let [e (launch-env (sample))]
    (doseq [n ["SteamAppId" "SteamGameId" "STEAM_COMPAT_APP_ID" "STEAM_COMPAT_CLIENT_INSTALL_PATH"
               "STEAM_COMPAT_DATA_PATH" "VFS_INJECT_STEAM_HELPER"]]
      (is (not (contains? e n)) n))
    (is (= env/base-dll-overrides (e "WINEDLLOVERRIDES")))))

(deftest the-hosts-steam-names-and-overrides-win-over-the-steam-launchs
  (let [l (assoc (steam-sample) :extra-env {"SteamGameId" "12345" "WINEDLLOVERRIDES" "dxgi=n"})
        e (launch-env l)]
    (is (= "12345" (e "SteamGameId")))
    (is (= "489830" (e "SteamAppId")))
    (is (= "mscoree=d;mshtml=d;steam.exe=b;dxgi=n" (e "WINEDLLOVERRIDES")))
    (is (= "mscoree=d;mshtml=d;steam.exe=n"
           ((launch-env (assoc-in l [:extra-env "WINEDLLOVERRIDES"] "steam.exe=n")) "WINEDLLOVERRIDES"))))
  (is (env/reserved? "VFS_INJECT_STEAM_HELPER") "only the launch asks for the helper"))

(deftest proxy-dlls-load-native-first
  (testing "Wine prefers its builtin dinput8, version, d3d11…: a mod's proxy loads only with n,b"
    (is (= "dinput8=n,b;version=n,b" (env/proxy-overrides ["dinput8.dll" "VERSION.DLL"])))
    (is (= "" (env/proxy-overrides [])))
    (is (= "mscoree=d;mshtml=d;dxgi=n,b"
           ((launch-env (assoc (sample) :extra-env {"WINEDLLOVERRIDES" (env/proxy-overrides ["dxgi.dll"])}))
            "WINEDLLOVERRIDES"))))
  (is (some #{"dinput8.dll"} env/proxy-dll-names))
  (is (= 30 (count env/proxy-dll-names)) "vfs-director/src/stage.rs:110-145"))

(deftest the-dxvk-overrides-are-protons-own
  (is (= "d3d11=n;d3d10core=n;d3d9=n;dxgi=n;d3d8=n;d3d12=n;d3d12core=n" env/proton-graphics-overrides)))

(deftest steam-sides-decision-is-what-the-env-is-built-from
  (let [side (steam/steam-side {:prefix-init {:proton {:steam-client "/ge/Steam" :app-id 489830}}
                                :state-dir "/home/u/.steam" :client-pid 7})
        e    (launch-env (assoc (sample) :steam side))]
    (is (= "489830" (e "SteamAppId")))
    (is (= steam/helper-command (e "VFS_INJECT_STEAM_HELPER")))
    (is (= "mscoree=d;mshtml=d;steam.exe=b" (e "WINEDLLOVERRIDES"))))
  (is (= (launch-env (sample)) (launch-env (assoc (sample) :steam {:mode :untouched :notes []})))
      "untouched is no Steam at all"))

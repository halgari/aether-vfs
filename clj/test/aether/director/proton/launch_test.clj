(ns aether.director.proton.launch-test
  "aether.director.proton.launch, the pure parts, ported from vfs-proton's
  launch/{mod,injector}.rs tests."
  (:require [aether.director.proton.launch :as launch]
            [clojure.string :as str]
            [clojure.test :refer [deftest is testing]]))

(def ^:private sample
  {:runtime      "/ge/GE-Proton11-6-x86_64"
   :injector     "/ge/bin/vfs-injector.exe"
   :shim-dll     "/ge/bin/vfs_shim_dll.dll"
   :target       "C:\\probe\\target.exe"
   :config-file  "/ge/state/shim.cfg"
   :ready-file   "/ge/state/ready.txt"
   :ring-bytes   33751040
   :arena-offset 65536
   :arena-len    33554432
   :args         ["-arg1" "arg2"]})

(defn- refusal [f]
  (try (f) nil (catch clojure.lang.ExceptionInfo e (assoc (ex-data e) :message (ex-message e)))))

(deftest wine-comes-from-inside-the-runtime-not-the-host-path
  (is (= "/ge/GE-Proton11-6-x86_64/files/bin/wine" (launch/wine-binary "/ge/GE-Proton11-6-x86_64/")))
  (is (= "/ge/GE-Proton11-6-x86_64/files/bin/wineserver" (launch/wineserver-binary "/ge/GE-Proton11-6-x86_64"))))

(deftest argv-is-the-injector-contract-in-order
  ;; vfs-injector <target_exe> <shim_dll> <config> <ready> [-- args]: positional,
  ;; so a swap is silent and this is the only thing that catches it.
  (let [[prog argv] (launch/command-line sample)]
    (is (= "/ge/GE-Proton11-6-x86_64/files/bin/wine" prog))
    (is (= ["/ge/bin/vfs-injector.exe" "C:\\probe\\target.exe" "/ge/bin/vfs_shim_dll.dll"
            "/ge/state/shim.cfg" "/ge/state/ready.txt"]
           (subvec argv 0 5)))))

(deftest target-args-follow-a-separator-and-keep-their-order
  (is (= ["--" "-arg1" "arg2"] (subvec (second (launch/command-line sample)) 5))))

(deftest no-separator-is-emitted-when-there-are-no-target-args
  (is (= 5 (count (second (launch/command-line (assoc sample :args []))))))
  (is (= 5 (count (second (launch/command-line (dissoc sample :args)))))))

(deftest a-view-too-small-for-the-arena-is-refused-before-spawning
  (let [r (refusal #(launch/check-geometry (assoc sample :ring-bytes (* 2 1024 1024) :arena-offset 132136
                                                  :arena-len 33554432)
                                           nil))]
    (is (= :geometry (::launch/error r)))
    (is (str/includes? (:message r) "exceeds ring_bytes") (:message r))))

(deftest a-consistent-geometry-passes
  (is (nil? (launch/check-geometry (assoc sample :ring-bytes 33751040 :arena-offset 132136 :arena-len 33554432) nil)))
  (is (nil? (launch/check-geometry (assoc sample :ring-host-path "/s/ring.bin") 33751040)) "a file exactly long enough"))

(deftest a-ring-file-shorter-than-ring-bytes-is-refused
  ;; mapping past a file's end faults on touch rather than failing at map time
  (let [r (refusal #(launch/check-geometry (assoc sample :ring-host-path "/s/ring.bin" :ring-bytes 65536
                                                  :arena-offset 0 :arena-len 0)
                                           128))]
    (is (= :geometry (::launch/error r)))
    (is (str/includes? (:message r) "but ring_bytes is") (:message r))
    (is (str/includes? (:message r) "/s/ring.bin"))))

(deftest the-launchs-reports-sit-beside-the-ready-file
  (is (= "/s/ready.flag.injector-error" (launch/injector-error-path "/s/ready.flag")))
  (is (= "/s/ready.flag.child-refused" (launch/child-refused-path "/s/ready.flag")))
  (is (= ["/s/ready.flag.injector-error" "/s/ready.flag.steam-helper"] (launch/stale-reports "/s/ready.flag"))
      "a report an earlier launch left would be read as this one's"))

(deftest injector-reports-are-described
  (let [dll (launch/describe-injector-error "target-exited:0xc0000135\n")]
    (is (and (str/includes? dll "0xc0000135") (str/includes? dll "STATUS_DLL_NOT_FOUND")) dll))
  (is (str/includes? (launch/describe-injector-error "target-exited:0xC000007B") "STATUS_INVALID_IMAGE_FORMAT"))
  (is (str/includes? (launch/describe-injector-error "target-exited:0xc0000142") "STATUS_DLL_INIT_FAILED"))
  (let [other (launch/describe-injector-error "target-exited:0x1")]
    (is (and (str/includes? other "0x1") (str/includes? other "before the shim reported ready")
             (not (str/includes? other "STATUS_"))) other))
  (is (str/includes? (launch/describe-injector-error "ready-timeout:180") "180 s"))
  (let [f (launch/describe-injector-error "fuse-failed:no ring")]
    (is (and (str/includes? f "attach to the director") (str/includes? f "no ring")) f))
  (let [b (launch/describe-injector-error "bootstrap-failed:shim config version 3")]
    (is (and (str/includes? b "bootstrap") (str/includes? b "version 3")) b))
  (is (= "injection failed: CreateProcess" (launch/describe-injector-error "inject:CreateProcess")))
  (is (= "injection failed: something new" (launch/describe-injector-error "something new"))))

(deftest child-refusals-are-lines-and-absent-means-none
  (is (= [] (launch/child-refusals nil)))
  (is (= ["C:\\a b\\x.exe ready-timeout" "y.exe child-32bit"]
         (launch/child-refusals "C:\\a b\\x.exe ready-timeout\n\ny.exe child-32bit\n")))
  (is (str/includes? (launch/describe-child-refusal "y.exe child-32bit") "refused and killed")))

(deftest finish-maps-exits-and-reads-the-injector-report
  (is (= {:exit 0} (launch/finish sample 0 nil)))
  (is (= {:exit 7} (launch/finish sample 7 "target-exited:0x1")) "a target's own code is its exit, report or not")
  (testing "2 and 3 are the injector's own: the target never ran"
    (doseq [code [2 3]]
      (let [r (launch/finish sample code nil)]
        (is (= [:injector-exit code] [(::launch/error r) (:code r)]))
        (is (str/includes? (:error r) (str "vfs-injector exited " code " without running the target")) (:error r))))
    (is (= :injector-exit (::launch/error (launch/finish sample 3 " \n"))) "an empty report says nothing")
    (let [r (launch/finish sample 3 "target-exited:0xc0000135")]
      (is (= [:injector 3] [(::launch/error r) (:code r)]))
      (is (str/starts-with? (:error r) "vfs-injector did not run the target: "))
      (is (str/includes? (:error r) "STATUS_DLL_NOT_FOUND") (:error r))))
  (testing "no code at all: signalled"
    (let [r (launch/finish sample nil nil)]
      (is (= :signalled (::launch/error r)))
      (is (str/includes? (:error r) "/ge/GE-Proton11-6-x86_64/files/bin/wine")))))


;; --- the Linux launch, composed (spec task 12) --------------------------------

(def ^:private session
  ;; what launch/session's Linux method returns, Linux-shaped
  {:dir "/home/me/s" :root "/home/me/s/root" :state "/home/me/s/state"
   :shim-config "/home/me/s/state/shim.cfg" :ready "/home/me/s/state/ready.flag"
   :root-location "C:\\vfs-session\\root"
   :roots {2 "C:\\users\\steamuser\\Documents\\My Games\\Skyrim" 1 "C:\\Saves"}
   :server {:transport :file :ring-path "C:\\vfs-session\\state\\ring.bin" :host-path "/home/me/s/state/ring.bin"
            :map-bytes 67174400 :cap 1048576 :arena-offset 33555504 :arena-len 33554432 :registry? true}})

(def ^:private proton
  {:runtime "/data/aev/runtimes/GE-Proton11-6-x86_64" :prefix "/data/aev/sessions/cairn-7/compat/pfx"
   :steam {:mode :helper :app-id 489830 :client "/home/me/.local/share/Steam" :notes []}})

(def ^:private host {:env {"PATH" "/usr/bin" "VFS_RING_SECTION" "Local\\old"} :cwd "/home/me"})

(def ^:private opts
  {:natives "/opt/cairn/natives" :exe "/home/me/s/stage/SkyrimSE.exe" :launch-image "/home/me/s/stage/SkyrimSE.exe"
   :args ["-forcesteamloader"] :env {"WINEDEBUG" "+loaddll"} :remove-env ["SteamUser"]})

(deftest wine-names-the-session-through-its-link-and-anything-else-through-z
  (is (= "C:\\vfs-session\\stage\\a\\b.exe" (launch/wine-path "/home/me/s" "/home/me/s/stage/a/b.exe")))
  (is (= "C:\\vfs-session" (launch/wine-path "/home/me/s/" "/home/me/s")))
  (is (= "Z:\\opt\\game\\g.exe" (launch/wine-path "/home/me/s" "/opt/game/g.exe")))
  (is (= "Z:\\home\\me\\s2\\x.exe" (launch/wine-path "/home/me/s" "/home/me/s2/x.exe")) "a sibling spelled alike")
  (is (= :not-absolute (::launch/error (refusal #(launch/wine-path "/home/me/s" "rel/x.exe"))))))

(deftest the-linux-plan-spawns-wine-with-the-injector-contract
  (let [{:keys [program argv]} (launch/plan session opts proton host)]
    (is (= "/data/aev/runtimes/GE-Proton11-6-x86_64/files/bin/wine" program))
    (is (= ["/opt/cairn/natives/vfs-injector.exe" "C:\\vfs-session\\stage\\SkyrimSE.exe"
            "/opt/cairn/natives/vfs_shim_dll.dll" "/home/me/s/state/shim.cfg" "/home/me/s/state/ready.flag"
            "--" "-forcesteamloader"]
           argv)
        "the target in C: form, the injector, DLL, config and ready as host paths")))

(deftest a-nested-exe-is-the-target-and-runs-in-its-own-folder
  ;; run/launch! names the launch image at the stage folder's top, whatever
  ;; folder the exe is staged in: the shim aliases the image's folder to root 0
  (let [nested (assoc opts :exe "/home/me/s/stage/bin/x64/game.exe" :launch-image "/home/me/s/stage/game.exe")
        {:keys [argv env]} (launch/plan session nested proton host)
        e (into {} env)]
    (is (= "C:\\vfs-session\\stage\\bin\\x64\\game.exe" (second argv)) "the exe runs, not the image's name")
    (is (= "C:\\vfs-session\\stage\\bin\\x64" (e "VFS_INJECT_CWD"))
        "in its own folder, which exists on disk (root 0's spelling of it doesn't)")
    (is (= "C:\\vfs-session\\stage\\game.exe" (e "VFS_LAUNCH_IMAGE")))))

(deftest the-linux-plan-env-is-proton-envs-with-the-live-geometry
  (let [{:keys [env remove]} (launch/plan session opts proton host)
        e (into {} env)]
    (is (= {"WINEPREFIX" "/data/aev/sessions/cairn-7/compat/pfx"
            "PROTONPATH" "/data/aev/runtimes/GE-Proton11-6-x86_64"
            "VFS_RING_PATH" "C:\\vfs-session\\state\\ring.bin"
            "VFS_RING_BYTES" "67174400" "VFS_RING_PAYLOAD_CAP" "1048576"
            "VFS_ARENA_OFFSET" "33555504" "VFS_ARENA_LEN" "33554432"
            "VFS_VIRTUAL_DIR" "C:\\vfs-session\\root"
            "VFS_VIRTUAL_ROOTS" "1=C:\\Saves;2=C:\\users\\steamuser\\Documents\\My Games\\Skyrim"
            "VFS_REGISTRY" "1"
            "VFS_INJECT_CWD" "C:\\vfs-session\\stage"
            "VFS_INJECT_STEAM_HELPER" "C:\\windows\\system32\\steam.exe C:\\windows\\system32\\rundll32.exe"
            "SteamAppId" "489830" "SteamGameId" "489830"
            "STEAM_COMPAT_CLIENT_INSTALL_PATH" "/home/me/.local/share/Steam"
            "WINEDLLOVERRIDES" "mscoree=d;mshtml=d;steam.exe=b"
            "WINEDEBUG" "+loaddll"
            "WINEARCH" "wow64"
            "VFS_LAUNCH_IMAGE" "C:\\vfs-session\\stage\\SkyrimSE.exe"}
           (dissoc e "LD_LIBRARY_PATH" "WINEDLLPATH" "ORIG_LD_LIBRARY_PATH")))
    (is (str/starts-with? (e "LD_LIBRARY_PATH") "/data/aev/runtimes/GE-Proton11-6-x86_64/files/lib/"))
    (is (some #{"VFS_RING_SECTION"} remove) "an inherited section name is cleared")
    (is (some #{"SteamUser"} remove) "and the caller's removals")
    (is (not-any? (set (keys e)) remove) "nothing set is removed")))

(deftest the-linux-plan-links-the-session-and-the-roots-outside-it
  (let [{:keys [session-link links]} (launch/plan session opts proton host)]
    (is (= {:link "/data/aev/sessions/cairn-7/compat/pfx/drive_c/vfs-session" :target "/home/me/s"} session-link))
    (is (= [[1 "C:\\Saves" "/home/me/s/roots/1" "/data/aev/sessions/cairn-7/compat/pfx/drive_c/Saves"]
            [2 "C:\\users\\steamuser\\Documents\\My Games\\Skyrim" "/home/me/s/roots/2"
             "/data/aev/sessions/cairn-7/compat/pfx/drive_c/users/steamuser/Documents/My Games/Skyrim"]]
           (mapv (juxt :id :location :backing :link) links))
        "root 0 lives inside the linked session dir; the others get their backing linked in, shallowest first"))
  (testing "a root 0 placed elsewhere is linked too, backed by the session's root dir"
    (let [{:keys [links]} (launch/plan (assoc session :root-location "C:\\Games\\Skyrim" :roots {}) opts proton host)]
      (is (= [[0 "/home/me/s/root"]] (mapv (juxt :id :backing) links))))))

(deftest the-linux-plan-carries-what-the-spawn-checks-and-cleans
  (let [p (launch/plan session opts proton host)]
    (is (= "/home/me/s/state/ring.bin" (:ring-host-path p)))
    (is (= {:ring-bytes 67174400 :arena-offset 33555504 :arena-len 33554432} (:geometry p)))
    (is (= ["/home/me/s/state/ready.flag.injector-error" "/home/me/s/state/ready.flag.steam-helper"] (:reports p)))
    (is (= [] (:notes p)))
    (is (= :helper (:steam-mode p)))))

(deftest the-linux-plan-refuses-what-a-wine-launch-cannot-carry
  (is (= :reserved-env (:aether.director.proton.env/error
                        (refusal #(launch/plan session (assoc opts :env {"WINEPREFIX" "/x"}) proton host)))))
  (is (= :ring-not-wine (::launch/error
                         (refusal #(launch/plan (assoc-in session [:server :ring-path] "/home/me/s/state/ring.bin")
                                                opts proton host))))
      "a ring named by its host path: ring/start was not given :shim-path")
  (is (= :ring-not-wine (::launch/error (refusal #(launch/plan (assoc-in session [:server :transport] :section)
                                                               opts proton host)))))
  (testing "a C: target is taken as given; cwd is wine-cwd's"
    (let [e (into {} (:env (launch/plan session (-> opts (dissoc :launch-image) (assoc :target "C:\\Games\\g.exe" :cwd "bin"))
                                        proton host)))]
      (is (= "C:\\vfs-session\\root\\bin" (e "VFS_INJECT_CWD")))
      (is (not (contains? e "VFS_LAUNCH_IMAGE"))))))

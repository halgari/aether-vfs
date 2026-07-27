(ns aether.vfs.proton-test
  (:require [clojure.test :refer [deftest is]]
            [aether.vfs.os.linux.proton :as proton]))

(deftest proton-command-builds-the-invocation
  (let [cmd (proton/proton-command {:proton "/opt/proton"
                                    :mountpoint "/tmp/mnt"
                                    :exe "SkyrimSE.exe"
                                    :steam-root "/home/u/.local/share/Steam"
                                    :app-id 489830
                                    :compat "/tmp/throwaway-compat"})]
    (is (= "/opt/proton" (:cmd cmd)))
    (is (= ["run" "/tmp/mnt/SkyrimSE.exe"] (:args cmd)))
    (is (= "/tmp/mnt" (:cwd cmd)))
    ;; the throwaway compat dir is used verbatim — never the real prefix
    (is (= "/tmp/throwaway-compat" (get-in cmd [:env "STEAM_COMPAT_DATA_PATH"])))
    (is (= "489830" (get-in cmd [:env "SteamAppId"])))
    (is (= "489830" (get-in cmd [:env "SteamGameId"])))))

(deftest wineserver-lives-beside-proton
  (is (= "/opt/GE-Proton10-34/files/bin/wineserver"
         (proton/wineserver-path "/opt/GE-Proton10-34/proton"))))

(ns aether.director.crossover-test
  "The macOS launch's bottle work that needs no CrossOver: its user folders,
  and reading Steam's sign-in from its log."
  (:require [aether.director.crossover :as cx]
            [clojure.java.io :as io]
            [clojure.test :refer [deftest is testing]])
  (:import (java.nio.file Files LinkOption)
           (java.nio.file.attribute FileAttribute)))

(defn- temp-dir ^java.io.File []
  (.toFile (Files/createTempDirectory "cx-test" (make-array FileAttribute 0))))

(defn- link! [^java.io.File at ^java.io.File target]
  (io/make-parents at)
  (Files/createSymbolicLink (.toPath at) (.toPath target) (make-array FileAttribute 0)))

(deftest the-bottles-user-folders-stay-in-the-bottle
  (let [prefix (temp-dir)
        mac (temp-dir)
        user (io/file prefix "drive_c" "users" "crossover")
        _ (.mkdirs (io/file mac "Documents"))
        _ (spit (io/file mac "Documents" "mine.txt") "keep me")
        _ (link! (io/file user "Documents") (io/file mac "Documents"))
        _ (link! (io/file user "Downloads") (io/file mac "Downloads"))
        _ (.mkdirs (io/file user "AppData"))
        _ (.mkdirs (io/file prefix "drive_c" "inside"))
        _ (link! (io/file user "Inside") (io/file prefix "drive_c" "inside"))
        replaced (cx/isolate-user-folders! prefix)]
    (is (= #{(str (io/file user "Documents")) (str (io/file user "Downloads"))} (set replaced)))
    (testing "now plain folders in the bottle"
      (is (.isDirectory (io/file user "Documents")))
      (is (not (Files/isSymbolicLink (.toPath (io/file user "Documents"))))))
    (is (= "keep me" (slurp (io/file mac "Documents" "mine.txt"))) "what the link pointed at is untouched")
    (is (Files/isSymbolicLink (.toPath (io/file user "Inside"))) "a link inside the bottle stays")
    (is (empty? (cx/isolate-user-folders! prefix)) "a second time, nothing to do")))

(deftest only-the-bottle-and-root-drives-stay
  (let [prefix (temp-dir)
        dd (io/file prefix "dosdevices")
        drive! (fn [n target] (link! (io/file dd n) (io/file target)))]
    (.mkdirs dd)
    (drive! "c:" (io/file prefix "drive_c"))
    (drive! "z:" (io/file "/"))
    (drive! "y:" (io/file (System/getProperty "user.home")))
    (drive! "d:" (io/file "/Volumes/Cairn"))
    (drive! "d::" (io/file "/dev/rdisk4s1"))
    (spit (io/file dd "com1") "a file, not a drive")
    (is (= #{"y:" "d:" "d::"} (set (cx/prune-drives! prefix))))
    (is (= #{"c:" "z:" "com1"} (set (.list dd))))))

(defn- steam-log! [prefix lines]
  (let [f (io/file prefix "drive_c" "Program Files (x86)" "Steam" "logs" "connection_log.txt")]
    (io/make-parents f)
    (spit f (apply str (map #(str % "\n") lines)))))

(defn- ms [ts]
  (-> (java.time.LocalDateTime/parse ts (java.time.format.DateTimeFormatter/ofPattern "yyyy-MM-dd HH:mm:ss"))
      (.atZone (java.time.ZoneId/systemDefault)) .toInstant .toEpochMilli))

(deftest steams-sign-in-from-its-log
  (let [prefix (temp-dir)]
    (is (not (cx/signed-in? prefix)) "no log, not signed in")
    (steam-log! prefix ["[2026-10-25 07:19:14] [Logged Off, 4, 0] [U:1:1] LogOn() called; not connected yet"
                        "[2026-10-25 07:19:15] [Logged On, 4, 7] [U:1:1] RecvMsgClientLogOnResponse() : processing complete"])
    (is (cx/signed-in? prefix))
    (testing "since a time: a sign-in before it is an earlier session's"
      (is (cx/signed-in? prefix (ms "2026-10-25 07:19:15")))
      (is (not (cx/signed-in? prefix (ms "2026-10-25 07:19:16")))))
    (steam-log! prefix ["[2026-10-25 07:19:15] [Logged On, 4, 7] [U:1:1] processing complete"
                        "[2026-10-25 08:00:00] [Logged Off, 4, 0] [U:1:1] Disconnected"])
    (is (not (cx/signed-in? prefix)) "the last state counts")))

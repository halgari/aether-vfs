//! `vfs_proton::steam`: whether a Steam client is running, from its pid file
//! and a process table.

use std::path::{Path, PathBuf};

use vfs_proton::steam::{not_running_note, running_client_in};

/// Scratch under Cargo's `CARGO_TARGET_TMPDIR`, not `/tmp`.
fn scratch(tag: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("vfs-proton-steam-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn proc_with(dir: &Path, pid: u32, comm: &str) -> PathBuf {
    let proc = dir.join("proc");
    std::fs::create_dir_all(proc.join(pid.to_string())).unwrap();
    std::fs::write(proc.join(pid.to_string()).join("comm"), format!("{comm}\n")).unwrap();
    proc
}

#[test]
fn a_pid_file_naming_a_live_steam_process_is_a_running_client() {
    let d = scratch("live");
    std::fs::write(d.join("steam.pid"), "4242\n").unwrap();
    let proc = proc_with(&d, 4242, "steam");
    assert_eq!(running_client_in(&d, &proc), Some(4242));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_stale_pid_file_is_not_a_running_client() {
    let d = scratch("stale");
    let proc = proc_with(&d, 1, "systemd");
    assert_eq!(running_client_in(&d, &proc), None, "no pid file");
    std::fs::write(d.join("steam.pid"), "4242").unwrap();
    assert_eq!(running_client_in(&d, &proc), None, "the process is gone");
    std::fs::write(d.join("steam.pid"), "1").unwrap();
    assert_eq!(
        running_client_in(&d, &proc),
        None,
        "the pid was reused by something else"
    );
    std::fs::write(d.join("steam.pid"), "not a pid").unwrap();
    assert_eq!(running_client_in(&d, &proc), None);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_note_is_one_line_naming_the_pid_file() {
    let n = not_running_note(Path::new("/home/u/.steam"));
    assert!(
        n.contains("/home/u/.steam/steam.pid") && n.contains("without Steam"),
        "{n}"
    );
    assert!(!n.contains('\n'));
}

mod helper_report {
    use super::scratch;
    use std::path::PathBuf;
    use vfs_proton::steam::{helper_note, helper_report_path, helper_status};
    use vfs_proton::{HelperStatus, SteamLaunch, SteamSide};

    fn helper() -> SteamSide {
        SteamSide::Helper(SteamLaunch {
            client: PathBuf::from("/s"),
            app_id: 489830,
        })
    }

    #[test]
    fn the_injectors_report_is_read_back() {
        let d = scratch("report");
        let ready = d.join("ready.flag");
        let report = helper_report_path(&ready);
        assert_eq!(report, d.join("ready.flag.steam-helper"));
        for (raw, want) in [
            (
                "started:236:317",
                HelperStatus::Started { pid: 236, ms: 317 },
            ),
            ("cleared", HelperStatus::Cleared),
            (
                "failed:it (process 9) exited before publishing itself",
                HelperStatus::NotRunning("it (process 9) exited before publishing itself".into()),
            ),
            (
                "disabled:SteamGameId is not set",
                HelperStatus::NotRunning("SteamGameId is not set".into()),
            ),
        ] {
            std::fs::write(&report, raw).unwrap();
            assert_eq!(helper_status(&helper(), &ready, false), want, "{raw}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_missing_report_is_pending_until_the_injector_is_past_it() {
        let d = scratch("missing");
        let ready = d.join("ready.flag");
        assert_eq!(
            helper_status(&helper(), &ready, false),
            HelperStatus::Pending
        );
        assert_eq!(
            helper_status(&helper(), &ready, true),
            HelperStatus::Unreported
        );
        std::fs::write(&ready, "ready").unwrap();
        assert_eq!(
            helper_status(&helper(), &ready, false),
            HelperStatus::Unreported,
            "the target is running, so the injector wrote whatever it was going to"
        );
        assert_eq!(
            helper_status(&SteamSide::Untouched, &ready, true),
            HelperStatus::NotRequested
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn only_a_helper_that_is_not_running_or_an_old_injector_is_noted() {
        assert_eq!(
            helper_note(&helper(), &HelperStatus::Started { pid: 1, ms: 1 }),
            None
        );
        assert_eq!(helper_note(&SteamSide::Off, &HelperStatus::Cleared), None);
        assert_eq!(helper_note(&helper(), &HelperStatus::Pending), None);
        let n = helper_note(&helper(), &HelperStatus::NotRunning("why".into())).unwrap();
        assert!(n.contains("(why)") && n.contains("without Steam"), "{n}");
        let n = helper_note(&helper(), &HelperStatus::Unreported).unwrap();
        assert!(n.contains("rebuild") && n.contains("without Steam"), "{n}");
        let n = helper_note(&SteamSide::Off, &HelperStatus::Unreported).unwrap();
        assert!(n.contains("rebuild") && !n.contains("without Steam"), "{n}");
    }
}

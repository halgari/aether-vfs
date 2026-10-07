//! The Proton launch-handle state machine: what a running launch is, how it
//! is stopped from another thread, and how it is reaped.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use vfs_proton::{
    launch::WineLaunch,
    layout::Root as ProtonRoot,
    prefix::{Prefix, PrefixLock},
    steam::SteamSide,
};

use crate::session::LaunchExit;
#[cfg(doc)]
use super::ProtonState;

/// An anonymous Wine prefix a session booted: its id under `home`'s
/// `sessions/`, and the runtime whose `wineserver` serves it.
#[cfg(unix)]
pub(super) struct AnonPrefix {
    pub(super) id: String,
    pub(super) home: ProtonRoot,
    pub(super) runtime: PathBuf,
    /// Where the prefix is under `home` for the session's `PrefixInit`.
    pub(super) prefix_dir: PathBuf,
}

/// A running Proton launch, from [`Session::launch_detached`].
///
/// **Liveness follows the prefix, not the first process.** A launch runs
/// until no Wine process is left in the session's own prefix — its
/// `wineserver` has exited — not merely until the `wine` process running the
/// injector's target exits. A launcher that starts the game and exits
/// (`skse64_loader.exe` starts `SkyrimSE.exe`) therefore stays running for as
/// long as the game does: [`LaunchHandle::try_wait`] and
/// [`LaunchHandle::is_running`] (non-blocking) and [`LaunchHandle::wait`]
/// (blocking) all see the prefix, and [`LaunchHandle::stop`] and
/// [`Session::stop_launch`] still stop it after the launcher is gone. The
/// exit code reported is the launcher's (the injector's target's);
/// [`LaunchExit::Stopped`] when a stop was requested. The prefix is locked to
/// this launch, so nothing else runs there; the prefix going quiet includes
/// `wineserver`'s few seconds of persistence after the last process.
///
/// Holds the prefix's lock until the launch ends. Dropping it while the
/// program runs stops the program.
#[cfg(unix)]
pub struct LaunchHandle {
    pub(super) child: std::process::Child,
    pub(super) wine: WineLaunch,
    pub(super) stopper: LaunchStopper,
    /// `child`'s exit status, once reaped.
    pub(super) wine_status: Option<std::process::ExitStatus>,
    /// `wineserver -w` for this prefix, started once `child` has exited: the
    /// launch runs until it returns.
    pub(super) quiet: Option<std::process::Child>,
    /// How the launch ended, once the prefix is quiet.
    pub(super) outcome: Option<Result<LaunchExit, String>>,
    pub(super) _prefix_lock: PrefixLock,
}

/// Stops a running Proton launch from any thread: see
/// [`LaunchHandle::stopper`].
#[cfg(unix)]
#[derive(Clone, Debug)]
pub struct LaunchStopper(pub(super) Arc<StopInner>);

#[cfg(unix)]
#[derive(Debug)]
pub(super) struct StopInner {
    pub(super) prefix: Prefix,
    pub(super) runtime: PathBuf,
    pub(super) requested: std::sync::atomic::AtomicBool,
    /// Set once this launch has been reaped — by [`LaunchHandle::conclude`]
    /// or its `Drop` — **before** `_prefix_lock` releases. A stopper kept
    /// past that point (the caller's own, or one handed out and forgotten)
    /// must not run `wineserver -k` on the prefix: once the lock is free, a
    /// later launch can be running there instead, and `wineserver -k` cannot
    /// tell the two apart. [`LaunchStopper::stop`]'s check-then-act and this
    /// flag share one mutex, so the two can never race past each other: if
    /// `stop` gets there first the launch is still this one's and the kill
    /// is real; if reaping gets there first `stop` sees `ended` and no-ops.
    pub(super) ended: Mutex<bool>,
}

/// How long [`LaunchHandle::stop`] waits for `wine` after stopping the
/// prefix's `wineserver` before killing it.
#[cfg(unix)]
const STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(unix)]
impl LaunchStopper {
    /// Whether the launch has ended — see [`StopInner::ended`].
    pub(super) fn has_ended(&self) -> bool {
        self.0.ended.lock().map(|e| *e).unwrap_or(true)
    }

    /// Stops the launch: stops its prefix's `wineserver` (bounded), which ends
    /// every Wine process in the prefix — the program, anything it started,
    /// and the injector. The prefix is locked to this launch, so nothing
    /// else is running there.
    ///
    /// A no-op, `Ok(())`, once the launch has already ended — see
    /// [`StopInner::ended`]. Without that check, a stopper kept past its
    /// launch's life (the fixed window `Session::launch` publishes one in
    /// `self.waiting` for, or simply a clone a caller held onto) could run
    /// `wineserver -k` against whatever the same prefix runs next.
    pub fn stop(&self) -> Result<(), String> {
        let ended = self
            .0
            .ended
            .lock()
            .map_err(|_| "stop: launch-ended lock poisoned".to_string())?;
        if *ended {
            return Ok(());
        }
        self.0.requested.store(true, std::sync::atomic::Ordering::SeqCst);
        let result = self
            .0
            .prefix
            .stop_wineserver(&self.0.runtime)
            .map_err(|e| format!("stop: {e}"));
        drop(ended);
        result
    }

    /// Whether [`LaunchStopper::stop`] has been called.
    pub fn was_stopped(&self) -> bool {
        self.0.requested.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Marks the launch ended — see [`StopInner::ended`]. Idempotent.
    fn mark_ended(&self) {
        if let Ok(mut ended) = self.0.ended.lock() {
            *ended = true;
        }
    }
}

/// Resets [`ProtonState::starting`] to `false` when a
/// [`Session::launch_detached`] call ends, on every path — success or an
/// early `?` return alike. Clears [`ProtonState::stop_pending`] the same way:
/// a `stop_launch` landing while this call is in flight but before it
/// reaches the spawn checkpoint (an early error — a bad image, a prefix
/// that won't `ensure`) would otherwise leave that flag set with nothing
/// left to consume it, and the *next*, unrelated `launch_detached` would
/// spawn its program only to stop it immediately.
#[cfg(unix)]
pub(super) struct StartingGuard<'a> {
    pub(super) starting: &'a std::sync::atomic::AtomicBool,
    pub(super) stop_pending: &'a std::sync::atomic::AtomicBool,
}

#[cfg(unix)]
impl Drop for StartingGuard<'_> {
    fn drop(&mut self) {
        self.starting.store(false, std::sync::atomic::Ordering::SeqCst);
        self.stop_pending.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(unix)]
impl LaunchHandle {
    /// The pid of the `wine` process running `vfs-injector.exe`, which lives
    /// as long as the injector's target does — not necessarily as long as the
    /// launch (see [`LaunchHandle`]). Not the program's own pid.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// What the launch has to say, one line each: what it said before it
    /// started (the Steam client is not running — also written at the top of
    /// [`LaunchOpts::log_file`], or to stderr without one), then, once the
    /// injector has reported, why the Steam helper is not running or that
    /// the Windows artifacts are too old to start it
    /// ([`LaunchHandle::steam_helper_status`]). Call it again after the
    /// program has started for the second part.
    pub fn notes(&self) -> Vec<String> {
        let mut notes = self.wine.notes.clone();
        notes.extend(vfs_proton::steam::helper_note(
            &self.wine.steam,
            &self.steam_helper_status(),
        ));
        notes
    }

    /// What became of Proton's Steam helper
    /// ([`Session::set_steam_helper`]): not asked for, still pending,
    /// started, cleared, not running and why, or not reported by an injector
    /// that predates it. Read from the injector's report beside the ready
    /// file; final once the program has started or `wine` has exited.
    pub fn steam_helper_status(&self) -> vfs_proton::HelperStatus {
        vfs_proton::steam::helper_status(
            &self.wine.steam,
            &self.wine.ready_file,
            self.wine_status.is_some() || self.outcome.is_some(),
        )
    }

    /// Whether the launch asked the injector to start Proton's Steam helper
    /// ([`Session::set_steam_helper`]); [`LaunchHandle::steam_helper_status`]
    /// says whether it did.
    pub fn steam_helper(&self) -> bool {
        matches!(self.wine.steam, SteamSide::Helper(_))
    }

    /// A stopper for this launch, to stop it from another thread while this
    /// handle is being waited on.
    pub fn stopper(&self) -> LaunchStopper {
        self.stopper.clone()
    }

    /// Whether the launch is still running: a Wine process is left in its
    /// prefix. Never blocks.
    pub fn is_running(&mut self) -> bool {
        matches!(self.poll(), Ok(None))
    }

    /// How the launch ended, if it has — see [`LaunchHandle`]. Never blocks.
    pub fn try_wait(&mut self) -> Result<Option<LaunchExit>, String> {
        self.poll()
    }

    /// Waits for the launch to end: for the prefix to be quiet, which is as
    /// long as the program (and anything it started) runs.
    pub fn wait(mut self) -> Result<LaunchExit, String> {
        self.block()
    }

    /// Stops the launch ([`LaunchStopper::stop`]) and waits for it to end,
    /// killing `wine` and the prefix watch if they outlive the prefix's
    /// `wineserver` by [`STOP_WAIT`].
    pub fn stop(mut self) -> Result<LaunchExit, String> {
        let stopped = self.stopper.stop();
        let deadline = std::time::Instant::now() + STOP_WAIT;
        let exit = loop {
            if let Some(exit) = self.poll().transpose() {
                break exit;
            }
            if std::time::Instant::now() >= deadline {
                self.abandon();
                break self.conclude();
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        stopped?;
        exit
    }

    /// One non-blocking step: reap `wine` if it has exited, then watch the
    /// prefix, and conclude once it is quiet.
    fn poll(&mut self) -> Result<Option<LaunchExit>, String> {
        if let Some(outcome) = &self.outcome {
            return outcome.clone().map(Some);
        }
        if self.wine_status.is_none() {
            match self.child.try_wait().map_err(|e| format!("launch: {e}"))? {
                Some(status) => self.wine_status = Some(status),
                None => return Ok(None),
            }
        }
        if !self.prefix_quiet(false)? {
            return Ok(None);
        }
        self.conclude().map(Some)
    }

    /// [`Self::poll`], blocking until the launch has ended.
    pub(super) fn block(&mut self) -> Result<LaunchExit, String> {
        if let Some(outcome) = &self.outcome {
            return outcome.clone();
        }
        if self.wine_status.is_none() {
            self.wine_status = Some(self.child.wait().map_err(|e| format!("launch: {e}"))?);
        }
        self.prefix_quiet(true)?;
        self.conclude()
    }

    /// Whether no Wine process is left in the prefix, once `wine` itself has
    /// exited: `wineserver -w` for it, started on first call, has returned
    /// (`block`: waited for). A watch that cannot be started counts as quiet
    /// — there is then nothing to observe the prefix with, and holding the
    /// launch open forever would be worse than ending it with `wine`.
    fn prefix_quiet(&mut self, block: bool) -> Result<bool, String> {
        if self.quiet.is_none() {
            match self.stopper.0.prefix.spawn_wineserver_wait(&self.stopper.0.runtime) {
                Ok(watch) => self.quiet = Some(watch),
                Err(_) => return Ok(true),
            }
        }
        let watch = self.quiet.as_mut().expect("started above");
        if block {
            watch.wait().map_err(|e| format!("launch: {e}"))?;
            return Ok(true);
        }
        Ok(watch.try_wait().map_err(|e| format!("launch: {e}"))?.is_some())
    }

    /// Kills and reaps whatever of the launch this handle still has a
    /// process for: `wine`, and the prefix watch.
    fn abandon(&mut self) {
        if self.wine_status.is_none() {
            let _ = self.child.kill();
            self.wine_status = self.child.wait().ok();
        }
        if let Some(watch) = &mut self.quiet {
            let _ = watch.kill();
            let _ = watch.wait();
        }
    }

    /// The prefix is quiet (or abandoned) and `wine` reaped: decide how the
    /// launch ended, record it, and mark it ended — see
    /// [`StopInner::ended`] — before `self` (and so `_prefix_lock`) can drop.
    fn conclude(&mut self) -> Result<LaunchExit, String> {
        let was_stopped = self.stopper.was_stopped();
        self.stopper.mark_ended();
        let outcome = if was_stopped {
            Ok(LaunchExit::Stopped)
        } else {
            match self.wine_status {
                Some(status) => vfs_proton::launch::finish(&self.wine, status)
                    .map(LaunchExit::Exited)
                    .map_err(|e| format!("launch: {e}")),
                None => Err("launch: wine could not be reaped".to_string()),
            }
        };
        self.outcome = Some(outcome.clone());
        outcome
    }
}

#[cfg(unix)]
impl Drop for LaunchHandle {
    /// A handle dropped while its launch runs — `wine`, or anything left in
    /// its prefix after `wine` exited — stops it: nothing could stop it
    /// afterwards, and the prefix lock it held is released here. Marks the
    /// launch ended either way (idempotent if [`Self::conclude`] already
    /// did), before that release — see [`StopInner::ended`].
    fn drop(&mut self) {
        if matches!(self.poll(), Ok(None)) {
            let _ = self.stopper.stop();
            self.abandon();
        }
        self.stopper.mark_ended();
    }
}

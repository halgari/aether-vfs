use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::broadcast;

use crate::error::SourceError;

/// Identifies one source operation (an archive open, a range read, a file
/// download) across its events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(pub u64);

/// Progress of source operations, for the CLI and the store's scheduler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceEvent {
    /// `total` is the number of bytes the job expects to receive, if known.
    Started {
        job: JobId,
        label: String,
        total: Option<u64>,
    },
    /// `bytes` more bytes arrived from the network (bytes fetched again after
    /// a retry are counted again).
    Progress {
        job: JobId,
        bytes: u64,
    },
    /// Attempt `attempt` failed with `error`; the next starts after `delay`.
    Retry {
        job: JobId,
        attempt: u32,
        delay: Duration,
        error: String,
    },
    Finished {
        job: JobId,
    },
    /// The job failed, or was dropped before finishing (`error` = "cancelled").
    Failed {
        job: JobId,
        error: String,
    },
    /// A Nexus API call found the account's allowance used up and waits
    /// `wait` for it to reset. Requests that need no API call (reads of
    /// archives whose download links are held) go on meanwhile.
    AllowanceWait {
        wait: Duration,
    },
    /// A wait reported by [`AllowanceWait`](Self::AllowanceWait) is over,
    /// however it ended.
    AllowanceWaitEnded,
}

/// A broadcast channel of [`SourceEvent`]s. Cloning shares the channel.
/// Sending never blocks; a receiver that falls behind sees `Lagged`.
#[derive(Debug, Clone)]
pub struct Events {
    tx: broadcast::Sender<SourceEvent>,
    next: Arc<AtomicU64>,
}

impl Events {
    pub fn new(capacity: usize) -> Events {
        let (tx, _) = broadcast::channel(capacity.max(1));
        Events {
            tx,
            next: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SourceEvent> {
        self.tx.subscribe()
    }

    fn send(&self, e: SourceEvent) {
        let _ = self.tx.send(e); // no receivers is fine
    }

    #[doc(hidden)]
    pub fn start(&self, label: impl Into<String>, total: Option<u64>) -> Job {
        let id = JobId(self.next.fetch_add(1, Ordering::Relaxed));
        self.send(SourceEvent::Started {
            job: id,
            label: label.into(),
            total,
        });
        Job {
            events: self.clone(),
            id,
            done: false,
        }
    }
}

/// An API call waiting for its allowance to reset, reported from creation
/// to drop ([`SourceEvent::AllowanceWait`]).
#[doc(hidden)]
pub struct AllowanceWait {
    events: Events,
}

impl Drop for AllowanceWait {
    fn drop(&mut self) {
        self.events.send(SourceEvent::AllowanceWaitEnded);
    }
}

impl Events {
    /// Report a wait of `wait` for the API allowance until the guard drops.
    #[doc(hidden)]
    pub fn allowance_wait(&self, wait: Duration) -> AllowanceWait {
        self.send(SourceEvent::AllowanceWait { wait });
        AllowanceWait {
            events: self.clone(),
        }
    }
}

impl Default for Events {
    fn default() -> Events {
        Events::new(1024)
    }
}

/// A running job. Dropping it without [`Job::finish`] or [`Job::fail`]
/// (a cancelled future) reports `Failed { error: "cancelled" }`.
#[doc(hidden)]
pub struct Job {
    events: Events,
    id: JobId,
    done: bool,
}

impl Job {
    #[doc(hidden)]
    pub fn progress(&self, bytes: u64) {
        if bytes > 0 {
            self.events.send(SourceEvent::Progress {
                job: self.id,
                bytes,
            });
        }
    }

    #[doc(hidden)]
    pub fn retry(&self, attempt: u32, delay: Duration, error: &SourceError) {
        self.events.send(SourceEvent::Retry {
            job: self.id,
            attempt,
            delay,
            error: error.to_string(),
        });
    }

    /// Report the outcome of `r` and pass it through.
    #[doc(hidden)]
    pub fn complete<T>(mut self, r: crate::error::Result<T>) -> crate::error::Result<T> {
        self.done = true;
        self.events.send(match &r {
            Ok(_) => SourceEvent::Finished { job: self.id },
            Err(e) => SourceEvent::Failed {
                job: self.id,
                error: e.to_string(),
            },
        });
        r
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        if !self.done {
            self.events.send(SourceEvent::Failed {
                job: self.id,
                error: "cancelled".into(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_report_start_progress_and_outcome() {
        let ev = Events::new(16);
        let mut rx = ev.subscribe();
        let job = ev.start("a", Some(10));
        job.progress(4);
        job.progress(0); // not reported
        let r: crate::error::Result<()> = job.complete(Ok(()));
        r.unwrap();
        let failing = ev.start("b", None);
        let _ = failing.complete::<()>(Err(SourceError::NexusUnauthorized));
        drop(ev.start("c", None));

        let got: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let (a, b, c) = (JobId(1), JobId(2), JobId(3));
        assert_eq!(
            got[0],
            SourceEvent::Started {
                job: a,
                label: "a".into(),
                total: Some(10)
            }
        );
        assert_eq!(got[1], SourceEvent::Progress { job: a, bytes: 4 });
        assert_eq!(got[2], SourceEvent::Finished { job: a });
        assert!(
            matches!(&got[4], SourceEvent::Failed { job, error } if *job == b && error.contains("401"))
        );
        assert_eq!(
            got[6],
            SourceEvent::Failed {
                job: c,
                error: "cancelled".into()
            }
        );
        assert_eq!(got.len(), 7);
    }

    #[test]
    fn an_allowance_wait_is_reported_until_its_guard_drops() {
        let ev = Events::new(16);
        let mut rx = ev.subscribe();
        let wait = ev.allowance_wait(Duration::from_secs(600));
        assert_eq!(
            rx.try_recv().unwrap(),
            SourceEvent::AllowanceWait {
                wait: Duration::from_secs(600)
            }
        );
        assert!(rx.try_recv().is_err(), "nothing more while it waits");
        drop(wait);
        assert_eq!(rx.try_recv().unwrap(), SourceEvent::AllowanceWaitEnded);
    }
}

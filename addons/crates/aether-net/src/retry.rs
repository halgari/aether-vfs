use std::future::Future;
use std::time::Duration;

use crate::error::{Result, SourceError};
use crate::events::Job;

/// Exponential backoff for retryable errors ([`SourceError::is_retryable`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total tries including the first.
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
    /// A server's Retry-After longer than this fails instead of waiting.
    pub max_retry_after: Duration,
}

impl Default for RetryPolicy {
    fn default() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
            max_retry_after: Duration::from_secs(300),
        }
    }
}

impl RetryPolicy {
    /// Delay before retry number `attempt` (1-based): the server's
    /// Retry-After if it gave one, else `base * 2^(attempt-1)` capped at `max_delay`.
    pub fn delay(&self, attempt: u32, err: &SourceError) -> Duration {
        err.retry_after().unwrap_or_else(|| {
            let exp = self
                .base_delay
                .saturating_mul(1u32 << (attempt.saturating_sub(1)).min(20));
            exp.min(self.max_delay)
        })
    }

    /// Run `op` until it succeeds, fails with a non-retryable error, or
    /// `max_attempts` is reached. Each retry is reported on `job`.
    #[doc(hidden)]
    pub async fn run<T, F, Fut>(&self, job: &Job, mut op: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let mut attempt = 1;
        loop {
            match op().await {
                Ok(v) => return Ok(v),
                Err(e) if e.is_retryable() && attempt < self.max_attempts => {
                    let delay = self.delay(attempt, &e);
                    if delay > self.max_retry_after {
                        return Err(e);
                    }
                    job.retry(attempt, delay, &e);
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Events, SourceEvent};
    use std::sync::atomic::{AtomicU32, Ordering};

    fn busy() -> SourceError {
        SourceError::Status {
            url: "u".into(),
            status: 503,
            body: String::new(),
        }
    }

    fn fast() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(3),
            max_retry_after: Duration::from_secs(1),
        }
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let p = RetryPolicy::default();
        assert_eq!(p.delay(1, &busy()), Duration::from_millis(500));
        assert_eq!(p.delay(3, &busy()), Duration::from_secs(2));
        assert_eq!(p.delay(40, &busy()), Duration::from_secs(30));
        let rl = SourceError::RateLimited {
            host: "h".into(),
            retry_after: Some(Duration::from_secs(7)),
        };
        assert_eq!(p.delay(1, &rl), Duration::from_secs(7));
    }

    #[tokio::test]
    async fn retries_transient_errors_then_succeeds() {
        let ev = Events::new(16);
        let mut rx = ev.subscribe();
        let job = ev.start("j", None);
        let calls = AtomicU32::new(0);
        let r = fast()
            .run(&job, || async {
                if calls.fetch_add(1, Ordering::SeqCst) < 2 {
                    Err(busy())
                } else {
                    Ok(42)
                }
            })
            .await;
        assert_eq!(r.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        let _ = job.complete(Ok(()));
        let retries = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|e| matches!(e, SourceEvent::Retry { .. }))
            .count();
        assert_eq!(retries, 2);
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts_and_on_fatal_errors() {
        let job = Events::new(4).start("j", None);
        let calls = AtomicU32::new(0);
        let r: Result<()> = fast()
            .run(&job, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(busy())
            })
            .await;
        assert!(matches!(r, Err(SourceError::Status { status: 503, .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        calls.store(0, Ordering::SeqCst);
        let r: Result<()> = fast()
            .run(&job, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(SourceError::NexusUnauthorized)
            })
            .await;
        assert!(matches!(r, Err(SourceError::NexusUnauthorized)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // A Retry-After beyond max_retry_after fails at once.
        calls.store(0, Ordering::SeqCst);
        let r: Result<()> = fast()
            .run(&job, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(SourceError::RateLimited {
                    host: "h".into(),
                    retry_after: Some(Duration::from_secs(3600)),
                })
            })
            .await;
        assert!(matches!(r, Err(SourceError::RateLimited { .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let _ = job.complete(Ok(()));
    }
}

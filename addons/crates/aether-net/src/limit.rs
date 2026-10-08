use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Caps concurrent requests: `global` in total and `per_host` to any one host.
#[derive(Debug)]
pub struct Limiter {
    global: Arc<Semaphore>,
    per_host_limit: usize,
    hosts: Mutex<HashMap<String, Arc<Semaphore>>>,
}

/// Held for the duration of one request (headers and body).
#[derive(Debug)]
pub struct Permit {
    _host: OwnedSemaphorePermit,
    _global: OwnedSemaphorePermit,
}

impl Limiter {
    pub fn new(global: usize, per_host: usize) -> Limiter {
        Limiter {
            global: Arc::new(Semaphore::new(global.max(1))),
            per_host_limit: per_host.max(1),
            hosts: Mutex::new(HashMap::new()),
        }
    }

    /// Wait for a slot for `host`. The host slot is taken first, so a busy
    /// host never holds global slots other hosts could use.
    pub async fn acquire(&self, host: &str) -> Permit {
        let host_sem = self
            .hosts
            .lock()
            .expect("limiter mutex poisoned")
            .entry(host.to_ascii_lowercase())
            .or_insert_with(|| Arc::new(Semaphore::new(self.per_host_limit)))
            .clone();
        let host = host_sem
            .acquire_owned()
            .await
            .expect("semaphore never closed");
        let global = self
            .global
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore never closed");
        Permit {
            _host: host,
            _global: global,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn enforces_global_and_per_host_limits() {
        let lim = Arc::new(Limiter::new(3, 2));
        let now = Arc::new([
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
        ]);
        let peak = Arc::new([
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
        ]);
        let total_peak = Arc::new(AtomicUsize::new(0));
        let total_now = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for i in 0..30 {
            let (lim, now, peak) = (lim.clone(), now.clone(), peak.clone());
            let (total_now, total_peak) = (total_now.clone(), total_peak.clone());
            tasks.push(tokio::spawn(async move {
                let h = i % 3;
                let _p = lim.acquire(&format!("host{h}")).await;
                let n = now[h].fetch_add(1, Ordering::SeqCst) + 1;
                peak[h].fetch_max(n, Ordering::SeqCst);
                let t = total_now.fetch_add(1, Ordering::SeqCst) + 1;
                total_peak.fetch_max(t, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
                total_now.fetch_sub(1, Ordering::SeqCst);
                now[h].fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(total_peak.load(Ordering::SeqCst), 3);
        for p in peak.iter() {
            assert!(p.load(Ordering::SeqCst) <= 2);
        }
    }

    #[tokio::test]
    async fn host_names_are_case_insensitive() {
        let lim = Limiter::new(10, 1);
        let _a = lim.acquire("Example.COM").await;
        let b = tokio::time::timeout(Duration::from_millis(50), lim.acquire("example.com")).await;
        assert!(b.is_err(), "second permit for the same host must wait");
    }
}

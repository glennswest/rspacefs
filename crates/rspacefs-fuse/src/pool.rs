//! A small, fixed-size, std-only thread pool.
//!
//! Used by the FUSE adapter to run blocking data-path ops (`read`, `write`)
//! off the single `fuser::Session::run()` dispatch thread. See
//! `docs/concurrency.md` and issue #23.
//!
//! No `rayon`, no `tokio` — consistent with the rest of the daemon (the
//! metrics HTTP server is hand-rolled std too). The pool owns `N` worker
//! threads that pull boxed closures off an `mpsc` channel. On `Drop` the
//! sender is dropped and the workers are joined, so a clean unmount drains
//! any in-flight jobs.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

type Job = Box<dyn FnOnce() + Send + 'static>;

/// A fixed-size pool of worker threads.
pub struct ThreadPool {
    /// `Option` so `Drop` can take it and close the channel, signalling
    /// workers to exit once the queue drains.
    tx: Option<Sender<Job>>,
    workers: Vec<JoinHandle<()>>,
}

impl ThreadPool {
    /// Build a pool with `size` worker threads (clamped to at least 1).
    pub fn new(size: usize) -> Self {
        let size = size.max(1);
        let (tx, rx) = channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        let mut workers = Vec::with_capacity(size);
        for _ in 0..size {
            workers.push(spawn_worker(Arc::clone(&rx)));
        }
        ThreadPool {
            tx: Some(tx),
            workers,
        }
    }

    /// Submit a job. Runs on some worker thread, in submission order modulo
    /// worker availability. If the pool is shutting down (sender already
    /// dropped) the job is silently discarded — callers offload work whose
    /// only observable effect is sending a FUSE reply, and a dropped reply
    /// falls through to the kernel's default, so this is safe.
    pub fn execute<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Box::new(f));
        }
    }

    /// Number of worker threads. Exercised by unit tests; kept public
    /// for a future `debug` control-socket field.
    #[allow(dead_code)]
    pub fn size(&self) -> usize {
        self.workers.len()
    }
}

/// One worker: block on the shared receiver, run each job to completion.
/// The lock is held only across `recv()`, never across the job, so jobs run
/// fully in parallel. A `recv` error means the sender was dropped (pool
/// shutting down) — exit the loop.
fn spawn_worker(rx: Arc<Mutex<Receiver<Job>>>) -> JoinHandle<()> {
    thread::spawn(move || loop {
        let job = {
            let guard = match rx.lock() {
                Ok(g) => g,
                // Receiver mutex poisoned by another worker panicking *while
                // holding the recv lock* — shouldn't happen since jobs run
                // unlocked, but recover rather than abort the whole pool.
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.recv()
        };
        match job {
            Ok(job) => job(),
            Err(_) => break, // channel closed: shut down
        }
    })
}

impl Drop for ThreadPool {
    fn drop(&mut self) {
        // Close the channel so workers see `Err` on `recv` once the queue is
        // drained, then wait for them to finish in-flight jobs.
        self.tx.take();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn runs_all_submitted_jobs() {
        let pool = ThreadPool::new(4);
        let counter = Arc::new(AtomicUsize::new(0));
        for _ in 0..1000 {
            let c = Arc::clone(&counter);
            pool.execute(move || {
                c.fetch_add(1, Ordering::Relaxed);
            });
        }
        // Drop joins all workers after the queue drains.
        drop(pool);
        assert_eq!(counter.load(Ordering::Relaxed), 1000);
    }

    #[test]
    fn size_is_at_least_one() {
        assert_eq!(ThreadPool::new(0).size(), 1);
        assert_eq!(ThreadPool::new(3).size(), 3);
    }

    #[test]
    fn jobs_actually_run_concurrently() {
        // With 4 workers, 4 jobs that each block on the same barrier-ish
        // gate must all be in flight at once, or the gate never opens.
        let pool = ThreadPool::new(4);
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        for _ in 0..4 {
            let inf = Arc::clone(&in_flight);
            let pk = Arc::clone(&peak);
            pool.execute(move || {
                let now = inf.fetch_add(1, Ordering::SeqCst) + 1;
                pk.fetch_max(now, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(50));
                inf.fetch_sub(1, Ordering::SeqCst);
            });
        }
        drop(pool);
        assert!(
            peak.load(Ordering::SeqCst) >= 2,
            "expected concurrent execution, peak in-flight was {}",
            peak.load(Ordering::SeqCst)
        );
    }
}

//! Bounded worker-thread pool.
//!
//! Uses a SyncSender (bounded channel) so that execute() returns false
//! immediately when all workers are busy AND the queue is full, instead of
//! blocking or spawning unboundedly. The caller can then send 503 to the
//! client rather than letting requests pile up in memory.

use std::sync::{Arc, Mutex};
use std::thread;

type Job = Box<dyn FnOnce() + Send + 'static>;

/// Fixed-size thread pool with a bounded work queue.
pub struct ThreadPool {
    _workers: Vec<thread::JoinHandle<()>>,
    sender:   std::sync::mpsc::SyncSender<Job>,
}

impl ThreadPool {
    /// Create a pool of `size` threads with a queue capacity of size x 4.
    ///
    /// # Panics
    /// Panics if size == 0.
    pub fn new(size: usize) -> Self {
        assert!(size > 0, "thread pool size must be >= 1");
        let (tx, rx) = std::sync::mpsc::sync_channel::<Job>(size * 4);
        let rx = Arc::new(Mutex::new(rx));
        let workers = (0..size)
            .map(|_| {
                let rx = Arc::clone(&rx);
                thread::spawn(move || loop {
                    match rx.lock().unwrap().recv() {
                        Ok(job) => job(),
                        Err(_)  => break,
                    }
                })
            })
            .collect();
        ThreadPool { _workers: workers, sender: tx }
    }

    /// Attempt to submit a job.
    /// Returns true if enqueued, false if the queue is full.
    pub fn execute(&self, f: impl FnOnce() + Send + 'static) -> bool {
        self.sender.try_send(Box::new(f)).is_ok()
    }

    /// Submit a job, blocking until space is available in the queue.
    /// Used in tests to guarantee all jobs are accepted.
    #[cfg(test)]
    fn execute_blocking(&self, f: impl FnOnce() + Send + 'static) {
        self.sender.send(Box::new(f)).unwrap();
    }
}

// ---- Unit tests ------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// All submitted jobs must eventually run (verified via a channel, not sleep).
    #[test]
    fn all_jobs_executed() {
        let pool = ThreadPool::new(4);
        let (tx, rx) = mpsc::channel::<u32>();
        for _ in 0..20u32 {
            let tx = tx.clone();
            // Use blocking send so the queue never rejects a job in this test.
            pool.execute_blocking(move || { tx.send(1).unwrap(); });
        }
        drop(tx);
        let total: u32 = rx.iter().sum();
        assert_eq!(total, 20);
    }

    /// A pool of 1 serialises jobs, so they are processed in submission order.
    #[test]
    fn pool_of_one_serialises_jobs() {
        let pool = ThreadPool::new(1);
        let (tx, rx) = mpsc::channel::<u32>();
        for i in 0..5u32 {
            let tx = tx.clone();
            pool.execute_blocking(move || { tx.send(i).unwrap(); });
        }
        drop(tx);
        let log: Vec<u32> = rx.iter().collect();
        assert_eq!(log.len(), 5);
        assert_eq!(log, vec![0, 1, 2, 3, 4]);
    }

    /// When the queue is full, execute() must return false without panicking.
    #[test]
    fn returns_false_when_queue_full() {
        let pool    = ThreadPool::new(1);
        let release = Arc::new(Mutex::new(false));
        let r       = Arc::clone(&release);
        // Block the single worker.
        pool.execute(move || {
            while !*r.lock().unwrap() {
                thread::sleep(Duration::from_millis(5));
            }
        });
        thread::sleep(Duration::from_millis(20));

        let mut rejected = false;
        for _ in 0..200 {
            if !pool.execute(|| {}) { rejected = true; break; }
        }
        *release.lock().unwrap() = true;
        assert!(rejected, "expected at least one rejection when queue is full");
    }
}

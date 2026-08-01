//! Counting semaphore bounding the loopback servers' per-connection threads.
//!
//! The control socket and file server spawn one handler thread per accepted
//! connection. Without a bound, a local process spamming connections could pile
//! up threads/FDs without limit. Each accept loop claims a slot *before*
//! spawning; at the cap the accept loop blocks, so excess connections wait in
//! the kernel listen backlog instead of growing our thread count. Handler read
//! timeouts guarantee stuck slots free themselves.
//!
//! Hand-rolled on Mutex + Condvar to keep the backend's no-new-dependencies
//! posture (cf. the hand-rolled HTTP in file_server.rs).

use std::sync::{Arc, Condvar, Mutex};

pub struct ConnectionLimiter {
    inner: Arc<LimiterInner>,
}

struct LimiterInner {
    cap: usize,
    active: Mutex<usize>,
    freed: Condvar,
}

impl ConnectionLimiter {
    pub fn new(cap: usize) -> Self {
        assert!(cap > 0, "connection cap must be positive");
        Self {
            inner: Arc::new(LimiterInner {
                cap,
                active: Mutex::new(0),
                freed: Condvar::new(),
            }),
        }
    }

    /// Blocks until a slot is free, then claims it. The slot is released when
    /// the returned guard drops — including on handler-thread panic, since
    /// unwinding runs destructors.
    pub fn acquire(&self) -> ConnectionSlot {
        // Recover from poisoning: the counter is a plain usize, so a panic while
        // holding the lock cannot leave it torn, and the poisoned value is still
        // the true count (the panicking thread's guard decrements on unwind).
        let mut active = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while *active >= self.inner.cap {
            active = self
                .inner
                .freed
                .wait(active)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        *active += 1;
        ConnectionSlot {
            inner: Arc::clone(&self.inner),
        }
    }
}

pub struct ConnectionSlot {
    inner: Arc<LimiterInner>,
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        let mut active = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *active = active.saturating_sub(1);
        self.inner.freed.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn slots_up_to_cap_acquire_without_blocking() {
        let limiter = ConnectionLimiter::new(3);
        let _a = limiter.acquire();
        let _b = limiter.acquire();
        let _c = limiter.acquire();
    }

    #[test]
    fn acquire_blocks_at_cap_and_wakes_on_release() {
        let limiter = ConnectionLimiter::new(1);
        let held = limiter.acquire();

        let (tx, rx) = mpsc::channel();
        let waiter = {
            let inner = Arc::clone(&limiter.inner);
            thread::spawn(move || {
                let slot = ConnectionLimiter { inner }.acquire();
                tx.send(()).unwrap();
                drop(slot);
            })
        };

        // The waiter must not get a slot while the cap is held.
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());

        drop(held);
        rx.recv_timeout(Duration::from_secs(5))
            .expect("waiter should acquire the freed slot");
        waiter.join().unwrap();
    }

    #[test]
    fn slot_released_on_panic() {
        let limiter = ConnectionLimiter::new(1);
        let inner = Arc::clone(&limiter.inner);
        let panicker = thread::spawn(move || {
            let _slot = ConnectionLimiter { inner }.acquire();
            panic!("handler crashed");
        });
        assert!(panicker.join().is_err());

        // The panicked thread's guard must have freed the slot (and left the
        // lock recoverable from poisoning).
        let _slot = limiter.acquire();
    }
}

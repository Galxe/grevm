//! Predicate-based notification for scheduler coordinator threads.
//!
//! The caller's predicate is authoritative. Rust's parker already coalesces notifications into a
//! single token and guarantees that `unpark` before `park` makes the next `park` return
//! immediately, so no second atomic state machine is needed here.

use std::{
    sync::OnceLock,
    thread::{self, Thread},
    time::Duration,
};

/// A single-consumer notification slot.
///
/// Notifications are hints to recheck the caller's predicate. They may arrive before registration,
/// before parking, while parked, or after a timeout; every case is safe because the predicate is
/// checked before each park and spurious wakeups are allowed.
#[derive(Debug)]
pub(super) struct WaitSlot {
    thread: OnceLock<Thread>,
}

impl WaitSlot {
    pub(super) fn new() -> Self {
        Self { thread: OnceLock::new() }
    }

    pub(super) fn register_current_thread(&self) {
        self.thread
            .set(thread::current())
            .expect("scheduler wait thread registered more than once");
    }

    pub(super) fn notify(&self) {
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
    }

    /// Park only while `blocked` remains true.
    ///
    /// `Thread::unpark` publishes a token even when it races between the second predicate check
    /// and `park_timeout`, closing the usual check/park lost-wakeup window.
    pub(super) fn wait_while(&self, timeout: Duration, mut blocked: impl FnMut() -> bool) {
        if !blocked() {
            return;
        }

        // Most scheduler stalls close within one worker timeslice.
        thread::yield_now();
        if blocked() {
            thread::park_timeout(timeout);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        time::Instant,
    };

    #[test]
    fn notification_before_registration_is_covered_by_the_predicate() {
        let slot = WaitSlot::new();
        let ready = AtomicBool::new(true);

        slot.notify();
        slot.register_current_thread();
        slot.wait_while(Duration::from_secs(1), || !ready.load(Ordering::Acquire));
    }

    #[test]
    fn notification_between_check_and_park_is_not_lost() {
        let slot = WaitSlot::new();
        let ready = AtomicBool::new(false);
        slot.register_current_thread();

        let mut checks = 0;
        let start = Instant::now();
        slot.wait_while(Duration::from_secs(1), || {
            checks += 1;
            if checks == 2 {
                ready.store(true, Ordering::Release);
                slot.notify();
            }
            true
        });

        assert!(start.elapsed() < Duration::from_millis(100));
        assert!(ready.load(Ordering::Acquire));
    }

    #[test]
    fn notification_wakes_a_parked_consumer() {
        let slot = Arc::new(WaitSlot::new());
        let ready = Arc::new(AtomicBool::new(false));
        let (parked_tx, parked_rx) = mpsc::channel();

        let consumer_slot = Arc::clone(&slot);
        let consumer_ready = Arc::clone(&ready);
        let consumer = thread::spawn(move || {
            consumer_slot.register_current_thread();
            let mut checks = 0;
            consumer_slot.wait_while(Duration::from_secs(1), || {
                checks += 1;
                if checks == 2 {
                    parked_tx.send(()).expect("park notification");
                }
                !consumer_ready.load(Ordering::Acquire)
            });
        });

        parked_rx.recv_timeout(Duration::from_secs(1)).expect("consumer did not prepare to park");
        ready.store(true, Ordering::Release);
        slot.notify();
        consumer.join().expect("consumer thread");
    }

    #[test]
    fn timeout_does_not_prevent_slot_reuse() {
        let slot = WaitSlot::new();
        slot.register_current_thread();

        slot.wait_while(Duration::from_millis(1), || true);

        let ready = AtomicBool::new(true);
        slot.wait_while(Duration::from_secs(1), || !ready.load(Ordering::Acquire));
    }
}

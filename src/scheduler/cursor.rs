use std::sync::atomic::{AtomicUsize, Ordering};

/// A monotonic cursor published by one scheduler coordinator.
#[derive(Debug)]
#[repr(transparent)]
pub(super) struct PublishedCursor(AtomicUsize);

impl PublishedCursor {
    #[inline]
    pub(super) fn new(value: usize) -> Self {
        Self(AtomicUsize::new(value))
    }

    #[inline]
    pub(super) fn get(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }

    #[inline]
    pub(super) fn publish(&self, value: usize) {
        debug_assert!(value >= self.get(), "published cursors must not move backwards");
        self.0.store(value, Ordering::Release);
    }

    #[inline]
    pub(super) fn reader(&self) -> PublishedCursorReader<'_> {
        PublishedCursorReader(&self.0)
    }
}

/// Read-only access to a published cursor.
///
/// Cache and dependency readers need the live committed boundary, but must not be able to publish
/// or rewind it.
#[derive(Debug, Clone, Copy)]
#[repr(transparent)]
pub(crate) struct PublishedCursorReader<'a>(&'a AtomicUsize);

impl PublishedCursorReader<'_> {
    #[cfg(test)]
    pub(crate) fn new(cursor: &AtomicUsize) -> PublishedCursorReader<'_> {
        PublishedCursorReader(cursor)
    }

    #[inline]
    pub(crate) fn get(self) -> usize {
        self.0.load(Ordering::Acquire)
    }
}

trait RewindableAtomic {
    fn load(&self, ordering: Ordering) -> usize;
    fn compare_exchange_weak(
        &self,
        current: usize,
        new: usize,
        success: Ordering,
        failure: Ordering,
    ) -> Result<usize, usize>;
}

impl RewindableAtomic for AtomicUsize {
    #[inline]
    fn load(&self, ordering: Ordering) -> usize {
        AtomicUsize::load(self, ordering)
    }

    #[inline]
    fn compare_exchange_weak(
        &self,
        current: usize,
        new: usize,
        success: Ordering,
        failure: Ordering,
    ) -> Result<usize, usize> {
        AtomicUsize::compare_exchange_weak(self, current, new, success, failure)
    }
}

#[inline]
fn claim_before(cursor: &impl RewindableAtomic, limit: usize) -> Option<usize> {
    loop {
        let current = cursor.load(Ordering::Acquire);
        if current >= limit {
            return None;
        }
        if cursor
            .compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Some(current);
        }
    }
}

/// A validation cursor that can be rewound when earlier work becomes eligible again.
#[derive(Debug)]
#[repr(transparent)]
pub(super) struct RewindableCursor(AtomicUsize);

impl RewindableCursor {
    #[inline]
    pub(super) fn new(value: usize) -> Self {
        Self(AtomicUsize::new(value))
    }

    #[inline]
    pub(super) fn get(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }

    #[inline]
    pub(super) fn claim_before(&self, limit: usize) -> Option<usize> {
        claim_before(&self.0, limit)
    }

    #[inline]
    pub(super) fn rewind(&self, value: usize) -> usize {
        self.0.fetch_min(value, Ordering::AcqRel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl RewindableAtomic for loom::sync::atomic::AtomicUsize {
        fn load(&self, ordering: Ordering) -> usize {
            loom::sync::atomic::AtomicUsize::load(self, ordering)
        }

        fn compare_exchange_weak(
            &self,
            current: usize,
            new: usize,
            success: Ordering,
            failure: Ordering,
        ) -> Result<usize, usize> {
            loom::sync::atomic::AtomicUsize::compare_exchange_weak(
                self, current, new, success, failure,
            )
        }
    }

    #[test]
    fn rewind_concurrent_with_claim_reissues_every_rewound_index() {
        loom::model(|| {
            use loom::{
                sync::{
                    Arc,
                    atomic::{AtomicUsize, Ordering},
                },
                thread,
            };

            const NO_CLAIM: usize = usize::MAX;

            let cursor = Arc::new(AtomicUsize::new(1));
            let concurrent_claim = Arc::new(AtomicUsize::new(NO_CLAIM));

            let claim_cursor = Arc::clone(&cursor);
            let claimed = Arc::clone(&concurrent_claim);
            let claim_thread = thread::spawn(move || {
                if let Some(index) = claim_before(claim_cursor.as_ref(), 2) {
                    claimed.store(index, Ordering::Relaxed);
                }
            });

            let rewind_cursor = Arc::clone(&cursor);
            let rewind_thread = thread::spawn(move || {
                rewind_cursor.fetch_min(0, Ordering::AcqRel);
            });

            claim_thread.join().unwrap();
            rewind_thread.join().unwrap();

            let mut claims = Vec::new();
            let concurrent = concurrent_claim.load(Ordering::Relaxed);
            if concurrent != NO_CLAIM {
                claims.push(concurrent);
            }
            while let Some(index) = claim_before(cursor.as_ref(), 2) {
                claims.push(index);
            }

            assert!(claims.iter().all(|&index| index < 2));
            assert!(claims.contains(&0), "rewound index must become claimable");
            assert!(claims.contains(&1), "the original position must remain claimable");
        });
    }

    #[test]
    fn rewind_requires_newer_validation_before_finality() {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum Status {
            Executed,
            Unconfirmed,
            Finality,
        }

        #[derive(Debug)]
        struct TransactionState {
            status: Status,
            rewind_timestamp: usize,
            finality_timestamp: usize,
            validation_claimed: bool,
        }

        loom::model(|| {
            use loom::{
                sync::{
                    Arc, Mutex,
                    atomic::{AtomicUsize, Ordering},
                },
                thread,
            };

            // Transaction 0 has completed its first validation at timestamp 1.
            let validation = Arc::new(AtomicUsize::new(1));
            let lower = Arc::new(AtomicUsize::new(0));
            let logical_clock = Arc::new(AtomicUsize::new(2));
            let cursor = Arc::new(AtomicUsize::new(1));
            let state = Arc::new(Mutex::new(TransactionState {
                status: Status::Unconfirmed,
                rewind_timestamp: 0,
                finality_timestamp: 0,
                validation_claimed: false,
            }));

            let rewind_cursor = Arc::clone(&cursor);
            let rewind_state = Arc::clone(&state);
            let rewind_lower = Arc::clone(&lower);
            let rewind_clock = Arc::clone(&logical_clock);
            let rewind_thread = thread::spawn(move || {
                let mut state = rewind_state.lock().unwrap();
                if state.status == Status::Finality {
                    return;
                }

                // Match the production ordering under the transaction state lock: invalidate the
                // old status, publish its lower timestamp, then reissue validation.
                state.status = Status::Executed;
                let timestamp = rewind_clock.fetch_add(1, Ordering::AcqRel);
                rewind_lower.fetch_max(timestamp, Ordering::AcqRel);
                rewind_cursor.fetch_min(0, Ordering::AcqRel);
                state.rewind_timestamp = timestamp;
            });

            let consumer_cursor = Arc::clone(&cursor);
            let consumer_state = Arc::clone(&state);
            let consumer_validation = Arc::clone(&validation);
            let consumer_lower = Arc::clone(&lower);
            let consumer_clock = Arc::clone(&logical_clock);
            let consumer_thread = thread::spawn(move || {
                if claim_before(consumer_cursor.as_ref(), 1) == Some(0) {
                    let mut state = consumer_state.lock().unwrap();
                    state.validation_claimed = true;
                    if state.status != Status::Finality {
                        let timestamp = consumer_clock.fetch_add(1, Ordering::AcqRel);
                        consumer_validation.fetch_max(timestamp, Ordering::AcqRel);
                        state.status = Status::Unconfirmed;
                    }
                }

                if consumer_cursor.load(Ordering::Acquire) == 0 {
                    return;
                }

                let mut state = consumer_state.lock().unwrap();
                if state.status != Status::Unconfirmed {
                    return;
                }
                let lower = consumer_lower.load(Ordering::Acquire);
                let validation = consumer_validation.load(Ordering::Acquire);
                if validation > lower {
                    state.status = Status::Finality;
                    state.finality_timestamp = validation;
                }
            });

            rewind_thread.join().unwrap();
            consumer_thread.join().unwrap();

            let state = state.lock().unwrap();
            let rewind = state.rewind_timestamp;
            let finality = state.finality_timestamp;
            if rewind != 0 && finality != 0 {
                assert!(
                    finality > rewind,
                    "a pre-rewind validation must not be eligible for finality"
                );
            }

            if rewind != 0 && !state.validation_claimed {
                assert_eq!(
                    claim_before(cursor.as_ref(), 1),
                    Some(0),
                    "rewind must leave transaction 0 available for validation"
                );
            }
        });
    }
}

use super::cursor::{PublishedCursor, PublishedCursorReader, RewindableCursor};
use std::{
    cmp::max,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[derive(Debug)]
struct ExecutionFrontier {
    executed: Vec<AtomicBool>,
    frontier: AtomicUsize,
}

impl ExecutionFrontier {
    fn new(num_txs: usize) -> Self {
        Self {
            executed: (0..num_txs).map(|_| AtomicBool::new(false)).collect(),
            frontier: AtomicUsize::new(0),
        }
    }

    fn advance(&self, mut start: usize) {
        loop {
            let mut end = start;
            while end < self.executed.len() && self.executed[end].load(Ordering::Acquire) {
                end += 1;
            }
            if end == start {
                return;
            }

            let current = self.frontier.fetch_max(end, Ordering::AcqRel);
            start = max(current, end);
        }
    }

    /// Publish that `index` has executed at least once.
    ///
    /// Only the transaction that fills the current gap attempts to advance the frontier.
    /// A single atomic update publishes the whole contiguous run that is already complete.
    fn publish(&self, index: usize) {
        let frontier = self.frontier.load(Ordering::Acquire);
        if index < frontier {
            return;
        }

        self.executed[index].store(true, Ordering::Release);
        // Reload after publishing. The frontier may have reached `index` between the first load
        // and the store; using the stale value would leave the newly filled gap unadvanced.
        let frontier = self.frontier.load(Ordering::Acquire);
        if index == frontier {
            self.advance(frontier);
        }
    }

    /// Return the first transaction that has not completed an initial execution.
    ///
    /// Readers participate in lock-free progress by advancing a ready frontier if needed.
    fn current(&self) -> usize {
        let frontier = self.frontier.load(Ordering::Acquire);
        // Lock-free helpers may observe a completion whose publishing worker has not advanced the
        // frontier yet. Help it here so a delayed publisher cannot stall validation progress.
        if frontier < self.executed.len() && self.executed[frontier].load(Ordering::Acquire) {
            self.advance(frontier);
            return self.frontier.load(Ordering::Acquire);
        }
        frontier
    }
}

/// Lock-free cursors and logical timestamps used by the scheduling state machine.
///
/// `validation` is the next validation claim and may rewind. `finality` and `committed` are
/// monotonic exclusive ends of the contiguous validated and ordered-commit prefixes,
/// respectively. `execution_frontier` is the first transaction without an initial execution.
pub(super) struct SchedulerContext {
    num_txs: usize,
    validation: RewindableCursor,
    finality: PublishedCursor,
    committed: PublishedCursor,
    execution_frontier: ExecutionFrontier,
    validation_resets: AtomicUsize,
    logical_clock: AtomicUsize,
    lower_timestamps: Vec<AtomicUsize>,
    unconfirmed_timestamps: Vec<AtomicUsize>,
}

impl SchedulerContext {
    pub(super) fn new(num_txs: usize) -> Self {
        Self {
            num_txs,
            validation: RewindableCursor::new(0),
            finality: PublishedCursor::new(0),
            committed: PublishedCursor::new(0),
            execution_frontier: ExecutionFrontier::new(num_txs),
            validation_resets: AtomicUsize::new(0),
            logical_clock: AtomicUsize::new(1),
            lower_timestamps: (0..num_txs).map(|_| AtomicUsize::new(0)).collect(),
            unconfirmed_timestamps: (0..num_txs).map(|_| AtomicUsize::new(0)).collect(),
        }
    }

    pub(super) fn rewind_validation_to(&self, index: usize) {
        if index >= self.num_txs {
            return;
        }
        // Publish invalidation before making the index claimable. Finality advances contiguously
        // and checks status plus this timestamp under transaction locks, so a validation predating
        // this rewind cannot enter the stable prefix afterward.
        let timestamp = self.logical_clock.fetch_add(1, Ordering::AcqRel);
        self.lower_timestamps[index].fetch_max(timestamp, Ordering::AcqRel);
        let previous = self.validation.rewind(index);
        if previous > index {
            self.validation_resets.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[inline]
    pub(super) fn logical_timestamp(&self) -> usize {
        self.logical_clock.fetch_add(1, Ordering::AcqRel)
    }

    #[inline]
    pub(super) fn executed(&self, index: usize) {
        self.execution_frontier.publish(index);
    }

    #[inline]
    pub(super) fn unconfirmed(&self, index: usize, timestamp: usize) {
        self.unconfirmed_timestamps[index].fetch_max(timestamp, Ordering::AcqRel);
    }

    #[inline]
    pub(super) fn finished(&self) -> bool {
        self.finality.get() >= self.num_txs
    }

    #[inline]
    pub(super) fn finality_idx(&self) -> usize {
        self.finality.get()
    }

    #[inline]
    pub(super) fn publish_finality(&self, index: usize) {
        self.finality.publish(index);
    }

    #[inline]
    pub(super) fn committed_idx(&self) -> usize {
        self.committed.get()
    }

    #[inline]
    pub(super) fn publish_commit(&self, index: usize) {
        self.committed.publish(index);
    }

    #[inline]
    pub(super) fn commit_cursor(&self) -> PublishedCursorReader<'_> {
        self.committed.reader()
    }

    #[inline]
    pub(super) fn validation_idx(&self) -> usize {
        self.validation.get()
    }

    #[inline]
    pub(super) fn validation_reset_count(&self) -> usize {
        self.validation_resets.load(Ordering::Relaxed)
    }

    #[inline]
    pub(super) fn lower_timestamp(&self, index: usize) -> usize {
        self.lower_timestamps[index].load(Ordering::Acquire)
    }

    #[inline]
    pub(super) fn unconfirmed_timestamp(&self, index: usize) -> usize {
        self.unconfirmed_timestamps[index].load(Ordering::Acquire)
    }

    #[inline]
    pub(super) fn execution_frontier(&self) -> usize {
        self.execution_frontier.current()
    }

    #[inline]
    pub(super) fn should_schedule(&self, executing_idx: usize) -> bool {
        let validation_idx = self.validation.get();
        let should_validate =
            validation_idx < executing_idx && validation_idx < self.execution_frontier.current();
        should_validate || executing_idx < self.num_txs
    }

    #[inline]
    pub(super) fn next_validation_idx(&self, executing_idx: usize) -> Option<usize> {
        let validation_limit = executing_idx.min(self.execution_frontier.current());
        self.validation.claim_before(validation_limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    #[test]
    fn execution_frontier_advances_over_out_of_order_completions() {
        let frontier = ExecutionFrontier::new(5);
        frontier.publish(4);
        frontier.publish(2);
        frontier.publish(1);
        assert_eq!(frontier.current(), 0);

        frontier.publish(0);
        assert_eq!(frontier.current(), 3);

        frontier.publish(3);
        assert_eq!(frontier.current(), 5);
    }

    #[test]
    fn reader_helps_a_delayed_frontier_publisher() {
        let frontier = ExecutionFrontier::new(2);
        frontier.executed[0].store(true, Ordering::Release);
        assert_eq!(frontier.current(), 1);
    }

    #[test]
    fn execution_frontier_handles_concurrent_publishers() {
        let frontier = ExecutionFrontier::new(1_000);
        std::thread::scope(|scope| {
            for offset in 0..8 {
                let frontier = &frontier;
                scope.spawn(move || {
                    for index in (offset..1_000).step_by(8).rev() {
                        frontier.publish(index);
                    }
                });
            }
        });
        assert_eq!(frontier.current(), 1_000);
    }

    #[test]
    fn concurrent_validation_claims_do_not_cross_execution_limit() {
        let context = SchedulerContext::new(32);
        for index in 0..8 {
            context.executed(index);
        }

        let claimed = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let context = &context;
                let claimed = &claimed;
                scope.spawn(move || {
                    while let Some(index) = context.next_validation_idx(5) {
                        claimed.lock().push(index);
                    }
                });
            }
        });

        let mut claimed = claimed.into_inner();
        claimed.sort_unstable();
        assert_eq!(claimed, (0..5).collect::<Vec<_>>());
        assert_eq!(context.validation_idx(), 5);
    }
}

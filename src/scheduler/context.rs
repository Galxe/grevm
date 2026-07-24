use crate::utils::ContinuousDetectSet;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Lock-free cursors and logical timestamps used by the scheduling state machine.
pub(super) struct SchedulerContext {
    num_txs: usize,
    validation_idx: AtomicUsize,
    pub(super) finality_idx: AtomicUsize,
    pub(super) commit_idx: AtomicUsize,
    pub(super) executed_set: ContinuousDetectSet,
    pub(super) reset_validation_idx_cnt: AtomicUsize,
    logical_ts: AtomicUsize,
    pub(super) lower_ts: Vec<AtomicUsize>,
    pub(super) unconfirmed_ts: Vec<AtomicUsize>,
}

impl SchedulerContext {
    pub(super) fn new(num_txs: usize) -> Self {
        Self {
            num_txs,
            validation_idx: AtomicUsize::new(0),
            finality_idx: AtomicUsize::new(0),
            commit_idx: AtomicUsize::new(0),
            executed_set: ContinuousDetectSet::new(num_txs),
            reset_validation_idx_cnt: AtomicUsize::new(0),
            logical_ts: AtomicUsize::new(1),
            lower_ts: (0..num_txs).map(|_| AtomicUsize::new(0)).collect(),
            unconfirmed_ts: (0..num_txs).map(|_| AtomicUsize::new(0)).collect(),
        }
    }

    pub(super) fn reset_validation_idx(&self, index: usize) {
        if index >= self.num_txs {
            return;
        }
        let timestamp = self.logical_ts.fetch_add(1, Ordering::AcqRel);
        self.lower_ts[index].fetch_max(timestamp, Ordering::AcqRel);
        let previous = self.validation_idx.fetch_min(index, Ordering::AcqRel);
        if previous > index {
            self.reset_validation_idx_cnt.fetch_add(1, Ordering::AcqRel);
        }
    }

    pub(super) fn logical_timestamp(&self) -> usize {
        self.logical_ts.fetch_add(1, Ordering::AcqRel)
    }

    pub(super) fn executed(&self, index: usize) {
        self.executed_set.add(index);
    }

    pub(super) fn unconfirmed(&self, index: usize, timestamp: usize) {
        self.unconfirmed_ts[index].fetch_max(timestamp, Ordering::AcqRel);
    }

    pub(super) fn finished(&self) -> bool {
        self.finality_idx.load(Ordering::Acquire) >= self.num_txs
    }

    pub(super) fn finality_idx(&self) -> usize {
        self.finality_idx.load(Ordering::Acquire)
    }

    pub(super) fn validation_idx(&self) -> usize {
        self.validation_idx.load(Ordering::Acquire)
    }

    pub(super) fn should_schedule(&self, executing_idx: usize) -> bool {
        let validation_idx = self.validation_idx.load(Ordering::Acquire);
        let should_validate =
            validation_idx < executing_idx && validation_idx < self.executed_set.continuous_idx();
        should_validate || executing_idx < self.num_txs
    }

    pub(super) fn next_validation_idx(&self, executing_idx: usize) -> Option<usize> {
        let validation_idx = self.validation_idx.load(Ordering::Acquire);
        if validation_idx >= executing_idx || validation_idx >= self.executed_set.continuous_idx() {
            return None;
        }
        let next = self.validation_idx.fetch_add(1, Ordering::AcqRel);
        (next < self.num_txs).then_some(next)
    }
}

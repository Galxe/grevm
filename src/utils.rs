use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Debug)]
pub(crate) struct ContinuousDetectSet {
    num_flag: usize,
    index_flag: Vec<AtomicBool>,
    num_index: AtomicUsize,
    continuous_idx: AtomicUsize,
}

impl ContinuousDetectSet {
    pub(crate) fn new(num_flag: usize) -> Self {
        Self {
            num_flag,
            index_flag: (0..num_flag).map(|_| AtomicBool::new(false)).collect(),
            num_index: AtomicUsize::new(0),
            continuous_idx: AtomicUsize::new(0),
        }
    }

    fn check_continuous(&self) {
        let mut continuous_idx = self.continuous_idx.load(Ordering::Acquire);
        while continuous_idx < self.num_flag &&
            self.index_flag[continuous_idx].load(Ordering::Acquire)
        {
            if self
                .continuous_idx
                .compare_exchange(
                    continuous_idx,
                    continuous_idx + 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                break;
            }
            continuous_idx = self.continuous_idx.load(Ordering::Acquire);
        }
    }

    pub(crate) fn add(&self, index: usize) {
        if !self.index_flag[index].swap(true, Ordering::Release) {
            self.num_index.fetch_add(1, Ordering::Release);
            self.check_continuous();
        }
    }

    pub(crate) fn continuous_idx(&self) -> usize {
        if self.num_index.load(Ordering::Acquire) >= self.num_flag &&
            self.continuous_idx.load(Ordering::Acquire) < self.num_flag
        {
            self.check_continuous();
        }
        self.continuous_idx.load(Ordering::Acquire)
    }
}

use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug)]
pub(crate) struct ContinuousDetectSet {
    num_flag: usize,
    index_flag: Vec<bool>,
    num_index: AtomicUsize,
    continuous_idx: AtomicUsize,
}

impl ContinuousDetectSet {
    pub(crate) fn new(num_flag: usize) -> Self {
        Self {
            num_flag,
            index_flag: vec![false; num_flag],
            num_index: AtomicUsize::new(0),
            continuous_idx: AtomicUsize::new(0),
        }
    }

    fn check_continuous(&self) {
        let mut continuous_idx = self.continuous_idx.load(Ordering::Relaxed);
        while continuous_idx < self.num_flag && self.index_flag[continuous_idx] {
            if self
                .continuous_idx
                .compare_exchange(
                    continuous_idx,
                    continuous_idx + 1,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_err()
            {
                break;
            }
            continuous_idx = self.continuous_idx.load(Ordering::Relaxed);
        }
    }

    pub(crate) fn add(&self, index: usize) {
        if !self.index_flag[index] {
            #[allow(invalid_reference_casting)]
            let index_flag =
                unsafe { &mut *(&self.index_flag as *const Vec<bool> as *mut Vec<bool>) };
            index_flag[index] = true;
            self.num_index.fetch_add(1, Ordering::Relaxed);
            self.check_continuous();
        }
    }

    pub(crate) fn continuous_idx(&self) -> usize {
        if self.num_index.load(Ordering::Relaxed) >= self.num_flag &&
            self.continuous_idx.load(Ordering::Relaxed) < self.num_flag
        {
            self.check_continuous();
        }
        self.continuous_idx.load(Ordering::Relaxed)
    }
}

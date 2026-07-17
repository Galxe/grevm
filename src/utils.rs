use rayon::prelude::{IntoParallelIterator, ParallelIterator};
use std::{
    cmp::min,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    thread,
};

/// Divide a range into disjoint partitions and process them through Rayon's worker pool.
pub fn fork_join_util<'scope, F>(num_elements: usize, num_partitions: Option<usize>, f: F)
where
    F: Fn(usize, usize, usize) + Send + Sync + 'scope,
{
    if num_elements == 0 {
        return;
    }
    let partitions = num_partitions
        .unwrap_or_else(|| thread::available_parallelism().map_or(8, |value| value.get()));
    assert!(partitions > 0, "fork-join partition count must be greater than zero");
    let remaining = num_elements % partitions;
    let chunk_size = num_elements / partitions;
    (0..partitions).into_par_iter().for_each(|index| {
        let start = chunk_size * index + min(index, remaining);
        let end = start + chunk_size + usize::from(index < remaining);
        f(start, end, index);
    });
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn fork_join_covers_each_index_exactly_once() {
        let visits = Mutex::new(vec![0usize; 17]);
        fork_join_util(17, Some(4), |start, end, _| {
            let mut visits = visits.lock().unwrap();
            for visit in &mut visits[start..end] {
                *visit += 1;
            }
        });
        assert!(visits.into_inner().unwrap().into_iter().all(|count| count == 1));
    }

    #[test]
    fn fork_join_accepts_empty_ranges() {
        fork_join_util(0, Some(0), |_, _, _| unreachable!());
    }
}

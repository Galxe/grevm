use rayon::prelude::{IntoParallelIterator, ParallelIterator};
use std::{cmp::min, thread};

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

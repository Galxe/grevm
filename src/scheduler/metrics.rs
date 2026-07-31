//! Per-block execution metrics.
//!
//! The collector accumulates scheduler events during one execution lifecycle and reports them when
//! that lifecycle ends. Parallel runs record `execution_time` when the finality loop exits;
//! `commit_time` sums ordered-commit calls, and `total_time` covers the complete lifecycle,
//! including sequential recovery.

use metrics_derive::Metrics;
#[cfg(feature = "test-utils")]
use std::collections::BTreeMap;
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

macro_rules! record_execute_metric {
    ($metrics:ident, $collector:expr, $field:ident) => {
        $metrics.$field.record(($collector).$field.load(Ordering::Relaxed) as f64);
    };
    ($metrics:ident, $collector:expr, $field:ident, skip_zero) => {{
        let value = ($collector).$field.load(Ordering::Relaxed);
        if value > 0 {
            $metrics.$field.record(value as f64);
        }
    }};
}

macro_rules! define_execute_metrics {
    (
        $(
            $(#[$field_meta:meta])*
            $field:ident $(=> $policy:ident)?;
        )+
    ) => {
        #[derive(Metrics)]
        #[metrics(scope = "grevm")]
        struct ExecuteMetrics {
            $(
                $(#[$field_meta])*
                $field: metrics::Histogram,
            )+
        }

        #[derive(Default)]
        pub(super) struct ExecuteMetricsCollector {
            $(
                $(#[$field_meta])*
                $field: AtomicUsize,
            )+
            #[cfg(test)]
            report_count: AtomicUsize,
        }

        impl ExecuteMetricsCollector {
            pub(super) fn report(&self) {
                #[cfg(test)]
                self.report_count.fetch_add(1, Ordering::Relaxed);
                let metrics = ExecuteMetrics::default();
                $(
                    record_execute_metric!(metrics, self, $field $(, $policy)?);
                )+
            }

            #[cfg(feature = "test-utils")]
            fn snapshot(&self) -> BTreeMap<&'static str, usize> {
                [
                    $(
                        (
                            concat!("grevm.", stringify!($field)),
                            self.$field.load(Ordering::Relaxed),
                        ),
                    )+
                ]
                .into_iter()
                .collect()
            }

            #[cfg(all(test, feature = "test-utils"))]
            pub(super) fn report_count(&self) -> usize {
                self.report_count.load(Ordering::Relaxed)
            }
        }
    };
}

define_execute_metrics! {
    /// Total transactions.
    total_tx_cnt;
    /// Conflict incarnations.
    conflict_cnt;
    /// Validation incarnations.
    validation_cnt;
    /// Execution incarnations.
    execution_cnt;
    /// Validation cursor resets.
    reset_validation_idx_cnt;
    /// Dependency updates without work.
    useless_dependent_update;
    /// Beneficiary history reads blocked by an unresolved preceding writer.
    ///
    /// The `miner` name is retained for metric-schema compatibility.
    conflict_by_miner;
    /// EVM error conflicts.
    conflict_by_error;
    /// Estimate read conflicts.
    conflict_by_estimate;
    /// Version conflicts.
    conflict_by_version;
    /// Transactions completed in one dependency attempt.
    one_attempt_with_dependency;
    /// Transactions needing more than two dependency attempts.
    more_attempts_with_dependency;
    /// Transactions without dependencies.
    no_dependency_txs;
    /// Transactions with at least one conflict.
    conflict_txs;
    /// Parallel execution time in nanoseconds.
    execution_time => skip_zero;
    /// Commit time in nanoseconds.
    commit_time;
    /// Total time in nanoseconds.
    total_time;
}

impl ExecuteMetricsCollector {
    pub(super) fn dependency_distance_histogram(&self) -> metrics::Histogram {
        metrics::histogram!("grevm.dependency_distance")
    }

    #[inline]
    pub(super) fn record_block_start(&self, total_txs: usize) {
        self.total_tx_cnt.store(total_txs, Ordering::Relaxed);
    }

    #[inline]
    pub(super) fn record_execution_attempt(&self) {
        self.execution_cnt.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub(super) fn record_validation_attempt(&self) {
        self.validation_cnt.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub(super) fn record_estimate_conflict(&self) {
        self.conflict_cnt.fetch_add(1, Ordering::Relaxed);
        self.conflict_by_estimate.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub(super) fn record_beneficiary_conflict(&self) {
        self.conflict_cnt.fetch_add(1, Ordering::Relaxed);
        self.conflict_by_miner.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub(super) fn record_evm_error_conflict(&self) {
        self.conflict_cnt.fetch_add(1, Ordering::Relaxed);
        self.conflict_by_error.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub(super) fn record_version_conflict(&self) {
        self.conflict_cnt.fetch_add(1, Ordering::Relaxed);
        self.conflict_by_version.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub(super) fn record_finalized(&self, incarnation: usize, has_dependency: bool) {
        if incarnation > 1 {
            self.conflict_txs.fetch_add(1, Ordering::Relaxed);
        }
        if has_dependency {
            if incarnation == 1 {
                self.one_attempt_with_dependency.fetch_add(1, Ordering::Relaxed);
            } else if incarnation > 2 {
                self.more_attempts_with_dependency.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            self.no_dependency_txs.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[inline]
    pub(super) fn record_useless_dependency_update(&self) {
        self.useless_dependent_update.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub(super) fn record_commit_time(&self, elapsed: Duration) {
        self.commit_time.fetch_add(elapsed.as_nanos() as usize, Ordering::Relaxed);
    }

    #[inline]
    pub(super) fn record_execution_time(&self, elapsed: Duration) {
        self.execution_time.store(elapsed.as_nanos() as usize, Ordering::Relaxed);
    }

    #[inline]
    pub(super) fn record_validation_resets(&self, resets: usize) {
        self.reset_validation_idx_cnt.store(resets, Ordering::Relaxed);
    }

    #[inline]
    pub(super) fn record_total_time(&self, elapsed: Duration) {
        self.total_time.store(elapsed.as_nanos() as usize, Ordering::Relaxed);
    }
}

#[cfg(feature = "test-utils")]
impl<DB: revm::DatabaseRef> super::Scheduler<DB> {
    /// Return this scheduler's per-execution metrics without going through a global recorder.
    pub fn metrics_snapshot(&self) -> BTreeMap<&'static str, usize> {
        self.metrics.snapshot()
    }
}

#[cfg(all(test, feature = "test-utils"))]
mod tests {
    use super::*;
    use metrics_util::debugging::DebuggingRecorder;
    use std::collections::BTreeSet;

    #[test]
    fn snapshot_schema_matches_reported_metrics() {
        let collector = ExecuteMetricsCollector::default();
        collector.record_block_start(7);
        // `execution_time` intentionally skips zero values in `report`.
        collector.record_execution_time(Duration::from_nanos(1));

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || collector.report());

        let reported_names = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .map(|(key, _, _, _)| key.key().name().to_owned())
            .collect::<BTreeSet<_>>();
        let snapshot = collector.snapshot();
        let snapshot_names =
            snapshot.keys().map(|name| (*name).to_owned()).collect::<BTreeSet<_>>();

        assert_eq!(reported_names, snapshot_names);
        assert_eq!(snapshot["grevm.total_tx_cnt"], 7);
    }

    #[test]
    fn conflict_methods_update_total_and_exactly_one_cause() {
        let collector = ExecuteMetricsCollector::default();
        collector.record_estimate_conflict();
        collector.record_beneficiary_conflict();
        collector.record_evm_error_conflict();
        collector.record_version_conflict();

        let snapshot = collector.snapshot();
        assert_eq!(snapshot["grevm.conflict_cnt"], 4);
        assert_eq!(snapshot["grevm.conflict_by_miner"], 1);
        assert_eq!(snapshot["grevm.conflict_by_estimate"], 1);
        assert_eq!(snapshot["grevm.conflict_by_error"], 1);
        assert_eq!(snapshot["grevm.conflict_by_version"], 1);
    }

    #[test]
    fn timing_methods_record_nanoseconds() {
        let collector = ExecuteMetricsCollector::default();
        collector.record_commit_time(Duration::from_nanos(3));
        collector.record_execution_time(Duration::from_nanos(5));
        collector.record_total_time(Duration::from_nanos(7));

        let snapshot = collector.snapshot();
        assert_eq!(snapshot["grevm.commit_time"], 3);
        assert_eq!(snapshot["grevm.execution_time"], 5);
        assert_eq!(snapshot["grevm.total_time"], 7);
    }
}

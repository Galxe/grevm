//! Lightweight dependency scheduling for speculative transactions.
//!
//! Edges only decide when work is retried and may intentionally omit conflicts. Block-STM
//! read-set validation, not this graph, is the correctness boundary.

use crate::{TxId, scheduler::PublishedCursorReader};
use ahash::AHashSet as HashSet;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

struct DependentState {
    onboard: bool,
    dependency: Option<TxId>,
}

impl Default for DependentState {
    fn default() -> Self {
        Self { onboard: true, dependency: None }
    }
}

pub(crate) struct TxDependency {
    num_txs: usize,
    // `onboard` means the transaction may be claimed; `dependency` blocks that claim until its
    // predecessor clears the reverse edge. Cursor claims remain advisory.
    dependent_state: Vec<Mutex<DependentState>>,
    // Reverse edges keyed by predecessor.
    affect_txs: Vec<Mutex<HashSet<TxId>>>,
    index: AtomicUsize,
}

impl TxDependency {
    pub(crate) fn new(num_txs: usize) -> Self {
        Self {
            num_txs,
            dependent_state: (0..num_txs).map(|_| Default::default()).collect(),
            affect_txs: (0..num_txs).map(|_| Default::default()).collect(),
            index: AtomicUsize::new(0),
        }
    }

    pub(crate) fn next(&self) -> Option<TxId> {
        if self.index.load(Ordering::Relaxed) >= self.num_txs {
            return None;
        }
        let index = self.index.fetch_add(1, Ordering::Relaxed);
        if index >= self.num_txs {
            return None;
        }
        let mut state = self.dependent_state[index].lock();
        if state.onboard && state.dependency.is_none() {
            state.onboard = false;
            return Some(index)
        }
        None
    }

    pub(crate) fn index(&self) -> usize {
        self.index.load(Ordering::Relaxed)
    }

    /// Clear transactions waiting on `txid`.
    ///
    /// When requested, the immediate successor can be handed directly to the same worker after
    /// its cursor position has already been passed; all other released work rewinds the cursor.
    pub(crate) fn remove(&self, txid: TxId, pop_next: bool) -> Option<TxId> {
        let mut next = None;
        let mut affects = self.affect_txs[txid].lock();
        if affects.is_empty() {
            return next;
        }
        for &tx in affects.iter() {
            let mut dependent = self.dependent_state[tx].lock();
            if dependent.dependency == Some(txid) {
                dependent.dependency = None;
                if dependent.onboard {
                    if pop_next && tx == txid + 1 && self.index.load(Ordering::Relaxed) > tx {
                        dependent.onboard = false;
                        next = Some(tx);
                    } else {
                        self.index.fetch_min(tx, Ordering::Relaxed);
                    }
                }
            }
        }
        affects.clear();
        next
    }

    /// Clear the immediate successor's blocker after publishing this committed boundary.
    pub(crate) fn commit(&self, txid: TxId) {
        let next = txid + 1;
        if next < self.num_txs {
            let mut state = self.dependent_state[next].lock();
            if state.onboard {
                state.dependency = None;
                self.index.fetch_min(next, Ordering::Relaxed);
            }
        }
    }

    /// Hold `txid` behind its own ordered-commit boundary.
    ///
    /// This is used after an EVM error or a read that requires committed origin state. Once the
    /// committed prefix reaches `txid`, no barrier is installed and the cursor is rewound
    /// immediately; otherwise committing `txid - 1` releases it through [`Self::commit`].
    pub(crate) fn key_tx(&self, txid: TxId, commit_idx: PublishedCursorReader<'_>) {
        let mut state = self.dependent_state[txid].lock();
        if txid > commit_idx.get() {
            state.dependency = Some(txid);
        }
        if !state.onboard {
            state.onboard = true;
        }
        if state.dependency.is_none() {
            self.index.fetch_min(txid, Ordering::Relaxed);
        }
    }

    /// Add one scheduling predecessor, or make `txid` eligible when no predecessor is needed.
    ///
    /// The caller normally supplies only the latest conflicting predecessor; validation covers
    /// dependencies not represented here. Replacing a blocker may leave a stale reverse edge;
    /// [`Self::remove`] rechecks the current blocker before releasing work.
    ///
    /// `dep_id` must be a strict predecessor of `txid`. Besides matching transaction order, this
    /// preserves the global lock order used below.
    pub(crate) fn add(&self, txid: TxId, dep_id: Option<TxId>) {
        if let Some(dep_id) = dep_id {
            assert!(
                dep_id < txid,
                "dependency transaction {dep_id} must precede dependent transaction {txid}",
            );
            let mut dep = self.affect_txs[dep_id].lock();
            let mut dep_state = self.dependent_state[dep_id].lock();
            let mut state = self.dependent_state[txid].lock();
            state.dependency = Some(dep_id);
            if !state.onboard {
                state.onboard = true;
            }

            dep.insert(txid);
            if !dep_state.onboard {
                dep_state.onboard = true;
            }
            if dep_state.dependency.is_none() {
                self.index.fetch_min(dep_id, Ordering::Relaxed);
            }
        } else {
            let mut state = self.dependent_state[txid].lock();
            if !state.onboard {
                state.onboard = true;
                state.dependency = None;
                self.index.fetch_min(txid, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacing_a_blocker_ignores_the_stale_reverse_edge() {
        let dependencies = TxDependency::new(3);
        assert_eq!(dependencies.next(), Some(0));
        assert_eq!(dependencies.next(), Some(1));
        assert_eq!(dependencies.next(), Some(2));

        dependencies.add(2, Some(0));
        dependencies.add(2, Some(1));

        assert_eq!(dependencies.next(), Some(0));
        dependencies.remove(0, false);
        assert_eq!(dependencies.next(), Some(1));
        assert_eq!(dependencies.next(), None, "transaction 2 must still wait for transaction 1");

        dependencies.remove(1, false);
        assert_eq!(dependencies.next(), Some(2));
        assert_eq!(dependencies.next(), None);
    }

    #[test]
    fn direct_handoff_is_not_claimed_again_by_the_cursor() {
        let dependencies = TxDependency::new(2);
        assert_eq!(dependencies.next(), Some(0));
        assert_eq!(dependencies.next(), Some(1));

        dependencies.add(1, Some(0));
        assert_eq!(dependencies.next(), Some(0));
        assert_eq!(dependencies.next(), None);

        assert_eq!(dependencies.remove(0, true), Some(1));
        assert_eq!(dependencies.next(), None);
    }

    #[test]
    fn committed_prefix_barrier_is_released_in_either_order() {
        fn claimed_pair() -> TxDependency {
            let dependencies = TxDependency::new(2);
            assert_eq!(dependencies.next(), Some(0));
            assert_eq!(dependencies.next(), Some(1));
            dependencies
        }

        let commit_cursor = AtomicUsize::new(0);
        let dependencies = claimed_pair();
        dependencies.key_tx(1, PublishedCursorReader::new(&commit_cursor));
        assert_eq!(dependencies.next(), None);
        commit_cursor.store(1, Ordering::Release);
        dependencies.commit(0);
        assert_eq!(dependencies.next(), Some(1));

        let commit_cursor = AtomicUsize::new(1);
        let dependencies = claimed_pair();
        dependencies.commit(0);
        dependencies.key_tx(1, PublishedCursorReader::new(&commit_cursor));
        assert_eq!(dependencies.next(), Some(1));
    }

    #[test]
    fn concurrent_commit_and_barrier_installation_do_not_lose_the_retry() {
        for _ in 0..100 {
            let dependencies = TxDependency::new(2);
            assert_eq!(dependencies.next(), Some(0));
            assert_eq!(dependencies.next(), Some(1));
            let commit_cursor = AtomicUsize::new(0);

            std::thread::scope(|scope| {
                scope.spawn(|| {
                    commit_cursor.store(1, Ordering::Release);
                    dependencies.commit(0);
                });
                scope.spawn(|| {
                    dependencies.key_tx(1, PublishedCursorReader::new(&commit_cursor));
                });
            });

            assert_eq!(dependencies.next(), Some(1));
            assert_eq!(dependencies.next(), None);
        }
    }

    #[test]
    #[should_panic(expected = "must precede")]
    fn dependency_must_be_a_strict_predecessor() {
        TxDependency::new(1).add(0, Some(0));
    }
}

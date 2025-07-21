use crate::TxId;
use ahash::AHashSet as HashSet;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Clone)]
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
    dependent_state: Vec<Mutex<DependentState>>,
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

    pub(crate) fn create(dependent_tx: Vec<Option<TxId>>, affect_txs: Vec<HashSet<TxId>>) -> Self {
        assert_eq!(dependent_tx.len(), affect_txs.len());
        let num_txs = dependent_tx.len();
        Self {
            num_txs,
            dependent_state: dependent_tx
                .into_iter()
                .map(|dep| Mutex::new(DependentState { onboard: true, dependency: dep }))
                .collect(),
            affect_txs: affect_txs.into_iter().map(|affects| Mutex::new(affects)).collect(),
            index: AtomicUsize::new(0),
        }
    }

    pub(crate) fn next(&self) -> Option<TxId> {
        if self.index.load(Ordering::Relaxed) < self.num_txs {
            let index = self.index.fetch_add(1, Ordering::Relaxed);
            if index < self.num_txs {
                let mut state = self.dependent_state[index].lock();
                if state.onboard && state.dependency.is_none() {
                    state.onboard = false;
                    return Some(index)
                }
            }
        }
        None
    }

    pub(crate) fn index(&self) -> usize {
        self.index.load(Ordering::Relaxed)
    }

    /// The benchmark `bench_dependency_distance` tests how transaction dependency distance affects
    /// actual conflicts. When dependency_distance ≤ 4, transactions exhibit significantly higher
    /// conflict probability, with the most pronounced effect occurring at distance = 1.
    /// Accordingly, Grevm specifically checks dependencies with distance = 1 when updating the DAG.
    /// This optimization balances conflict detection accuracy with computational efficiency.
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

    /// When transactions fail due to EVM execution errors or access incorrect miner/self-destructed
    /// states, Grevm assigns them a special self-referential dependency (where dependency equals
    /// the transaction's own ID). This dependency is only cleared when commit_idx matches txid,
    /// ensuring the transaction can only proceed after obtaining the correct account state. This
    /// mechanism guarantees state consistency while maintaining parallel execution capabilities.
    pub(crate) fn key_tx(&self, txid: TxId, commit_idx: &AtomicUsize) {
        let mut state = self.dependent_state[txid].lock();
        if txid > commit_idx.load(Ordering::Acquire) {
            state.dependency = Some(txid);
        }
        if !state.onboard {
            state.onboard = true;
        }
        if state.dependency.is_none() {
            self.index.fetch_min(txid, Ordering::Relaxed);
        }
    }

    pub(crate) fn add(&self, txid: TxId, dep_id: Option<TxId>) {
        if let Some(dep_id) = dep_id {
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

    pub(crate) fn print(&self) {
        let dependent_tx: Vec<(TxId, DependentState)> =
            self.dependent_state.iter().map(|dep| dep.lock().clone()).enumerate().collect();
        let affect_txs: Vec<(TxId, HashSet<TxId>)> =
            self.affect_txs.iter().map(|affects| affects.lock().clone()).enumerate().collect();
        println!("tx_states: {:?}", dependent_tx);
        println!("affect_txs: {:?}", affect_txs);
    }
}

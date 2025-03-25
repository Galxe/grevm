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

pub struct TxDependency {
    num_txs: usize,
    dependent_state: Vec<Mutex<DependentState>>,
    affect_txs: Vec<Mutex<HashSet<TxId>>>,
    num_onboard: AtomicUsize,
    index: AtomicUsize,
}

impl TxDependency {
    pub fn new(num_txs: usize) -> Self {
        Self {
            num_txs,
            dependent_state: (0..num_txs).map(|_| Default::default()).collect(),
            affect_txs: (0..num_txs).map(|_| Default::default()).collect(),
            num_onboard: AtomicUsize::new(num_txs),
            index: AtomicUsize::new(0),
        }
    }

    pub fn create(dependent_tx: Vec<Option<TxId>>, affect_txs: Vec<HashSet<TxId>>) -> Self {
        assert_eq!(dependent_tx.len(), affect_txs.len());
        let num_txs = dependent_tx.len();
        Self {
            num_txs,
            dependent_state: dependent_tx
                .into_iter()
                .map(|dep| Mutex::new(DependentState { onboard: true, dependency: dep }))
                .collect(),
            affect_txs: affect_txs.into_iter().map(|affects| Mutex::new(affects)).collect(),
            num_onboard: AtomicUsize::new(num_txs),
            index: AtomicUsize::new(0),
        }
    }

    pub fn next(&self) -> Option<TxId> {
        if self.index.load(Ordering::Relaxed) < self.num_txs {
            let index = self.index.fetch_add(1, Ordering::Relaxed);
            if index < self.num_txs {
                let mut state = self.dependent_state[index].lock();
                if state.onboard && state.dependency.is_none() {
                    state.onboard = false;
                    self.num_onboard.fetch_sub(1, Ordering::Relaxed);
                    return Some(index)
                }
            }
        }
        None
    }

    pub fn index(&self) -> usize {
        self.index.load(Ordering::Relaxed)
    }

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
                        self.num_onboard.fetch_sub(1, Ordering::Relaxed);
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

    pub fn add(&self, txid: TxId, dep_id: Option<TxId>) {
        if let Some(dep_id) = dep_id {
            let mut dep = self.affect_txs[dep_id].lock();
            let mut dep_state = self.dependent_state[dep_id].lock();
            let mut state = self.dependent_state[txid].lock();
            state.dependency = Some(dep_id);
            if !state.onboard {
                state.onboard = true;
                self.num_onboard.fetch_add(1, Ordering::Relaxed);
            }

            dep.insert(txid);
            if !dep_state.onboard {
                dep_state.onboard = true;
                self.num_onboard.fetch_add(1, Ordering::Relaxed);
            }
            if dep_state.dependency.is_none() {
                self.index.fetch_min(dep_id, Ordering::Relaxed);
            }
        } else {
            let mut state = self.dependent_state[txid].lock();
            if !state.onboard {
                state.onboard = true;
                self.index.fetch_min(txid, Ordering::Relaxed);
            }
            if state.dependency.is_none() {
                self.num_onboard.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn print(&self) {
        let dependent_tx: Vec<(TxId, DependentState)> =
            self.dependent_state.iter().map(|dep| dep.lock().clone()).enumerate().collect();
        let affect_txs: Vec<(TxId, HashSet<TxId>)> =
            self.affect_txs.iter().map(|affects| affects.lock().clone()).enumerate().collect();
        println!("tx_states: {:?}", dependent_tx);
        println!("affect_txs: {:?}", affect_txs);
    }
}

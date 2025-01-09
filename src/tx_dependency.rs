use crate::TxId;
use ahash::AHashSet as HashSet;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

static LONG_DEPENDENCY: usize = 4;

pub struct TxDependency {
    dependent_cnt: Vec<AtomicUsize>,
    affect_txs: Vec<Mutex<HashSet<TxId>>>,
}

impl TxDependency {
    pub fn new(num_txs: usize) -> Self {
        Self {
            dependent_cnt: (0..num_txs).map(|_| AtomicUsize::new(0)).collect(),
            affect_txs: (0..num_txs).map(|_| Default::default()).collect(),
        }
    }

    pub fn create(dependent_cnt: Vec<AtomicUsize>, affect_txs: Vec<Mutex<HashSet<TxId>>>) -> Self {
        assert_eq!(dependent_cnt.len(), affect_txs.len());
        Self { dependent_cnt, affect_txs }
    }

    pub fn is_read(&self, txid: TxId) -> bool {
        self.dependent_cnt[txid].load(Ordering::Acquire) == 0
    }

    pub fn remove(&self, txid: TxId) -> Vec<TxId> {
        let mut ready_txs = vec![];
        let mut txs = self.affect_txs[txid].lock();
        if !txs.is_empty() {
            for &affect_id in txs.iter() {
                let dependent_cnt = self.dependent_cnt[affect_id].fetch_sub(1, Ordering::AcqRel);
                assert!(dependent_cnt > 0);
                if dependent_cnt == 1 {
                    ready_txs.push(affect_id);
                }
            }
            txs.clear();
        }
        ready_txs
    }

    pub fn add(&self, txid: TxId, blocking_tx: TxId) {
        let mut affect = self.affect_txs[blocking_tx].lock();
        assert!(affect.insert(txid));
        self.dependent_cnt[txid].fetch_add(1, Ordering::AcqRel);
    }

    pub fn print(&self) {
        let dependent_cnt: Vec<(TxId, usize)> =
            self.dependent_cnt.iter().map(|cnt| cnt.load(Ordering::Acquire)).enumerate().collect();
        let affect_txs: Vec<(TxId, HashSet<TxId>)> =
            self.affect_txs.iter().map(|txs| (*txs.lock()).clone()).enumerate().collect();
        println!("dependent_cnt: {:?}", dependent_cnt);
        println!("affect_txs: {:?}", affect_txs);
    }
}

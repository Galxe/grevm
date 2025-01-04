use crate::{utils::OrderedSet, TxId};
use ahash::AHashSet as HashSet;

pub struct TxDependency {
    dependent_tx: Vec<Option<TxId>>,
    affect_txs: Vec<HashSet<TxId>>,
    no_dep_txs: OrderedSet,
}

impl TxDependency {
    pub fn new(num_txs: usize) -> Self {
        Self {
            dependent_tx: vec![None; num_txs],
            affect_txs: vec![HashSet::new(); num_txs],
            no_dep_txs: OrderedSet::new(num_txs, true),
        }
    }

    pub fn create(
        dependent_tx: Vec<Option<TxId>>,
        affect_txs: Vec<HashSet<TxId>>,
        no_dep_txs: OrderedSet,
    ) -> Self {
        Self { dependent_tx, affect_txs, no_dep_txs }
    }

    pub fn next(&mut self, num: usize) -> Vec<TxId> {
        let mut txs = Vec::with_capacity(num);
        for _ in 0..num {
            if let Some(txid) = self.no_dep_txs.pop_first() {
                txs.push(txid);
            } else {
                break;
            }
        }
        txs
    }

    pub fn remove(&mut self, txid: TxId) {
        let affect_txs = std::mem::take(&mut self.affect_txs[txid]);
        for affect_tx in affect_txs {
            if self.dependent_tx[affect_tx] == Some(txid) {
                self.dependent_tx[affect_tx] = None;
                self.no_dep_txs.insert(affect_tx);
            }
        }
    }

    pub fn add(&mut self, txid: TxId, dep_id: Option<TxId>) {
        if let Some(dep_id) = dep_id {
            assert!(dep_id < txid);
            if self.dependent_tx[txid].is_none() || dep_id > self.dependent_tx[txid].unwrap() {
                self.dependent_tx[txid] = Some(dep_id);
                self.affect_txs[dep_id].insert(txid);
                if self.dependent_tx[dep_id].is_none() {
                    self.no_dep_txs.insert(dep_id);
                }
            }
        } else {
            assert!(self.dependent_tx[txid].is_none());
            self.no_dep_txs.insert(txid);
        }
    }

    pub fn print(&self) {
        println!("no_dep_txs: {:?}", self.no_dep_txs.to_set());
        let dependent_tx: Vec<(TxId, Option<TxId>)> =
            self.dependent_tx.clone().into_iter().enumerate().collect();
        let affect_txs: Vec<(TxId, HashSet<TxId>)> =
            self.affect_txs.clone().into_iter().enumerate().collect();
        println!("dependent_tx: {:?}", dependent_tx);
        println!("affect_txs: {:?}", affect_txs);
    }
}

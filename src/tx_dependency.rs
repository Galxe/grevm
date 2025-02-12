use crate::TxId;
use ahash::AHashSet as HashSet;
use std::collections::BTreeSet;

pub struct TxDependency {
    dependent_tx: Vec<Option<TxId>>,
    affect_txs: Vec<HashSet<TxId>>,
    key_tx: Vec<bool>,
    update_cnt: Vec<usize>,
    no_dep_txs: BTreeSet<TxId>,
}

impl TxDependency {
    pub fn new(num_txs: usize) -> Self {
        Self {
            dependent_tx: vec![None; num_txs],
            affect_txs: vec![HashSet::new(); num_txs],
            key_tx: vec![false; num_txs],
            update_cnt: vec![0; num_txs],
            no_dep_txs: (0..num_txs).collect(),
        }
    }

    pub fn create(
        dependent_tx: Vec<Option<TxId>>,
        affect_txs: Vec<HashSet<TxId>>,
        no_dep_txs: BTreeSet<TxId>,
    ) -> Self {
        assert_eq!(dependent_tx.len(), affect_txs.len());
        let num_txs = dependent_tx.len();
        Self {
            dependent_tx,
            affect_txs,
            key_tx: vec![false; num_txs],
            update_cnt: vec![0; num_txs],
            no_dep_txs,
        }
    }

    pub fn next(&mut self, finality_idx: TxId) -> Vec<TxId> {
        let mut txs = vec![];
        if let Some(txid) = self.no_dep_txs.pop_first() {
            let mut continuous = txid;
            txs.push(continuous);
            if continuous == finality_idx {
                while !self.affect_txs[continuous].is_empty() {
                    let next = continuous + 1;
                    if self.affect_txs[continuous].remove(&next) {
                        if self.dependent_tx[next] == Some(continuous) {
                            self.dependent_tx[next] = None;
                            txs.push(next);
                            continuous = next;
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                }
            }
        }
        txs
    }

    pub fn commit(&mut self, txid: TxId) {
        if !self.affect_txs[txid].is_empty() {
            let affect_txs = std::mem::take(&mut self.affect_txs[txid]);
            for affect_tx in affect_txs {
                if self.dependent_tx[affect_tx] == Some(txid) {
                    self.dependent_tx[affect_tx] = None;
                    self.no_dep_txs.insert(affect_tx);
                }
            }
        }
    }

    pub fn remove(&mut self, txid: TxId) {
        if !self.key_tx[txid] {
            self.commit(txid);
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
            self.key_tx[self.dependent_tx[txid].unwrap()] = true;
        }
        if self.dependent_tx[txid].is_none() {
            self.no_dep_txs.insert(txid);
        } else {
            self.no_dep_txs.remove(&txid);
        }
    }

    pub fn print(&self) {
        println!("no_dep_txs: {:?}", self.no_dep_txs);
        let dependent_tx: Vec<(TxId, Option<TxId>)> =
            self.dependent_tx.clone().into_iter().enumerate().collect();
        let affect_txs: Vec<(TxId, HashSet<TxId>)> =
            self.affect_txs.clone().into_iter().enumerate().collect();
        println!("dependent_tx: {:?}", dependent_tx);
        println!("affect_txs: {:?}", affect_txs);
    }
}

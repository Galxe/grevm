use crate::{fork_join_util, LocationAndType, SharedTxStates, TxId, DEBUG_BOTTLENECK};
use smallvec::SmallVec;
use std::{
    cmp::{max, min, Reverse},
    collections::{BTreeMap, BTreeSet, BinaryHeap, VecDeque},
};
use std::fs::File;
use std::io::{BufWriter, Write};

pub(crate) type DependentTxsVec = SmallVec<[TxId; 1]>;

use ahash::{AHashMap as HashMap, AHashSet as HashSet};
use metrics::{counter, histogram};
use tracing::info;

const RAW_TRANSFER_WEIGHT: usize = 1;

/// TxDependency is used to store the dependency relationship between transactions.
/// The dependency relationship is stored in the form of a directed graph by adjacency list.
pub(crate) struct TxDependency {
    // if txi <- txj, then tx_dependency[txj - num_finality_txs].push(txi)
    tx_dependency: Vec<DependentTxsVec>,
    // when a tx is in finality state, we don't need to store their dependencies
    num_finality_txs: usize,

    // After one round of transaction execution,
    // the running time can be obtained, which can make the next round of partitioning more
    // balanced.
    #[allow(dead_code)]
    tx_running_time: Option<Vec<u64>>,

    // Partitioning is balanced based on weights.
    // In the first round, weights can be assigned based on transaction type and called contract
    // type, while in the second round, weights can be assigned based on tx_running_time.
    #[allow(dead_code)]
    tx_weight: Option<Vec<usize>>,

    pub round: Option<usize>,
    pub block_height: u64,
    tx_states: SharedTxStates,
}

impl TxDependency {
    pub(crate) fn new(num_txs: usize) -> Self {
        TxDependency {
            tx_dependency: vec![DependentTxsVec::new(); num_txs],
            num_finality_txs: 0,
            tx_running_time: None,
            tx_weight: None,
            round: None,
            block_height: 0,
            tx_states: Default::default(),
        }
    }

    #[fastrace::trace]
    pub fn init_tx_dependency(&mut self, tx_states: SharedTxStates) {
        let mut last_write_tx: HashMap<LocationAndType, TxId> = HashMap::new();
        for (txid, rw_set) in tx_states.iter().enumerate() {
            let mut dependencies = HashSet::new();
            for (location, _) in rw_set.read_set.iter() {
                if let Some(previous) = last_write_tx.get(location) {
                    dependencies.insert(*previous);
                }
            }
            self.tx_dependency[txid] = dependencies.into_iter().collect();
            for location in rw_set.write_set.iter() {
                last_write_tx.insert(location.clone(), txid);
            }
        }
        self.tx_states = tx_states;
    }

    /// Retrieve the optimal partitions based on the dependency relationships between transactions.
    /// This method uses a breadth-first traversal from back to front to group connected subgraphs,
    /// and employs a greedy algorithm to allocate these groups into different partitions.
    pub(crate) fn fetch_best_partitions(&mut self, partition_count: usize) -> Vec<Vec<TxId>> {
        let mut num_group = 0;
        let mut weighted_group: BTreeMap<usize, Vec<DependentTxsVec>> = BTreeMap::new();
        let tx_weight = self
            .tx_weight
            .take()
            .unwrap_or_else(|| vec![RAW_TRANSFER_WEIGHT; self.tx_dependency.len()]);
        let num_finality_txs = self.num_finality_txs;
        let mut txid = self.num_finality_txs + self.tx_dependency.len() - 1;

        // the revert of self.tx_dependency
        // if txi <- txj, then tx_dependency[txi - num_finality_txs].push(txj)
        let mut revert_dependency: Vec<DependentTxsVec> =
            vec![DependentTxsVec::new(); self.tx_dependency.len()];
        let mut is_related: Vec<bool> = vec![false; self.tx_dependency.len()];
        {
            let single_groups = weighted_group.entry(RAW_TRANSFER_WEIGHT).or_default();
            for index in (0..self.tx_dependency.len()).rev() {
                let txj = index + num_finality_txs;
                let txj_dep = &self.tx_dependency[index];
                if txj_dep.is_empty() {
                    if !is_related[index] {
                        let mut single_group = DependentTxsVec::new();
                        single_group.push(txj);
                        single_groups.push(single_group);
                        num_group += 1;
                    }
                } else {
                    is_related[index] = true;
                    for txi in txj_dep {
                        let txi_index = *txi - num_finality_txs;
                        revert_dependency[txi_index].push(txj);
                        is_related[txi_index] = true;
                    }
                }
            }
            if single_groups.is_empty() {
                weighted_group.remove(&RAW_TRANSFER_WEIGHT);
            }
        }
        // Because transactions only rely on transactions with lower ID,
        // we can search from the transaction with the highest ID from back to front.
        // Despite having three layers of loops, the time complexity is only o(num_txs)
        let mut breadth_queue = VecDeque::new();
        while txid >= num_finality_txs {
            let index = txid - num_finality_txs;
            if is_related[index] {
                let mut group = DependentTxsVec::new();
                let mut weight: usize = 0;
                // Traverse the breadth from back to front
                breadth_queue.clear();
                breadth_queue.push_back(index);
                is_related[index] = false;
                while let Some(top_index) = breadth_queue.pop_front() {
                    // txj -> txi, where txj = top_index + num_finality_txs
                    for top_down in self.tx_dependency[top_index]
                        .iter()
                        .chain(revert_dependency[top_index].iter())
                    // txk -> txj, where txj = top_index + num_finality_txs
                    {
                        let next_index = *top_down - num_finality_txs;
                        if is_related[next_index] {
                            breadth_queue.push_back(next_index);
                            is_related[next_index] = false;
                        }
                    }
                    weight += tx_weight[index];
                    group.push(top_index + num_finality_txs);
                }
                weighted_group.entry(weight).or_default().push(group);
                num_group += 1;
            }
            if txid == 0 {
                break;
            }
            txid -= 1;
        }
        self.skew_analyze(&weighted_group);

        let num_partitions = min(partition_count, num_group);
        if num_partitions == 0 {
            return vec![vec![]];
        }
        let mut partitioned_group: Vec<BTreeSet<TxId>> = vec![BTreeSet::new(); num_partitions];
        let mut partition_weight = BinaryHeap::new();
        // Separate processing of groups with a weight of 1
        // Because there is only one transaction in these groups,
        // processing them separately can greatly optimize performance.
        if weighted_group.len() == 1 {
            if let Some(groups) = weighted_group.remove(&RAW_TRANSFER_WEIGHT) {
                fork_join_util(groups.len(), Some(num_partitions), |start_pos, end_pos, part| {
                    #[allow(invalid_reference_casting)]
                    let partition = unsafe {
                        &mut *(&partitioned_group[part] as *const BTreeSet<TxId>
                            as *mut BTreeSet<TxId>)
                    };
                    for pos in start_pos..end_pos {
                        for txid in groups[pos].iter() {
                            partition.insert(*txid);
                        }
                    }
                });
            }
        }
        for index in 0..num_partitions {
            partition_weight
                .push(Reverse((partitioned_group[index].len() * RAW_TRANSFER_WEIGHT, index)));
        }

        for (add_weight, groups) in weighted_group.into_iter().rev() {
            for group in groups {
                if let Some(Reverse((weight, index))) = partition_weight.pop() {
                    partitioned_group[index].extend(group);
                    let new_weight = weight + add_weight;
                    partition_weight.push(Reverse((new_weight, index)));
                }
            }
        }
        partitioned_group
            .into_iter()
            .filter(|bs| !bs.is_empty())
            .map(|bs| bs.into_iter().collect())
            .collect()
    }

    /// Update the dependency relationship between transactions.
    /// After each round of transaction execution, update the dependencies between transactions.
    /// The new dependencies are used to optimize the partitioning for the next round of
    /// transactions, ensuring that conflicting transactions can read the transactions they
    /// depend on.
    pub(crate) fn update_tx_dependency(
        &mut self,
        tx_dependency: Vec<DependentTxsVec>,
        num_finality_txs: usize,
    ) {
        if (self.tx_dependency.len() + self.num_finality_txs) !=
            (tx_dependency.len() + num_finality_txs)
        {
            panic!("Different transaction number");
        }
        self.tx_dependency = tx_dependency;
        self.num_finality_txs = num_finality_txs;
    }

    fn skew_analyze(&self, weighted_group: &BTreeMap<usize, Vec<DependentTxsVec>>) {
        if !(*DEBUG_BOTTLENECK) {
            return;
        }
        let num_finality_txs = self.num_finality_txs;
        let num_txs = num_finality_txs + self.tx_dependency.len();
        let num_remaining = self.tx_dependency.len();
        if let Some(0) = self.round {
            counter!("grevm.total_block_cnt").increment(1);
        }
        let mut subgraph = BTreeSet::new();
        let mut two = false;
        if let Some((_, groups)) = weighted_group.last_key_value() {
            if self.round.is_none() {
                let largest_ratio = groups[0].len() as f64 / num_remaining as f64;
                histogram!("grevm.large_graph_ratio").record(largest_ratio);
                if groups[0].len() >= num_remaining / 2 {
                    counter!("grevm.low_parallelism_cnt").increment(1);
                    two = true;
                }
            }
            if groups[0].len() >= num_remaining / 3 {
                subgraph.extend(groups[0].clone());
            }
        }
        if num_txs < 64 || num_remaining < num_txs / 3 || subgraph.is_empty() {
            return;
        }

        let mut longest_chain: Vec<TxId> = vec![];
        // ChainLength -> ChainNumber
        let mut chains = BTreeMap::new();
        let mut visited = HashSet::new();
        for txid in subgraph.iter().rev() {
            if !visited.contains(txid) {
                let mut txid = *txid;
                let mut chain_len = 0;
                let mut curr_chain = vec![];
                while !visited.contains(&txid) {
                    chain_len += 1;
                    visited.insert(txid);
                    curr_chain.push(txid);
                    let dep: BTreeSet<TxId> =
                        self.tx_dependency[txid - num_finality_txs].clone().into_iter().collect();
                    for dep_id in dep.into_iter().rev() {
                        if !visited.contains(&dep_id) {
                            txid = dep_id;
                            break;
                        }
                    }
                }
                if curr_chain.len() > longest_chain.len() {
                    longest_chain = curr_chain;
                }
                let chain_num = chains.get(&chain_len).cloned().unwrap_or(0) + 1;
                chains.insert(chain_len, chain_num);
            }
        }

        let graph_len = subgraph.len();
        let tip = self.round.map(|r| format!("round{}", r)).unwrap_or(String::from("none"));
        counter!("grevm.large_graph_block_cnt", "tip" => tip.clone()).increment(1);
        if let Some((chain_len, _)) = chains.last_key_value() {
            let chain_len = *chain_len;
            let chain_type = if chain_len > graph_len * 2 / 3 {
                // Long chain
                "chain"
            } else if chain_len < max(3, graph_len / 6) {
                // Star Graph
                "star"
            } else {
                // Fork Graph
                "fork"
            };
            counter!("grevm.large_graph", "type" => chain_type, "tip" => tip.clone()).increment(1);
            if self.round.is_none() {
                info!("Block({}) has large subgraph, type={}", self.block_height, chain_type);
                if two && chain_type == "chain" {
                    self.draw_dag(weighted_group, longest_chain);
                }
            }
        }
    }

    fn draw_dag(&self, weighted_group: &BTreeMap<usize, Vec<DependentTxsVec>>, longest_chain: Vec<TxId>) {
        let file = File::create(format!("dag/block{}.info", self.block_height)).unwrap();
        let mut writer = BufWriter::new(file);
        writeln!(writer, "[dag]").unwrap();

        for txid in (0..self.tx_dependency.len()).rev() {
            let deps: BTreeSet<TxId> = self.tx_dependency[txid].clone().into_iter().collect();
            for dep_id in deps.into_iter().rev() {
                writeln!(writer, "tx{}->tx{}", txid, dep_id).unwrap();
            }
        }
        writeln!(writer, "").unwrap();

        let mut locations: HashSet<LocationAndType> = HashSet::new();
        info!("Block({}) has large subgraph, longest chain: {:?}", self.block_height, longest_chain);
        for index in 0..longest_chain.len() - 1 {
            let txid = longest_chain[index];
            let dep_id = longest_chain[index + 1];
            let rs = self.tx_states[txid].read_set.clone();
            let ws = self.tx_states[dep_id].write_set.clone();
            let mut intersection = HashSet::new();
            for l in ws {
                if rs.contains_key(&l) {
                    intersection.insert(l);
                }
            }
            if locations.is_empty() {
                locations = intersection;
            } else {
                let join: HashSet<LocationAndType> = locations.intersection(&intersection).cloned().collect();
                if join.is_empty() {
                    info!("Block({}) has large subgraph, ==> tx{}: {:?}", self.block_height, txid, locations);
                }
                locations = join;
            }
        }
    }
}

mod async_commit;
mod hint;
mod scheduler;
mod storage;
mod tx_dependency;
mod utils;

use ahash::{AHashMap as HashMap, AHashSet as HashSet};
use lazy_static::lazy_static;
use rayon::prelude::{IntoParallelIterator, ParallelIterator};
use revm_primitives::{AccountInfo, Address, Bytecode, EVMResult, B256, U256};
use std::{cmp::min, thread};

lazy_static! {
    static ref CONCURRENT_LEVEL: usize =
        thread::available_parallelism().map(|n| n.get()).unwrap_or(8) * 2;
}

type TxId = usize;

#[derive(Debug, Clone, Eq, PartialEq, Default)]
enum TransactionStatus {
    #[default]
    Initial,
    Executing,
    Executed,
    Validating,
    Unconfirmed,
    Conflict,
    Finality,
}

#[derive(Debug, Default)]
struct TxState {
    pub status: TransactionStatus,
    pub incarnation: usize,
    pub has_dependency: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct TxVersion {
    pub txid: TxId,
    pub incarnation: usize,
}

impl TxVersion {
    pub fn new(txid: TxId, incarnation: usize) -> Self {
        Self { txid, incarnation }
    }
}

#[derive(Debug, PartialEq)]
enum ReadVersion {
    MvMemory(TxVersion),
    Storage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AccountBasic {
    /// The balance of the account.
    pub balance: U256,
    /// The nonce of the account.
    pub nonce: u64,
    pub code_hash: Option<B256>,
}

#[derive(Debug, Clone)]
enum MemoryValue {
    Basic(AccountInfo),
    Code(Bytecode),
    CodeHash(Bytecode),
    Storage(U256),
    SelfDestructed,
}

#[derive(Debug, Clone)]
struct MemoryEntry {
    incarnation: usize,
    data: MemoryValue,
    estimate: bool,
}

impl MemoryEntry {
    pub fn new(incarnation: usize, data: MemoryValue, estimate: bool) -> Self {
        Self { incarnation, data, estimate }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum LocationAndType {
    Basic(Address),

    Storage(Address, U256),

    Code(Address),

    CodeHash(B256),
}

struct TransactionResult<DBError> {
    pub read_set: HashMap<LocationAndType, ReadVersion>,
    pub write_set: HashSet<LocationAndType>,
    pub execute_result: EVMResult<DBError>,
}

#[derive(Clone, Debug)]
enum Task {
    Execution(TxVersion),
    Validation(TxVersion),
    ExecutionGroup(Vec<TxVersion>),
}

impl Default for Task {
    fn default() -> Self {
        Task::Execution(TxVersion::new(0, 0))
    }
}

enum AbortReason {
    EvmError,
    SelfDestructed,
}

/// Utility function for parallel execution using fork-join pattern.
///
/// This function divides the work into partitions and executes the provided closure `f`
/// in parallel across multiple threads. The number of partitions can be specified, or it
/// will default to twice the number of CPU cores plus one.
///
/// # Arguments
///
/// * `num_elements` - The total number of elements to process.
/// * `num_partitions` - Optional number of partitions to divide the work into.
/// * `f` - A closure that takes three arguments: the start index, the end index, and the partition
///   index.
///
/// # Example
///
/// ```
/// use grevm::fork_join_util;
/// fork_join_util(100, Some(4), |start, end, index| {
///     println!("Partition {}: processing elements {} to {}", index, start, end);
/// });
/// ```
pub fn fork_join_util<'scope, F>(num_elements: usize, num_partitions: Option<usize>, f: F)
where
    F: Fn(usize, usize, usize) + Send + Sync + 'scope,
{
    let parallel_cnt = num_partitions.unwrap_or(*CONCURRENT_LEVEL);
    let remaining = num_elements % parallel_cnt;
    let chunk_size = num_elements / parallel_cnt;
    (0..parallel_cnt).into_par_iter().for_each(|index| {
        let start_pos = chunk_size * index + min(index, remaining);
        let mut end_pos = start_pos + chunk_size;
        if index < remaining {
            end_pos += 1;
        }
        f(start_pos, end_pos, index);
    });
}

pub use async_commit::StateAsyncCommit;
pub use scheduler::Scheduler;
pub use storage::{ParallelBundleState, ParallelTakeBundle};
